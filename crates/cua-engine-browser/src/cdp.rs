//! Minimal Chrome DevTools Protocol client.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
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
        teardown(Some(&mut self.child), &self.profile_dir);
    }
}

/// A browser process and profile directory that are not yet owned by a session.
///
/// `std::process::Child` does not kill on drop, so every error path after
/// `spawn` used to leave a live Chromium plus its profile directory behind — the
/// only cleanup lives in `BrowserSession::Drop`, which a failed launch never
/// reaches. Dropping this guard kills the process and deletes the directory;
/// [`PendingBrowser::commit`] hands both to the session and makes the guard inert.
struct PendingBrowser {
    child: Option<Child>,
    profile_dir: Option<PathBuf>,
}

impl PendingBrowser {
    /// Take responsibility for a profile directory that has no process yet.
    fn owns_dir(profile_dir: PathBuf) -> Self {
        Self {
            child: None,
            profile_dir: Some(profile_dir),
        }
    }

    /// Adopt the spawned process, returning its pid for the error messages.
    fn attach(&mut self, child: Child) -> u32 {
        let pid = child.id();
        self.child = Some(child);
        pid
    }

    fn commit(&mut self) -> (Child, PathBuf) {
        (
            self.child.take().expect("committed once"),
            self.profile_dir.take().expect("committed once"),
        )
    }
}

impl Drop for PendingBrowser {
    fn drop(&mut self) {
        let mut child = self.child.take();
        match self.profile_dir.take() {
            Some(dir) => teardown(child.as_mut(), &dir),
            None => {
                // Only reachable after `commit` handed both halves over, which
                // disarms the guard; killing here keeps a half-committed guard
                // from detaching a process instead of leaking it.
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                    // Reap it: an unreaped child leaves a zombie on Unix, which
                    // is a leak of a different kind but still a leak.
                    let _ = child.wait();
                }
            }
        }
    }
}

/// Kill a browser and delete its profile directory.
///
/// `Child::kill` is not enough. Measured on this machine (2026-10-24): Edge's
/// spawn target re-execs the real browser and exits, so the handle we own was
/// already dead while the browser root and seven children kept running with the
/// profile open — `kill` "succeeded", `remove_dir_all` failed, and both were
/// swallowed by `let _`. Anything still holding a profile directory we minted is
/// therefore swept by the `--user-data-dir` argument, which only we hand out.
fn teardown(child: Option<&mut Child>, profile_dir: &Path) {
    // Nothing could be holding the profile open if we never got a process, so
    // the sweep is only worth its cost when one existed.
    let had_process = child.is_some();
    if let Some(child) = child {
        let _ = child.kill();
        // Reap it: an unreaped child leaves a zombie on Unix, which is a
        // leak of a different kind but still a leak.
        let _ = child.wait();
    }
    let orphans = if had_process {
        pids_using_profile(profile_dir)
    } else {
        Vec::new()
    };
    if !orphans.is_empty() {
        tracing::warn!(
            pids = ?orphans,
            profile = %profile_dir.display(),
            "browser processes survived the child handle and still hold this profile"
        );
        for pid in orphans {
            kill_pid(pid);
        }
    }
    if !remove_profile_dir(profile_dir) {
        tracing::warn!(
            profile = %profile_dir.display(),
            "profile directory survived teardown and is left in the temp dir"
        );
    }
}

/// Pids of browser processes running with `profile_dir` as their user data dir.
fn pids_using_profile(profile_dir: &Path) -> Vec<u32> {
    let mut found = Vec::new();

    #[cfg(windows)]
    {
        // Single-quoted so the path's backslashes stay literal; a stray quote in
        // the path is doubled rather than interpolated.
        let literal = profile_dir.display().to_string().replace('\'', "''");
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \
             \"Name='msedge.exe' OR Name='chrome.exe' OR Name='chromium.exe' OR Name='headless_shell.exe'\" | \
             Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains('{literal}') }} | \
             ForEach-Object {{ $_.ProcessId }}"
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = out {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let field = line.trim().trim_start_matches('\u{feff}');
                if let Ok(pid) = field.parse::<u32>() {
                    if pid != std::process::id() {
                        found.push(pid);
                    }
                }
            }
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let needle = format!("--user-data-dir={}", profile_dir.display());
        let self_pid = std::process::id();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                if pid == self_pid {
                    continue;
                }
                let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                    continue;
                };
                // Arguments are NUL-separated, so this matches a whole flag
                // rather than a substring of some other option.
                if cmdline.split(|b| *b == 0).any(|a| a == needle.as_bytes()) {
                    found.push(pid);
                }
            }
        }
    }

    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    let _ = profile_dir;

    found
}

/// Force a process we do not own to exit.
fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    #[cfg(not(any(windows, unix)))]
    let _ = pid;
}

/// Delete the profile, giving Chromium a moment to release its file handles.
///
/// Returns whether the directory is actually gone; callers cannot pretend success
/// here, because a swallowed failure is what turned a cleanup bug into hundreds
/// of megabytes per run.
fn remove_profile_dir(dir: &Path) -> bool {
    for attempt in 0..12 {
        if !dir.exists() {
            return true;
        }
        let _ = std::fs::remove_dir_all(dir);
        if !dir.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(if attempt == 0 { 50 } else { 100 }));
    }
    !dir.exists()
}

fn find_browser() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CUA_HUB_BROWSER") {
        let path = PathBuf::from(p);
        if is_launchable(&path) {
            return Some(path);
        }
        // Somebody set the variable on purpose; falling through to the generic
        // "no Chrome/Edge found" would hide the real cause -- a typo, a
        // directory, or a file without an execute bit.
        tracing::warn!(
            path = %path.display(),
            "CUA_HUB_BROWSER is set but is not a launchable executable; ignoring it"
        );
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

    let path = std::env::var_os("PATH").unwrap_or_default();
    candidates
        .into_iter()
        .find_map(|candidate| resolve_candidate(&candidate, &path))
}

/// Can we actually start this path? A directory is not a browser.
#[cfg(unix)]
fn is_launchable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_launchable(path: &Path) -> bool {
    path.is_file()
}

/// Find `candidate` the way a shell would.
///
/// The Linux candidates are bare names (`chromium`, `google-chrome`), and
/// `Path::exists()` resolves a bare name against the *current working directory*.
/// Measured by the issue #11 audit: with `/usr/bin/chromium` installed, launch
/// still answered "no Chrome/Edge found" unless `CUA_HUB_BROWSER` was set -- the
/// engine worked, discovery made it unusable out of the box.
fn resolve_candidate(candidate: &Path, path: &std::ffi::OsStr) -> Option<PathBuf> {
    if candidate.is_absolute() {
        if is_launchable(candidate) {
            return Some(candidate.to_path_buf());
        }
        return None;
    }
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            // An empty PATH entry is the current directory; that is exactly the
            // answer we are replacing.
            continue;
        }
        let hit = dir.join(candidate);
        if is_launchable(&hit) {
            return Some(hit);
        }
    }
    None
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Claim `dir` for exclusive use: `true` only when this call is what created it.
///
/// `create_dir_all` answers "yes" for a directory an earlier crashed launch left
/// behind, which is how a stale profile — cookies, tabs, history and all —
/// silently became the next run's supposedly isolated one.
fn claim_profile_dir(dir: &Path) -> Result<bool> {
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("create {}", dir.display())),
    }
}

/// Mint a profile directory that has never existed before, and own it.
///
/// The name used to be `cua-hub-browser-{pid}-{port}`, which is both guessable
/// and — worse — reusable. The counter plus sub-second clock make two calls in
/// the same process disagree even when the port is reused, and
/// [`claim_profile_dir`] refuses anything already on disk.
fn fresh_profile_dir() -> Result<PathBuf> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    for _ in 0..64 {
        let n = SERIAL.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let dir = base.join(format!(
            "cua-hub-browser-{}-{}-{}",
            std::process::id(),
            n,
            nanos
        ));
        if claim_profile_dir(&dir)? {
            return Ok(dir);
        }
    }
    bail!("could not allocate a fresh browser profile directory")
}

/// Whether a process id is still live, straight from the OS.
///
/// An oracle independent of the `Child` handle: the handles under test are moved
/// into [`PendingBrowser`] and consumed by its `Drop`, so the only way to prove
/// cleanup is to ask the process table rather than the type system.
#[cfg(test)]
fn process_is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let Ok(out) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        // `INFO: No tasks are running which match the search criteria.` has no
        // bare pid field; the CSV row for a live process starts
        // "image.exe","<pid>",...
        text.split("\",\"")
            .nth(1)
            .map(|field| field.trim() == pid.to_string())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// A leaf process that stays running for a minute, used to prove that cleanup
/// really kills instead of merely detaching. `ping`/`sleep` spawn no children, so
/// a surviving pid means the guard failed rather than an orphaned grandchild.
#[cfg(test)]
fn slow_child() -> Child {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("ping");
        c.args(["-n", "60", "127.0.0.1"]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("sleep");
        c.arg("60");
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a slow child")
}

async fn wait_for_debugger(port: u16, attempts: u32) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/json/version");
    for _ in 0..attempts {
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
    launch_with(&binary, opts, 80).await
}

/// Launch `binary` and wait up to `wait_attempts * 125ms` for its devtools port.
///
/// Split out from [`launch`] so the failure paths are reachable from a test with
/// a two-attempt budget instead of the ten-second production one.
async fn launch_with(
    binary: &Path,
    opts: LaunchOptions,
    wait_attempts: u32,
) -> Result<BrowserSession> {
    let port = free_port()?;
    let profile_dir = fresh_profile_dir()?;
    // The guard owns the directory before the process exists, so a `spawn` that
    // fails outright (a bad `CUA_HUB_BROWSER` path) cannot strand it either.
    let mut pending = PendingBrowser::owns_dir(profile_dir.clone());

    let start_url = opts
        .start_url
        .clone()
        .unwrap_or_else(|| "about:blank".into());
    let mut cmd = Command::new(binary);
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
    // From here until the session exists, `pending` is the only owner of the
    // process and the directory; every `?` below unwinds through its Drop.
    let child_pid = pending.attach(child);

    wait_for_debugger(port, wait_attempts)
        .await
        .with_context(|| format!("debugger never came up for pid {child_pid}"))?;
    let (target_id, ws_url) = first_page_target(port)
        .await
        .with_context(|| format!("no page target for pid {child_pid}"))?;
    let (ws, _resp) = connect_async(&ws_url)
        .await
        .with_context(|| format!("failed to connect to {ws_url} (pid {child_pid})"))?;

    let (child, profile_dir) = pending.commit();
    Ok(BrowserSession {
        child,
        profile_dir,
        ws,
        next_id: 1,
        port,
        target_id,
    })
}

/// Shared element lookup, as a JavaScript expression that evaluates to the element or
/// `null`. A plain selector reaches into same-origin iframes; `text=` searches the
/// visible labels of the controls, because the model is asked for what it can see.
const FIND_ELEMENT_JS: &str = r#"((root) => {
    const deepOne = (r) => {
        const e = r.querySelector(raw);
        if (e) return e;
        for (const f of r.querySelectorAll('iframe')) {
            try { const d = f.contentDocument; if (d) { const g = deepOne(d); if (g) return g; } } catch (err) {}
        }
        return null;
    };
    const collect = (r, out) => {
        out.push(...r.querySelectorAll('a,button,input,[role=button],[role=link]'));
        for (const f of r.querySelectorAll('iframe')) {
            try { const d = f.contentDocument; if (d) collect(d, out); } catch (err) {}
        }
        return out;
    };
    if (raw.startsWith('text=')) {
        const needle = raw.slice(5).toLowerCase();
        return collect(document, []).find(e => ((e.innerText || e.value || e.getAttribute('aria-label') || '').toLowerCase().includes(needle))) || null;
    }
    return deepOne(document);
})(document)"#;

/// Quality used only for the before/after comparison of a click. Cheap, and identical
/// on both sides so the bytes mean something.
const PIXEL_DIFF_QUALITY: u8 = 30;

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

    /// What the element at `selector` says about itself, or `found: false`.
    pub async fn read_state(&mut self, selector: &str) -> Result<ElementState> {
        let script = format!(
            r#"(() => {{
                const raw = {sel};
                const el = {FIND};
                if (!el) return {{ found: false }};
                const tag = (el.tagName || '').toLowerCase();
                const isCheck = tag === 'input' && (el.type === 'checkbox' || el.type === 'radio');
                const kind = el.isContentEditable ? 'contenteditable'
                    : isCheck ? 'checkbox'
                    : tag === 'textarea' ? 'textarea'
                    : ('value' in el) ? 'input'
                    : 'other';
                const num = (v) => (v === null || v === undefined) ? null : v;
                const aria = el.getAttribute ? el.getAttribute('aria-expanded') : null;
                return {{
                    found: true,
                    kind,
                    checked: num('checked' in el ? el.checked : null),
                    selected: num('selected' in el ? el.selected : null),
                    expanded: num(aria === null ? null : aria === 'true'),
                    disabled: num('disabled' in el ? el.disabled : null),
                    value: num('value' in el ? String(el.value) : null),
                    text: num(el.textContent === null ? null : String(el.textContent).trim()),
                }};
            }})()"#,
            sel = serde_json::to_string(selector)?,
            FIND = FIND_ELEMENT_JS,
        );
        let value = self.eval(&script).await?;
        serde_json::from_value::<ElementState>(value)
            .map_err(|e| anyhow!("unreadable element state: {e}"))
    }

    /// Click the target and report **what it did**, never a bare `ok`.
    ///
    /// The reading comes from the element's own semantic state (checked / selected /
    /// expanded / disabled / value). An element that publishes nothing -- the styled
    /// `<div>` that acts as a button -- falls back to a screenshot comparison, and if
    /// even that says nothing the answer is `unverifiable`. A click that "succeeded"
    /// while the page stayed still is the exact failure this engine was trusted for.
    pub async fn click(&mut self, selector: &str) -> Result<String> {
        let before = self.read_state(selector).await?;
        if !before.found {
            return Ok("NOT_FOUND".into());
        }
        // The pixels are only worth taking when the element cannot answer for itself.
        let shot_before = if before.comparable() {
            Vec::new()
        } else {
            self.screenshot_jpeg(PIXEL_DIFF_QUALITY)
                .await
                .unwrap_or_default()
        };

        let script = format!(
            r#"(() => {{
                const raw = {sel};
                const el = ({FIND});
                if (!el) return 'NOT_FOUND';
                el.scrollIntoView({{ block: 'center' }});
                el.click();
                return 'OK';
            }})()"#,
            sel = serde_json::to_string(selector)?,
            FIND = FIND_ELEMENT_JS,
        );
        let fired = self.eval(&script).await?;
        if fired.as_str() == Some("NOT_FOUND") {
            return Ok("NOT_FOUND".into());
        }

        // React and friends apply the state change on the next tick, so one immediate
        // read would report `unchanged` for a click that did land. Bounded polling,
        // and the last reading is the one reported.
        let mut after = before.clone();
        for attempt in 0..3 {
            tokio::time::sleep(Duration::from_millis(120)).await;
            after = self.read_state(selector).await?;
            if judge_element_states(Some(&before), Some(&after)) == Some(cua_core::Effect::Changed)
            {
                break;
            }
            let _ = attempt;
        }
        if let Some(effect) = judge_element_states(Some(&before), Some(&after)) {
            let moved = before.moved_fields(&after);
            let reading = if moved.is_empty() {
                after.fields().join(",")
            } else {
                moved.join(",")
            };
            return Ok(format!(
                "clicked {selector} [effect={}] ({reading})",
                effect.as_str()
            ));
        }
        let shot_after = self
            .screenshot_jpeg(PIXEL_DIFF_QUALITY)
            .await
            .unwrap_or_default();
        Ok(match judge_pixels(&shot_before, &shot_after) {
            Some(effect) => format!(
                "clicked {selector} [effect={}] (no state on this element; {} in the viewport)",
                effect.as_str(),
                if effect == cua_core::Effect::Changed {
                    "pixels changed"
                } else {
                    "pixels identical"
                }
            ),
            None => format!(
                "clicked {selector} [effect=unverifiable] (this element publishes no state and the page could not be compared -- nothing here says the click did anything)"
            ),
        })
    }

    /// Place the caret at the **end** of a contenteditable, with no selection range.
    ///
    /// `Input.insertText` writes over whatever is selected, so focusing alone is not
    /// enough: without collapsing to the end, an editor holding existing text would
    /// have that text replaced by ours -- the destruction this path exists to avoid.
    pub async fn focus_end_of(&mut self, selector: &str) -> Result<bool> {
        let script = format!(
            r#"(() => {{
                const raw = {sel};
                const el = {FIND};
                if (!el) return false;
                el.scrollIntoView({{ block: 'center' }});
                el.focus();
                const range = document.createRange();
                range.selectNodeContents(el);
                range.collapse(false);
                const sel = window.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
                return document.activeElement === el;
            }})()"#,
            sel = serde_json::to_string(selector)?,
            FIND = FIND_ELEMENT_JS,
        );
        let focused = self.eval(&script).await?;
        Ok(focused.as_bool().unwrap_or(false))
    }

    /// `Input.insertText` -- the browser's own "type this here", dispatched as a real
    /// input event at the caret. It does not replace content and it does trigger the
    /// framework listeners that setting `.value` from script silently skips.
    pub async fn insert_text(&mut self, text: &str) -> Result<()> {
        self.send("Input.insertText", json!({ "text": text }))
            .await
            .map(|_| ())
    }

    /// Write text into a field and **read it back before believing it**.
    ///
    /// A plain input is verified by equality: if the field does not hold what was
    /// asked for, the step is reported as a failure -- an `ok` here would be the same
    /// lie the desktop `invoke` used to tell. A contenteditable cannot be judged by
    /// equality (it holds markup and the editor rewrites it), so it goes through
    /// `insertText` at the caret and is judged by the three standards in
    /// [`judge_inserted_text`].
    pub async fn type_text(&mut self, selector: &str, text: &str) -> Result<String> {
        let before = self.read_state(selector).await?;
        if !before.found {
            return Ok("NOT_FOUND".into());
        }
        let rich = before.kind == "contenteditable";
        let expected_chars = text.chars().count();

        if rich {
            if !self.focus_end_of(selector).await? {
                return Err(anyhow!(
                    "FAILED: could not put the caret in the contenteditable {selector}; nothing was inserted"
                ));
            }
            self.insert_text(text).await?;
        } else {
            let script = format!(
                r#"(() => {{
                    const raw = {sel};
                    const el = {FIND};
                    if (!el) return false;
                    el.focus();
                    const value = {text};
                    if ('value' in el) {{ el.value = value; }} else {{ el.textContent = value; }}
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                    return true;
                }})()"#,
                sel = serde_json::to_string(selector)?,
                FIND = FIND_ELEMENT_JS,
                text = serde_json::to_string(text)?,
            );
            let written = self.eval(&script).await?;
            if !written.as_bool().unwrap_or(false) {
                return Ok("NOT_FOUND".into());
            }
        }

        let mut after = self.read_state(selector).await?;
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let again = self.read_state(selector).await?;
            after = again;
            let ok = if rich {
                insertion_ok(&before, &after, text)
            } else {
                after.value.as_deref() == Some(text)
            };
            if ok {
                break;
            }
        }

        if rich {
            if insertion_ok(&before, &after, text) {
                return Ok(format!(
                    "inserted {expected_chars} chars into {selector} [verified: not empty, +{} chars, phrase present] content now {:?}",
                    after
                        .text
                        .as_ref()
                        .map(|t| t.chars().count())
                        .unwrap_or(0)
                        .saturating_sub(before.text.as_ref().map(|t| t.chars().count()).unwrap_or(0)),
                    after.text.as_deref().map(|t| truncate(t, 60))
                ));
            }
            return Err(anyhow!(
                "FAILED: insertText into the contenteditable {selector} did not take -- content reads {:?} (was {:?}), expected at least {expected_chars} new characters and the phrase to be present",
                after.text.as_deref().map(|t| truncate(t, 60)),
                before.text.as_deref().map(|t| truncate(t, 60)),
            ));
        }

        match judge_text_assignment(text, after.value.as_deref()) {
            cua_core::Effect::Changed => Ok(format!(
                "typed {expected_chars} chars into {selector} [verified: value reads as sent]"
            )),
            cua_core::Effect::Unchanged => Err(anyhow!(
                "FAILED: {selector} reads {:?}, not {text:?} -- the text did not land, and this is not reported as a success",
                after.value.as_deref().map(|v| truncate(v, 60))
            )),
            cua_core::Effect::Unverifiable => Ok(format!(
                "typed {expected_chars} chars into {selector} [unverifiable: the element reads no value back -- not proof that it landed]"
            )),
        }
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

/// The semantic state a page element publishes about itself.
///
/// This is what a click is checked against. A checkbox knows it got checked, an
/// option knows it got selected, a field knows its value; a `<div>` styled to look
/// like a button knows nothing, and that case is answered `unverifiable` rather
/// than with an `ok` nobody earned.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ElementState {
    pub found: bool,
    /// `checkbox` / `input` / `textarea` / `contenteditable` / `other`.
    pub kind: String,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub disabled: Option<bool>,
    pub value: Option<String>,
    pub text: Option<String>,
}

impl ElementState {
    /// Did the element tell us anything an action could have moved?
    pub fn comparable(&self) -> bool {
        self.found
            && (self.checked.is_some()
                || self.selected.is_some()
                || self.expanded.is_some()
                || self.disabled.is_some()
                || self.value.is_some())
    }

    /// A short human-readable rendering, for the note the model reads back.
    pub fn fields(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(c) = self.checked {
            out.push(format!("checked={c}"));
        }
        if let Some(s) = self.selected {
            out.push(format!("selected={s}"));
        }
        if let Some(e) = self.expanded {
            out.push(format!("expanded={e}"));
        }
        if let Some(d) = self.disabled {
            out.push(format!("disabled={d}"));
        }
        if let Some(v) = &self.value {
            out.push(format!("value={}", truncate(v, 40)));
        }
        out
    }

    /// Only the fields that moved, as `name before->after`.
    ///
    /// The note is what the model reads next turn; a wall of unchanged fields buries
    /// the one thing that happened, and an `unchanged` verdict should not pretend to
    /// have found differences.
    pub fn moved_fields(&self, after: &ElementState) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |name: &str, before: Option<bool>, after_v: Option<bool>| {
            if before != after_v {
                out.push(format!("{name} {:?}->{:?}", before, after_v));
            }
        };
        push("checked", self.checked, after.checked);
        push("selected", self.selected, after.selected);
        push("expanded", self.expanded, after.expanded);
        push("disabled", self.disabled, after.disabled);
        if self.value != after.value {
            let short =
                |v: &Option<String>| v.as_deref().map(|s| truncate(s, 40)).unwrap_or_default();
            out.push(format!(
                "value {:?}->{:?}",
                short(&self.value),
                short(&after.value)
            ));
        }
        out
    }
}

/// Compare two readings of the same element.
///
/// `None` means neither side said anything, which is not a verdict: the caller falls
/// back to pixels and, failing that, reports `unverifiable`. A field that appeared on
/// one side only counts as a move -- `checked` flipping from absent to present is
/// exactly what a checkbox click is supposed to do.
pub fn judge_element_states(
    before: Option<&ElementState>,
    after: Option<&ElementState>,
) -> Option<cua_core::Effect> {
    use cua_core::Effect;
    let before = before.filter(|s| s.comparable())?;
    let after = after.filter(|s| s.comparable())?;
    // `text` is deliberately not compared: it is the page's own wording and any
    // re-render moves it, which would report `changed` for a click that did nothing.
    let moved = before.checked != after.checked
        || before.selected != after.selected
        || before.expanded != after.expanded
        || before.disabled != after.disabled
        || before.value != after.value;
    Some(if moved {
        Effect::Changed
    } else {
        Effect::Unchanged
    })
}

/// Compare two screenshots taken with the same encoder settings.
///
/// Coarse on purpose. The JPEG encoder here is deterministic for identical input, so
/// identical bytes are a real signal that nothing in the viewport moved; different
/// bytes only say "something in the picture changed", which an animation can also do.
/// That is good enough as the fallback when an element publishes no state, and it is
/// not presented as more than it is.
pub fn judge_pixels(before: &[u8], after: &[u8]) -> Option<cua_core::Effect> {
    use cua_core::Effect;
    if before.is_empty() || after.is_empty() {
        return None;
    }
    Some(if before == after {
        Effect::Unchanged
    } else {
        Effect::Changed
    })
}

/// Did the field end up holding exactly the text that was asked for?
///
/// A plain input is an equality test, because that is what "typed into it" means.
pub fn judge_text_assignment(expected: &str, read_back: Option<&str>) -> cua_core::Effect {
    use cua_core::Effect;
    match read_back {
        Some(actual) if actual == expected => Effect::Changed,
        Some(_) => Effect::Unchanged,
        None => Effect::Unverifiable,
    }
}

/// The three standards for a rich-text insertion, checked instead of equality.
///
/// A contenteditable holds markup and the editor rewrites what it is given, so
/// "equal to what we sent" is not the right test -- but "nothing appeared" must not
/// pass either. The three are: the region is not empty, the length it gained accounts
/// for what was inserted, and the distinctive phrase we sent is actually in there.
/// `key_phrase` is a substring chosen by the caller, never the whole payload.
pub fn judge_inserted_text(
    before_len: usize,
    after_len: usize,
    inserted: &str,
    key_phrase: &str,
    after_text: &str,
) -> cua_core::Effect {
    use cua_core::Effect;
    let gained = after_len.saturating_sub(before_len);
    let wanted = inserted.chars().count();
    let non_empty = !after_text.trim().is_empty();
    let length_accounts = wanted == 0 || gained >= wanted;
    let phrase_present = !key_phrase.trim().is_empty() && after_text.contains(key_phrase);
    if non_empty && length_accounts && phrase_present {
        Effect::Changed
    } else {
        Effect::Unchanged
    }
}

/// The distinctive fragment of an insertion: the middle of it, so a placeholder or a
/// leading/trailing space does not look like a successful write.
pub fn key_phrase_of(text: &str) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return trimmed.to_string();
    }
    let start = chars.len() / 4;
    chars
        .iter()
        .skip(start)
        .take(chars.len() / 2)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The three standards applied to the two readings of a rich-text region.
fn insertion_ok(before: &ElementState, after: &ElementState, inserted: &str) -> bool {
    let before_text = before.text.clone().unwrap_or_default();
    let after_text = after.text.clone().unwrap_or_default();
    judge_inserted_text(
        before_text.chars().count(),
        after_text.chars().count(),
        inserted,
        &key_phrase_of(inserted),
        &after_text,
    ) == cua_core::Effect::Changed
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Profile directories live in a shared temp dir and are found by prefix, so
    /// these tests take turns. An async lock, because the guard is held across
    /// awaits.
    fn temp_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    async fn temp_slot() -> tokio::sync::MutexGuard<'static, ()> {
        temp_lock().lock().await
    }

    fn leftover_profiles() -> Vec<PathBuf> {
        let prefix = format!("cua-hub-browser-{}-", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with(&prefix))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A browser that exits at once, so `wait_for_debugger` is the failing step.
    fn unresponsive_browser() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from("cmd")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/bin/true")
        }
    }

    #[tokio::test]
    async fn a_profile_directory_is_never_reused() {
        let _slot = temp_slot().await;
        let first = fresh_profile_dir().unwrap();
        let second = fresh_profile_dir().unwrap();
        assert_ne!(first, second, "two launches cannot share a profile");
        // Both were created, not merely named: `create_dir` only succeeds on a
        // name that had no contents, so a leaked profile can never be adopted.
        assert!(first.is_dir() && second.is_dir());
        std::fs::remove_dir_all(&first).unwrap();
        std::fs::remove_dir_all(&second).unwrap();
    }

    #[tokio::test]
    async fn a_stale_profile_is_never_adopted() {
        let _slot = temp_slot().await;
        let dir = fresh_profile_dir().unwrap();
        // What a crashed launch leaves behind is exactly this: a directory with
        // someone else's state in it.
        std::fs::write(dir.join("Cookies"), b"a previous run's state").unwrap();
        assert!(
            !claim_profile_dir(&dir).unwrap(),
            "an occupied profile directory was handed out again"
        );
        let next = fresh_profile_dir().unwrap();
        assert_ne!(next, dir, "the mint landed on the stale directory");
        assert!(
            !next.join("Cookies").exists(),
            "the new profile inherited the stale one's files"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&next).unwrap();
    }

    #[tokio::test]
    async fn a_bare_browser_name_is_found_through_path() {
        let _slot = temp_slot().await;
        // A unique scratch directory standing in for one PATH entry. The process
        // environment is never mutated, so the test stays parallel-safe.
        let dir = fresh_profile_dir().unwrap();
        let name = format!("cua-hub-fake-browser{}", std::env::consts::EXE_SUFFIX);
        let planted = dir.join(&name);
        std::fs::write(&planted, b"not a real browser").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = resolve_candidate(Path::new(&name), dir.as_os_str())
            .expect("a bare name listed in PATH must resolve to the planted file");
        assert_eq!(found, planted, "the answer must be an absolute path");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Same name, same file, no execute bit. `Path::exists` accepted it;
            // a text file on PATH is not something we can spawn.
            std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                resolve_candidate(Path::new(&name), dir.as_os_str()),
                None,
                "a non-executable file is not a browser"
            );
        }

        // A directory that merely has the right name is not launchable either.
        let lookalike = dir.join(format!("cua-hub-lookalike{}", std::env::consts::EXE_SUFFIX));
        std::fs::create_dir(&lookalike).unwrap();
        assert_eq!(
            resolve_candidate(Path::new(lookalike.file_name().unwrap()), dir.as_os_str()),
            None,
            "a directory is not an executable"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_dropped_guard_kills_a_live_child_and_removes_the_profile() {
        let _slot = temp_slot().await;
        let child = slow_child();
        let pid = child.id();
        let dir = fresh_profile_dir().unwrap();
        assert!(process_is_alive(pid), "the fixture child is running");
        let mut guard = PendingBrowser::owns_dir(dir.clone());
        guard.attach(child);
        drop(guard);
        assert!(
            !process_is_alive(pid),
            "pid {pid} outlived the guard that owned it"
        );
        assert!(
            !dir.exists(),
            "the profile outlived the guard that owned it"
        );
    }

    #[tokio::test]
    async fn a_committed_guard_leaves_both_the_child_and_the_profile_alone() {
        let _slot = temp_slot().await;
        let child = slow_child();
        let pid = child.id();
        let dir = fresh_profile_dir().unwrap();
        let mut guard = PendingBrowser::owns_dir(dir.clone());
        guard.attach(child);
        let (mut child, handed_dir) = guard.commit();
        drop(guard);
        assert!(process_is_alive(pid), "committing must not kill");
        assert!(
            handed_dir == dir && dir.is_dir(),
            "committing must not delete"
        );
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `BrowserSession` is not `Debug`, so `expect_err` is unavailable; fail the
    /// test with the session dropped instead (its own `Drop` cleans up).
    async fn launch_failure(binary: &Path, attempts: u32) -> anyhow::Error {
        match launch_with(binary, LaunchOptions::default(), attempts).await {
            Err(e) => e,
            Ok(_session) => panic!("expected a launch that never reaches devtools"),
        }
    }

    #[tokio::test]
    async fn a_failed_launch_leaves_no_profile_behind() {
        let _slot = temp_slot().await;
        let before = leftover_profiles();
        let err = launch_failure(&unresponsive_browser(), 2).await;
        assert!(
            err.to_string().contains("debugger never came up"),
            "unexpected error: {err}"
        );
        assert_eq!(
            leftover_profiles(),
            before,
            "the failed launch leaked its profile directory"
        );
    }

    #[tokio::test]
    async fn a_launch_that_cannot_spawn_leaves_no_profile_behind() {
        let _slot = temp_slot().await;
        let before = leftover_profiles();
        let err = launch_failure(Path::new("cua-hub-no-such-browser-9f3a2"), 1).await;
        assert!(
            err.to_string().contains("failed to launch"),
            "unexpected error: {err}"
        );
        assert_eq!(
            leftover_profiles(),
            before,
            "a launch that never spawned still created and abandoned a profile"
        );
    }

    #[tokio::test]
    async fn a_failed_launch_names_the_pid_it_killed() {
        let _slot = temp_slot().await;
        let text = format!("{:#}", launch_failure(&unresponsive_browser(), 1).await);
        // The chain must carry the child pid: without it an operator cannot tell
        // a bad `CUA_HUB_BROWSER` path from a browser that started and hung.
        let pid_part = text
            .split("debugger never came up for pid ")
            .nth(1)
            .unwrap_or_default();
        assert!(
            pid_part.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "error does not report the killed pid: {text}"
        );
    }

    /// The check CI cannot make without a browser, and the one that caught the
    /// real bug: the process we spawn is not always the browser process, so a
    /// session drop must leave no profile directory, no answering devtools port,
    /// and no browser still running with our profile.
    #[tokio::test]
    #[ignore = "needs a local Chrome/Edge; run: cargo test -p cua-engine-browser -- --ignored"]
    async fn a_real_browser_session_leaves_nothing_behind() {
        let _slot = temp_slot().await;
        let binary = find_browser().expect("set CUA_HUB_BROWSER to a Chrome/Edge executable");
        let before = leftover_profiles();
        let session = launch_with(&binary, LaunchOptions::default(), 80)
            .await
            .expect("launch a real browser");
        let dir = session.profile_dir.clone();
        let port = session.port;
        assert!(
            dir.is_dir(),
            "the profile should exist while the session runs"
        );
        drop(session);

        assert!(!dir.exists(), "profile {dir:?} survived the session drop");
        let still_serving = reqwest::get(format!("http://127.0.0.1:{port}/json/version"))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        assert!(
            !still_serving,
            "devtools port {port} still answers: a browser process survived the drop"
        );
        assert_eq!(
            pids_using_profile(&dir),
            Vec::<u32>::new(),
            "browser processes still reference the deleted profile"
        );
        assert_eq!(
            leftover_profiles(),
            before,
            "teardown left a profile directory in the temp dir"
        );
    }
    fn state(checked: Option<bool>, value: Option<&str>) -> ElementState {
        ElementState {
            found: true,
            kind: "checkbox".into(),
            checked,
            selected: None,
            expanded: None,
            disabled: Some(false),
            value: value.map(str::to_string),
            text: Some("x".into()),
        }
    }

    #[test]
    fn a_state_move_on_the_element_is_the_verdict_for_a_click() {
        use cua_core::Effect;
        assert_eq!(
            judge_element_states(
                Some(&state(Some(false), None)),
                Some(&state(Some(true), None))
            ),
            Some(Effect::Changed)
        );
        assert_eq!(
            judge_element_states(
                Some(&state(Some(true), None)),
                Some(&state(Some(true), None))
            ),
            Some(Effect::Unchanged)
        );
        // The page's own wording is not evidence: `text` is left out on purpose, or
        // every repaint would look like a click that did something.
        let mut original = state(Some(true), None);
        original.text = Some("x".into());
        let mut reworded = state(Some(true), None);
        reworded.text = Some("completely different wording".into());
        assert_eq!(
            judge_element_states(Some(&original), Some(&reworded)),
            Some(Effect::Unchanged)
        );
    }

    #[test]
    fn an_element_that_says_nothing_gets_no_verdict_rather_than_a_free_ok() {
        // `comparable()` false on both sides sends the caller to pixels, and failing
        // that to `unverifiable` -- never the bare "OK" this used to return.
        let mute = ElementState {
            found: true,
            kind: "other".into(),
            ..Default::default()
        };
        assert!(!mute.comparable());
        assert_eq!(judge_element_states(Some(&mute), Some(&mute)), None);
        assert_eq!(judge_element_states(None, Some(&mute)), None);
    }

    #[test]
    fn identical_pixels_are_a_real_signal_and_a_failed_capture_is_none() {
        use cua_core::Effect;
        assert_eq!(judge_pixels(b"abc", b"abc"), Some(Effect::Unchanged));
        assert_eq!(judge_pixels(b"abc", b"abd"), Some(Effect::Changed));
        assert_eq!(judge_pixels(b"", b"abc"), None);
    }

    #[test]
    fn a_field_holding_other_text_did_not_take_the_text() {
        use cua_core::Effect;
        assert_eq!(judge_text_assignment("abc", Some("abc")), Effect::Changed);
        assert_eq!(judge_text_assignment("abc", Some("ab")), Effect::Unchanged);
        assert_eq!(judge_text_assignment("abc", None), Effect::Unverifiable);
    }

    #[test]
    fn rich_text_is_judged_by_three_standards_not_by_equality() {
        use cua_core::Effect;
        let inserted = "追加的文字 APPENDED-OK";
        let phrase = key_phrase_of(inserted);
        assert!(
            phrase.chars().count() >= 4,
            "a slice of the payload, not the whole thing: {phrase}"
        );
        assert_eq!(
            judge_inserted_text(
                8,
                8 + inserted.chars().count(),
                inserted,
                &phrase,
                "已有内容 追加的文字 APPENDED-OK",
            ),
            Effect::Changed,
            "existing content kept plus the phrase present is what append means"
        );
        assert_eq!(
            judge_inserted_text(8, 8, inserted, &phrase, ""),
            Effect::Unchanged,
            "nothing appeared"
        );
        assert_eq!(
            judge_inserted_text(
                8,
                8 + inserted.chars().count(),
                inserted,
                &phrase,
                "已有内容 别的字",
            ),
            Effect::Unchanged,
            "length alone is not proof: the phrase has to be in there"
        );
        assert_eq!(
            judge_inserted_text(30, 12, inserted, &phrase, "truncated"),
            Effect::Unchanged,
            "the content went DOWN -- an overwrite that destroyed text is not a successful insert"
        );
    }

    #[test]
    fn a_short_payload_is_its_own_key_phrase() {
        assert_eq!(key_phrase_of("  ok  "), "ok");
        assert_eq!(key_phrase_of("12345678"), "12345678");
    }

    /// The three items of the urgent contract, against a real page.
    ///
    /// Ignored by default because it launches a browser. Run with
    /// `cargo test -p cua-engine-browser -- --ignored readback_verdicts`.
    #[tokio::test]
    #[ignore = "launches a real browser"]
    async fn readback_verdicts_on_a_real_page() {
        let _slot = temp_slot().await;
        let binary = find_browser().expect("set CUA_HUB_BROWSER to a Chrome/Edge executable");
        let html = "<!doctype html><meta charset=utf-8><title>cua readback fixture</title>\n\
             <input id=agree type=checkbox>\n\
             <input id=field>\n\
             <input id=upper>\n\
             <div id=note contenteditable=true>已有内容 KEEP-ME</div>\n\
             <div id=stub contenteditable=true>seed</div>\n\
             <div id=plainbtn role=button>press me</div>\n\
             <script>\n\
               const up = document.getElementById('upper');\n\
               up.addEventListener('input', () => { up.value = up.value.toUpperCase(); });\n\
               const stub = document.getElementById('stub');\n\
               stub.addEventListener('input', () => { stub.textContent = ''; });\n\
             </script>";
        let file = std::env::temp_dir().join("cua-readback-fixture.html");
        std::fs::write(&file, html).unwrap();
        let url = format!("file:///{}", file.display().to_string().replace('\\', "/"));

        let mut session = launch_with(
            &binary,
            LaunchOptions {
                ..Default::default()
            },
            80,
        )
        .await
        .expect("launch a real browser");
        // The same order `run_task` uses: the command-line URL is not a guarantee the
        // attached tab is on that page, so navigate and check the page says so.
        session.navigate(&url).await.unwrap();
        let page = session.page_summary().await.unwrap();
        assert!(
            page.title.contains("cua readback fixture"),
            "the fixture did not load: {}",
            page.url
        );

        // 1. click a checkbox: the element's own state is the verdict.
        let first = session.click("#agree").await.unwrap();
        assert!(first.contains("[effect=changed]"), "{first}");
        assert!(
            first.contains("checked Some(false)->Some(true)"),
            "the reading has to be in the note: {first}"
        );
        let second = session.click("#agree").await.unwrap();
        assert!(
            second.contains("checked Some(true)->Some(false)"),
            "the second click has to show it going back: {second}"
        );

        // 2. type into a field: believed only when the field reads back what was sent.
        let typed = session.type_text("#field", "HELLO-世界").await.unwrap();
        assert!(typed.contains("[verified"), "{typed}");

        // negative control: an input that rewrites what it is given must fail, not ok.
        let note = session
            .type_text("#upper", "abc")
            .await
            .expect_err("the fixture uppercases to ABC, which is not what was sent");
        assert!(note.to_string().starts_with("FAILED:"), "{note}");
        assert!(note.to_string().contains("ABC"), "{note}");

        // 3. contenteditable: append at the caret and keep what was already there.
        let rich = session
            .type_text("#note", "追加的文字 APPENDED-OK")
            .await
            .unwrap();
        assert!(rich.contains("[verified"), "{rich}");
        let after = session.read_state("#note").await.unwrap();
        let held = after.text.unwrap_or_default();
        assert!(
            held.contains("KEEP-ME"),
            "the insertion replaced the content instead of appending: {held}"
        );
        assert!(held.contains("APPENDED-OK"), "{held}");

        // negative control: an editor that drops the insertion is reported as a failure.
        let note = session
            .type_text("#stub", "应当留下痕迹的文字 SOME-PHRASE")
            .await
            .expect_err("the fixture clears the contenteditable on input");
        assert!(note.to_string().starts_with("FAILED:"), "{note}");

        // a control that publishes no state never gets a bare ok.
        let mute = session.click("#plainbtn").await.unwrap();
        assert_ne!(mute, "OK", "the answer this work exists to retire");
        assert!(mute.contains("[effect="), "{mute}");
    }
}
