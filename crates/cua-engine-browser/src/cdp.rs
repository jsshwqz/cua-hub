//! Minimal Chrome DevTools Protocol client.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct LaunchOptions {
    pub headed: bool,
    pub start_url: Option<String>,
    pub window_size: (u32, u32),
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            headed: false,
            start_url: None,
            window_size: (1440, 900),
        }
    }
}

pub struct BrowserSession {
    child: Child,
    profile_dir: PathBuf,
    ws: Ws,
    next_id: u64,
    pub port: u16,
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

fn find_browser() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CUA_HUB_BROWSER") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    let candidates: Vec<PathBuf> = if cfg!(windows) {
        let pf = std::env::var("ProgramFiles").unwrap_or_default();
        let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_default();
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        vec![
            format!("{pf86}\\Microsoft\\Edge\\Application\\msedge.exe").into(),
            format!("{pf}\\Google\\Chrome\\Application\\chrome.exe").into(),
            format!("{local}\\Google\\Chrome\\Application\\chrome.exe").into(),
            format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe").into(),
        ]
    } else {
        [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    };

    candidates.into_iter().find(|p| p.exists())
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

async fn wait_for_debugger(port: u16) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/json/version");
    for _ in 0..80 {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(125)).await;
    }
    bail!("browser devtools endpoint did not come up on port {port}")
}

async fn first_page_ws(port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let list: Value = reqwest::get(&url).await?.json().await?;
    list.as_array()
        .and_then(|pages| pages.iter().find(|p| p["type"] == "page"))
        .and_then(|p| p["webSocketDebuggerUrl"].as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no page target with a websocket URL"))
}

pub async fn launch(opts: LaunchOptions) -> Result<BrowserSession> {
    let binary = find_browser().ok_or_else(|| {
        anyhow!("no Chrome/Edge found; set CUA_HUB_BROWSER to the executable path")
    })?;
    let port = free_port()?;
    let profile_dir =
        std::env::temp_dir().join(format!("cua-hub-browser-{}-{}", std::process::id(), port));
    std::fs::create_dir_all(&profile_dir)?;

    let start_url = opts
        .start_url
        .clone()
        .unwrap_or_else(|| "about:blank".into());
    let mut cmd = Command::new(&binary);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg(format!(
            "--window-size={},{}",
            opts.window_size.0, opts.window_size.1
        ))
        .arg(start_url);
    if !opts.headed {
        cmd.arg("--headless=new").arg("--disable-gpu");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to launch {}", binary.display()))?;
    wait_for_debugger(port).await?;
    let ws_url = first_page_ws(port).await?;
    let (ws, _resp) = connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect to {ws_url}"))?;

    Ok(BrowserSession {
        child,
        profile_dir,
        ws,
        next_id: 1,
        port,
    })
}

impl BrowserSession {
    async fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "id": id, "method": method, "params": params });
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| anyhow!("websocket send: {e}"))?;

        loop {
            let message = tokio::time::timeout(Duration::from_secs(30), self.ws.next())
                .await
                .map_err(|_| anyhow!("CDP timeout waiting for {method}"))?
                .ok_or_else(|| anyhow!("CDP socket closed during {method}"))?
                .map_err(|e| anyhow!("CDP socket error: {e}"))?;
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)?;
                    if value["id"].as_u64() == Some(id) {
                        if let Some(err) = value.get("error") {
                            bail!("CDP {method} error: {err}");
                        }
                        return Ok(value["result"].clone());
                    }
                    // Events and other responses are ignored.
                }
                Message::Ping(payload) => {
                    let _ = self.ws.send(Message::Pong(payload)).await;
                }
                Message::Close(_) => bail!("CDP socket closed"),
                _ => {}
            }
        }
    }

    pub async fn eval(&mut self, script: &str) -> Result<Value> {
        let result = self
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": script,
                    "returnByValue": true,
                    "awaitPromise": true
                }),
            )
            .await?;
        if let Some(exc) = result.get("exceptionDetails") {
            bail!("page script threw: {exc}");
        }
        Ok(result["result"]["value"].clone())
    }

    pub async fn navigate(&mut self, url: &str) -> Result<()> {
        self.send("Page.navigate", json!({ "url": url })).await?;
        for _ in 0..80 {
            let ready = self
                .eval("document.readyState")
                .await
                .unwrap_or_else(|_| json!("loading"));
            if ready == "complete" {
                tokio::time::sleep(Duration::from_millis(250)).await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!("page did not finish loading: {url}")
    }

    /// URL, title and a trimmed visible-text snapshot.
    pub async fn page_summary(&mut self) -> Result<PageSummary> {
        let value = self
            .eval(
                "({ url: location.href, title: document.title, \
                  text: (document.body ? document.body.innerText : '').slice(0, 3000) })",
            )
            .await?;
        Ok(PageSummary {
            url: value["url"].as_str().unwrap_or_default().to_string(),
            title: value["title"].as_str().unwrap_or_default().to_string(),
            text: value["text"].as_str().unwrap_or_default().to_string(),
        })
    }

    pub async fn click(&mut self, selector: &str) -> Result<String> {
        let script = format!(
            r#"(() => {{
                const raw = {sel};
                let el = null;
                if (raw.startsWith('text=')) {{
                    const needle = raw.slice(5).toLowerCase();
                    const candidates = [...document.querySelectorAll('a,button,input,[role=button],[role=link]')];
                    el = candidates.find(e => ((e.innerText || e.value || e.getAttribute('aria-label') || '').toLowerCase().includes(needle)));
                }} else {{
                    el = document.querySelector(raw);
                }}
                if (!el) return 'NOT_FOUND';
                el.scrollIntoView({{ block: 'center' }});
                el.click();
                return 'OK';
            }})()"#,
            sel = serde_json::to_string(selector)?
        );
        let result = self.eval(&script).await?;
        Ok(result.as_str().unwrap_or("UNKNOWN").to_string())
    }

    pub async fn type_text(&mut self, selector: &str, text: &str) -> Result<String> {
        let script = format!(
            r#"(() => {{
                const el = document.querySelector({sel});
                if (!el) return 'NOT_FOUND';
                el.focus();
                const value = {text};
                if ('value' in el) {{ el.value = value; }} else {{ el.textContent = value; }}
                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                return 'OK';
            }})()"#,
            sel = serde_json::to_string(selector)?,
            text = serde_json::to_string(text)?
        );
        let result = self.eval(&script).await?;
        Ok(result.as_str().unwrap_or("UNKNOWN").to_string())
    }

    /// Press a key using the CDP input domain (Enter, Tab, ...).
    pub async fn press_key(&mut self, key: &str) -> Result<()> {
        let (code, vk) = match key {
            "Enter" => ("Enter", 13),
            "Tab" => ("Tab", 9),
            "Escape" => ("Escape", 27),
            other => bail!("unsupported browser key: {other}"),
        };
        for event in ["keyDown", "keyUp"] {
            self.send(
                "Input.dispatchKeyEvent",
                json!({ "type": event, "key": key, "code": code, "windowsVirtualKeyCode": vk }),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn screenshot_jpeg(&mut self, quality: u8) -> Result<Vec<u8>> {
        let result = self
            .send(
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": quality }),
            )
            .await?;
        let data = result["data"]
            .as_str()
            .ok_or_else(|| anyhow!("screenshot has no data"))?;
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("decode screenshot")
    }
}

#[derive(Debug, Clone)]
pub struct PageSummary {
    pub url: String,
    pub title: String,
    pub text: String,
}
