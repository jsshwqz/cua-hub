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
    /// Target id of the page we are attached to.
    target_id: String,
}

#[derive(Debug, Clone)]
pub struct TabInfo {
    pub id: String,
    pub title: String,
    pub url: String,
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

async fn first_page_target(port: u16) -> Result<(String, String)> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let list: Value = reqwest::get(&url).await?.json().await?;
    list.as_array()
        .and_then(|pages| pages.iter().find(|p| p["type"] == "page"))
        .and_then(|p| {
            Some((
                p["id"].as_str()?.to_string(),
                p["webSocketDebuggerUrl"].as_str()?.to_string(),
            ))
        })
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
    let (target_id, ws_url) = first_page_target(port).await?;
    let (ws, _resp) = connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect to {ws_url}"))?;

    Ok(BrowserSession {
        child,
        profile_dir,
        ws,
        next_id: 1,
        port,
        target_id,
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

    /// URL, title, visible text and a compact accessibility tree.
    pub async fn page_summary(&mut self) -> Result<PageSummary> {
        let value = self
            .eval(
                "({ url: location.href, title: document.title, \
                  text: (document.body ? document.body.innerText : '').slice(0, 3000) })",
            )
            .await?;
        let ax = self.ax_summary().await.unwrap_or_default();
        Ok(PageSummary {
            url: value["url"].as_str().unwrap_or_default().to_string(),
            title: value["title"].as_str().unwrap_or_default().to_string(),
            text: value["text"].as_str().unwrap_or_default().to_string(),
            ax,
        })
    }

    /// Flatten the CDP accessibility tree into prompt-friendly lines.
    pub async fn ax_summary(&mut self) -> Result<String> {
        let result = self.send("Accessibility.getFullAXTree", json!({})).await?;
        let nodes = result["nodes"].as_array().cloned().unwrap_or_default();
        let mut out = String::new();
        let mut kept = 0usize;
        for node in nodes {
            if node["ignored"].as_bool().unwrap_or(true) {
                continue;
            }
            let role = node["role"]["value"].as_str().unwrap_or_default();
            let name = node["name"]["value"].as_str().unwrap_or_default().trim();
            if name.is_empty() || role == "generic" || role == "none" {
                continue;
            }
            let line = format!("- {role}: {}\n", truncate(name, 120));
            if out.len() + line.len() > 5000 || kept >= 120 {
                break;
            }
            out.push_str(&line);
            kept += 1;
        }
        Ok(out)
    }

    pub async fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        let size = self.eval("({w: innerWidth, h: innerHeight})").await?;
        let x = size["w"].as_i64().unwrap_or(800) / 2;
        let y = size["h"].as_i64().unwrap_or(600) / 2;
        self.send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": x,
                "y": y,
                "deltaX": dx,
                "deltaY": dy
            }),
        )
        .await?;
        Ok(())
    }

    /// Poll until the selector exists (searching same-origin iframes too).
    pub async fn wait_for_selector(&mut self, selector: &str, timeout_ms: u64) -> Result<bool> {
        let sel = serde_json::to_string(selector)?;
        let script = format!(
            r#"(() => {{
                const sel = {sel};
                const deep = (root) => {{
                    if (root.querySelector(sel)) return true;
                    for (const f of root.querySelectorAll('iframe')) {{
                        try {{
                            const d = f.contentDocument;
                            if (d && deep(d)) return true;
                        }} catch (e) {{}}
                    }}
                    return false;
                }};
                return deep(document);
            }})()"#
        );
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.max(200));
        loop {
            if self.eval(&script).await.unwrap_or(json!(false)) == json!(true) {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let url = format!("http://127.0.0.1:{}/json/list", self.port);
        let list: Value = reqwest::get(&url).await?.json().await?;
        let mut tabs = Vec::new();
        for page in list.as_array().into_iter().flatten() {
            if page["type"] != "page" {
                continue;
            }
            tabs.push(TabInfo {
                id: page["id"].as_str().unwrap_or_default().to_string(),
                title: page["title"].as_str().unwrap_or_default().to_string(),
                url: page["url"].as_str().unwrap_or_default().to_string(),
            });
        }
        Ok(tabs)
    }

    pub async fn switch_tab(&mut self, index: usize) -> Result<()> {
        let tabs = self.list_tabs().await?;
        let tab = tabs
            .get(index)
            .ok_or_else(|| anyhow!("tab index {index} out of range ({} tabs)", tabs.len()))?;
        let url = format!("http://127.0.0.1:{}/json/list", self.port);
        let list: Value = reqwest::get(&url).await?.json().await?;
        let ws_url = list
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["id"].as_str() == Some(tab.id.as_str()))
            .and_then(|p| p["webSocketDebuggerUrl"].as_str())
            .ok_or_else(|| anyhow!("tab has no websocket URL"))?
            .to_string();
        self.ws = connect_async(&ws_url)
            .await
            .with_context(|| format!("switch to tab {index}"))?
            .0;
        self.target_id = tab.id.clone();
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }

    pub async fn new_tab(&mut self, url: &str) -> Result<()> {
        let mut endpoint =
            reqwest::Url::parse(&format!("http://127.0.0.1:{}/json/new", self.port))?;
        endpoint.query_pairs_mut().append_pair("url", url);
        let value: Value = reqwest::Client::new()
            .put(endpoint)
            .send()
            .await?
            .json()
            .await?;
        let id = value["id"].as_str().unwrap_or_default().to_string();
        let ws_url = value["webSocketDebuggerUrl"]
            .as_str()
            .ok_or_else(|| anyhow!("new tab response has no websocket URL"))?
            .to_string();
        self.ws = connect_async(&ws_url).await?.0;
        self.target_id = id;
        tokio::time::sleep(Duration::from_millis(250)).await;
        Ok(())
    }

    pub async fn close_tab(&mut self, index: Option<usize>) -> Result<()> {
        let tabs = self.list_tabs().await?;
        let id = match index {
            Some(i) => tabs
                .get(i)
                .ok_or_else(|| anyhow!("tab index {i} out of range ({} tabs)", tabs.len()))?
                .id
                .clone(),
            None => self.target_id.clone(),
        };
        if id == self.target_id {
            let resp =
                reqwest::get(format!("http://127.0.0.1:{}/json/close/{id}", self.port)).await?;
            if !resp.status().is_success() {
                bail!("close current tab: HTTP {}", resp.status());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let remaining = self.list_tabs().await?;
            if !remaining.is_empty() {
                self.switch_tab(0).await.ok();
            }
        } else {
            // Target.closeTarget works on page sessions in Chromium; fall back
            // to the HTTP endpoint if this build rejects it.
            if self
                .send("Target.closeTarget", json!({ "targetId": id }))
                .await
                .is_err()
            {
                let _ =
                    reqwest::get(format!("http://127.0.0.1:{}/json/close/{id}", self.port)).await?;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }

    pub async fn click(&mut self, selector: &str) -> Result<String> {
        let script = format!(
            r#"(() => {{
                const raw = {sel};
                const deepOne = (root) => {{
                    let el = root.querySelector(raw);
                    if (el) return el;
                    for (const f of root.querySelectorAll('iframe')) {{
                        try {{
                            const d = f.contentDocument;
                            if (d) {{ const r = deepOne(d); if (r) return r; }}
                        }} catch (e) {{}}
                    }}
                    return null;
                }};
                const collect = (root, out) => {{
                    out.push(...root.querySelectorAll('a,button,input,[role=button],[role=link]'));
                    for (const f of root.querySelectorAll('iframe')) {{
                        try {{
                            const d = f.contentDocument;
                            if (d) collect(d, out);
                        }} catch (e) {{}}
                    }}
                    return out;
                }};
                let el = null;
                if (raw.startsWith('text=')) {{
                    const needle = raw.slice(5).toLowerCase();
                    const candidates = collect(document, []);
                    el = candidates.find(e => ((e.innerText || e.value || e.getAttribute('aria-label') || '').toLowerCase().includes(needle)));
                }} else {{
                    el = deepOne(document);
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
                const sel = {sel};
                const deepOne = (root) => {{
                    let el = root.querySelector(sel);
                    if (el) return el;
                    for (const f of root.querySelectorAll('iframe')) {{
                        try {{
                            const d = f.contentDocument;
                            if (d) {{ const r = deepOne(d); if (r) return r; }}
                        }} catch (e) {{}}
                    }}
                    return null;
                }};
                const el = deepOne(document);
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
    /// Flattened accessibility tree (role: name per line).
    pub ax: String,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
