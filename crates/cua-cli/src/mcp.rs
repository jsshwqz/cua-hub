//! Minimal MCP server: newline-delimited JSON-RPC 2.0 over stdio, plus a
//! streamable-HTTP style POST endpoint (`/mcp`) for remote or containerised
//! clients.
//!
//! Exposes the machine as tools so other agents can drive it: run a task,
//! capture the screen, read the accessibility tree, find/invoke controls, and
//! inject mouse/keyboard input.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use cua_core::{Action, EngineKind, Point};
use serde_json::{json, Value};

use crate::elements;
use crate::observations;
use crate::runner::{self, RunOptions};

/// Server-initiated elicitation over stdio. Clients that support MCP
/// elicitation answer `elicitation/create`; others simply never respond and the
/// action is treated as declined after a timeout.
#[derive(Clone)]
struct Elicitor {
    pending: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>>,
    counter: Arc<AtomicU64>,
}

impl Elicitor {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicU64::new(1)),
        }
    }

    async fn ask(&self, message: &str) -> Option<bool> {
        let id = format!("elicit-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(id.clone(), tx);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": message,
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "approve": {
                            "type": "boolean",
                            "title": "Approve",
                            "description": "Allow this high-risk action"
                        }
                    },
                    "required": ["approve"]
                }
            }
        });
        if write_msg(&request).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return None;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(resp)) => {
                let action = resp["result"]["action"].as_str().unwrap_or_default();
                let approve = resp["result"]["content"]["approve"]
                    .as_bool()
                    .unwrap_or(false);
                Some(action == "accept" && approve)
            }
            _ => {
                self.pending.lock().expect("pending lock").remove(&id);
                None
            }
        }
    }
}

fn elicitor() -> &'static OnceLock<Elicitor> {
    static ELICITOR: OnceLock<Elicitor> = OnceLock::new();
    &ELICITOR
}

fn approver_from_elicitor() -> Option<runner::Approver> {
    let elicitor = elicitor().get()?.clone();
    Some(Arc::new(move |risk, description| {
        let elicitor = elicitor.clone();
        Box::pin(async move {
            // The tier travels with the question: an elicitation that just says
            // "approve this?" is how a confirmation becomes an Enter key.
            elicitor
                .ask(&format!(
                    "Approve this {} risk action? {description}",
                    risk.as_str()
                ))
                .await
                .unwrap_or(false)
        })
    }))
}

pub(crate) fn run_options_from_args(args: &Value) -> std::result::Result<RunOptions, Value> {
    let task = required_str(args, "task")?;
    let engine = match args["engine"].as_str() {
        Some("vision") => Some(EngineKind::Vision),
        Some("code") => Some(EngineKind::Code),
        Some("access") => Some(EngineKind::Access),
        Some("browser") => Some(EngineKind::Browser),
        _ => None,
    };
    Ok(RunOptions {
        task,
        max_steps: args["max_steps"].as_u64().unwrap_or(20) as u32,
        yes: args["yes"].as_bool().unwrap_or(false),
        allow_high: args["allow_high"].as_bool().unwrap_or(false),
        dry_run: args["dry_run"].as_bool().unwrap_or(false),
        engine,
        provider: crate::runner::ProviderArg::Auto,
        model: args["model"].as_str().map(str::to_string),
        max_edge: args["max_edge"].as_u64().unwrap_or(1568) as u32,
        run_dir: None,
        settle_ms: args["settle_ms"].as_u64().unwrap_or(600),
        ui_tree: args["ui_tree"].as_bool().unwrap_or(false),
        browser_headed: args["browser_headed"].as_bool().unwrap_or(false),
        approver: approver_from_elicitor(),
    })
}

/// Mirror of [`run_options_from_args`] for `orchestrate`, so the confirmation
/// defaults of both entry points are stated in one place and can be tested.
pub(crate) fn orchestrate_options_from_args(
    args: &Value,
) -> std::result::Result<crate::orchestrator::OrchestrateOptions, Value> {
    Ok(crate::orchestrator::OrchestrateOptions {
        task: required_str(args, "task")?,
        dry_run: args["dry_run"].as_bool().unwrap_or(false),
        max_plan_steps: args["max_plan_steps"].as_u64().unwrap_or(6) as usize,
        max_steps_per_engine: args["max_steps"].as_u64().unwrap_or(10) as u32,
        retries_per_step: args["retries"].as_u64().unwrap_or(1) as u32,
        max_replans: args["max_replans"].as_u64().unwrap_or(1) as usize,
        model: args["model"].as_str().map(str::to_string),
        headed: args["headed"].as_bool().unwrap_or(false),
        // Neither default may be `true`: a caller that lets a model plan commands
        // must say so explicitly, or be able to answer a confirmation.
        yes: args["yes"].as_bool().unwrap_or(false),
        allow_high: args["allow_high"].as_bool().unwrap_or(false),
        run_dir: None,
        // The same client that answers an elicitation for `run_task` can answer
        // one here; without this the only way to orchestrate would be to switch
        // confirmations off entirely.
        approver: approver_from_elicitor(),
    })
}

const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn serve() -> Result<()> {
    let server_elicitor = Elicitor::new();
    let _ = elicitor().set(server_elicitor.clone());

    // Read stdin on a dedicated blocking thread: tokio's stdin on Windows does
    // not reliably wake for messages that arrive after the first read while
    // the pipe stays open (verified with an MCP client: requests 1-2 answered,
    // request 3 hung until EOF).
    //
    // Responses to server-initiated elicitation are dispatched straight to the
    // waiting oneshot from this thread; routing them through the request loop
    // would deadlock while a tool call awaits approval.
    let pending = server_elicitor.pending.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("method").is_some() {
                if tx.send(value).is_err() {
                    break;
                }
            } else if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                if let Some(sender) = pending.lock().expect("pending lock").remove(id) {
                    let _ = sender.send(value);
                }
            }
        }
    });

    while let Some(request) = rx.recv().await {
        if request["method"].as_str() == Some("shutdown") {
            if let Some(id) = request.get("id").cloned() {
                write_msg(&success_response(id, json!({})))?;
            }
            break;
        }

        if let Some(response) = handle_request(request).await {
            write_msg(&response)?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC request. Returns `None` for notifications.
pub async fn handle_request(request: Value) -> Option<Value> {
    // Notifications carry no id and expect no response.
    let id = request.get("id").cloned()?;
    let method = request["method"].as_str().unwrap_or_default();
    tracing::debug!(method, "mcp request");

    Some(match method {
        "initialize" => success_response(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "cua-hub",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Tools to see and control this desktop. Prefer accessibility_tree + invoke_control for precise work; use run_task for natural-language goals. Read a window with window_state, then act on the element_index values it returned. Several deterministic actions can be sent at once with then:[{\"do\":\"<tool>\"}]: the chain stops at the first failure and returns one trace plus one fresh observation instead of a round trip per action."
            }),
        ),
        "ping" => success_response(id, json!({})),
        "tools/list" => success_response(id, json!({ "tools": tool_defs() })),
        "tools/call" => {
            let params = &request["params"];
            let tool = params["name"].as_str().unwrap_or_default();
            tracing::debug!(tool, "tool call");
            let result = call_tool(tool, &params["arguments"]).await;
            success_response(id, result)
        }
        other => error_response(id, -32601, &format!("method not found: {other}")),
    })
}

/// Serve MCP over HTTP: POST JSON-RPC to `/mcp`, `GET /health` for probes.
pub async fn serve_http(bind: &str) -> Result<()> {
    // There is no authentication: a remote bind would let anyone drive this
    // desktop. Require an explicit opt-in for non-loopback addresses.
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let local = matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1");
    let token = std::env::var("CUA_HUB_MCP_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    if !local && token.is_none() && std::env::var("CUA_HUB_ALLOW_REMOTE").as_deref() != Ok("1") {
        anyhow::bail!(
            "refusing to bind MCP HTTP on {host} without CUA_HUB_MCP_TOKEN; set CUA_HUB_ALLOW_REMOTE=1 only if you front it with your own auth"
        );
    }

    let notice = unauthenticated_remote_notice(bind, local, &token);

    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };

    async fn rpc(
        State(token): State<Option<String>>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> axum::response::Response {
        if let Some(expected) = token {
            let presented = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if presented != format!("Bearer {expected}") {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "missing or invalid bearer token" })),
                )
                    .into_response();
            }
        }
        match handle_request(request).await {
            Some(response) => (StatusCode::OK, Json(response)).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        }
    }

    let app = Router::new()
        .route("/mcp", post(rpc))
        .route("/health", get(|| async { "ok" }))
        .with_state(token);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {bind}: {e}"))?;
    eprintln!("MCP HTTP listening on http://{bind}/mcp");
    if let Some(notice) = notice {
        // After the bind succeeded, so a port that never opened is not announced as
        // one. Refusing to bind was already loud; getting through the opt-in was not.
        eprint!("{notice}");
        tracing::warn!(
            bind,
            "MCP HTTP bound to a non-loopback address with no bearer token: unauthenticated remote control of this desktop"
        );
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("http server: {e}"))?;
    Ok(())
}

fn write_msg(value: &Value) -> Result<()> {
    let text = serde_json::to_string(value)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_ok(value: impl Into<Value>) -> Value {
    let value: Value = value.into();
    let mut content = Vec::new();
    if let Some(parts) = value.as_array() {
        content.extend(parts.iter().cloned());
    } else if let Some(text) = value.as_str() {
        content.push(json!({ "type": "text", "text": text }));
    } else {
        content.push(json!({ "type": "text", "text": value.to_string() }));
    }
    json!({ "content": content, "isError": false })
}

fn tool_err(message: impl std::fmt::Display) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.to_string() }],
        "isError": true
    })
}

/// Reject coordinates outside the primary display before injecting input.
fn ensure_on_screen(x: i64, y: i64) -> std::result::Result<(), Value> {
    let screen = cua_io::capture_primary().map_err(tool_err)?;
    if x < 0 || y < 0 || x >= screen.width as i64 || y >= screen.height as i64 {
        return Err(tool_err(format!(
            "point ({x},{y}) is outside the screen ({}x{})",
            screen.width, screen.height
        )));
    }
    Ok(())
}

/// Traversal budget for a target-addressed action, so a caller can widen it and
/// -- more usefully -- narrow it in a test. A walk that runs out answers
/// "did not reach", which must never be read as "this control has no pattern".
fn action_limits(args: &Value) -> (usize, usize) {
    (
        args["max_nodes"]
            .as_u64()
            .map(|n| n.clamp(1, 100_000) as usize)
            .unwrap_or(cua_engine_access::DEFAULT_ACTION_NODES),
        args["max_depth"]
            .as_u64()
            .map(|d| d.clamp(1, 64) as usize)
            .unwrap_or(cua_engine_access::DEFAULT_ACTION_DEPTH),
    )
}

/// Phrase what an action visibly did, from the state read back off the control.
///
/// `unchanged` is not "failed": the audit's panel button answered the pattern and
/// opened nothing, and a caller has to be able to see that difference without
/// taking a screenshot. `unverifiable` says the readback was unavailable, which is
/// a statement about this platform, not about the action.
fn effect_note(report: &cua_engine_access::ActionReport, text_write: bool) -> String {
    let mut readback = Vec::new();
    if let Some(state) = &report.state {
        if let Some(toggle) = &state.toggle {
            readback.push(format!("toggle={toggle}"));
        }
        if let Some(expanded) = &state.expanded {
            readback.push(format!("expanded={expanded}"));
        }
        if let Some(value) = &state.value {
            let shown: String = value.chars().take(40).collect();
            readback.push(format!("value={shown:?}"));
        }
    }
    if readback.is_empty() {
        readback.push(if text_write {
            "this platform cannot read the field's text back".to_string()
        } else {
            "no readable state on this control".to_string()
        });
    }
    format!(
        "; effect={} ({})",
        report.effect.as_str(),
        readback.join(" ")
    )
}

/// Where a window-relative offset actually lands, or why it is refused.
///
/// `window_input` documents that its `x`/`y` aim inside the named window, and the
/// numbers come from the model. Without a containment check an offset is a click
/// anywhere on the desktop -- while the answer still reports the window it was
/// "aimed at". Arithmetic stays in i64 because `as i32` would wrap a huge value
/// straight back into the window's own neighbourhood, which is the failure this
/// prevents rather than the one it looks like.
fn window_relative_point(
    window: &cua_io::WindowInfo,
    x: Option<i64>,
    y: Option<i64>,
) -> std::result::Result<Option<(i32, i32)>, Value> {
    let (x, y) = match (x, y) {
        (None, None) => return Ok(None),
        (Some(_), None) | (None, Some(_)) => {
            return Err(tool_err(json!({
                "ok": false,
                "error": {
                    "code": "half_a_point",
                    "message": "window-relative input needs both x and y; one alone is not a point",
                    "retry": true,
                }
            })));
        }
        (Some(x), Some(y)) => (x, y),
    };
    if x < 0 || y < 0 || x >= window.width as i64 || y >= window.height as i64 {
        return Err(tool_err(json!({
            "ok": false,
            "error": {
                "code": "offset_outside_window",
                "message": format!(
                    "window-relative point ({x},{y}) is outside {} {:?} ({}x{}); this tool never clicks outside the window it names",
                    window.app, window.title, window.width, window.height
                ),
                "window": {
                    "x": window.x,
                    "y": window.y,
                    "width": window.width,
                    "height": window.height,
                },
                "hint": "aim inside the window, or use click with absolute screen pixels knowingly",
                "retry": true,
            }
        })));
    }
    let absolute_x = window.x as i64 + x;
    let absolute_y = window.y as i64 + y;
    match (i32::try_from(absolute_x), i32::try_from(absolute_y)) {
        (Ok(px), Ok(py)) => Ok(Some((px, py))),
        _ => Err(tool_err(format!(
            "window-relative point ({x},{y}) resolves to ({absolute_x},{absolute_y}), outside the coordinate range this desktop can address"
        ))),
    }
}

/// The centre of a control rectangle, or `None` when the rectangle is degenerate.
///
/// A toolkit answers `(0,0,0,0)` for a control it has never laid out, and the
/// "centre" of that is the top-left pixel of the screen. Every fallback click goes
/// through here so the check cannot be forgotten by the next call site.
fn control_center(rect: (i32, i32, i32, i32)) -> Option<(i32, i32)> {
    let (x, y, w, h) = rect;
    (w > 0 && h > 0).then(|| (x + w / 2, y + h / 2))
}

/// What to print when the HTTP endpoint will accept callers it cannot authenticate.
///
/// Refusing to bind was loud; passing the `CUA_HUB_ALLOW_REMOTE=1` opt-in was not,
/// so an operator who set it for a quick test never learned that the open port is an
/// unauthenticated remote-control desk for this machine -- `run_task` with the code
/// engine included.
fn unauthenticated_remote_notice(
    bind: &str,
    local: bool,
    token: &Option<String>,
) -> Option<String> {
    if local || token.is_some() {
        return None;
    }
    Some(format!(
        "\n==================================================================\n\
         SECURITY: MCP HTTP is listening on {bind} with NO authentication.\n\
         Anyone who can reach this port can drive this desktop -- screenshot,\n\
         click, type, and run_task (which can execute commands on this machine).\n\
         Set CUA_HUB_MCP_TOKEN to require a bearer token, or bind to 127.0.0.1\n\
         and front it with your own authenticated proxy. You opted in with\n\
         CUA_HUB_ALLOW_REMOTE=1; unset it to make a non-loopback bind refuse.\n\
         ==================================================================\n"
    ))
}

fn required_str(args: &Value, key: &str) -> std::result::Result<String, Value> {
    args[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| tool_err(format!("missing string argument `{key}`")))
}

/// The control a name-addressed tool means: a `target` object, or the `name`
/// shorthand every caller already used.
///
/// Both at once is refused. That pair is the double address whose silent
/// resolution this stage exists to remove -- one of the two would have to win,
/// and the caller would never learn which.
fn target_arg(args: &Value) -> std::result::Result<cua_engine_access::target::Target, Value> {
    let target = args.get("target").filter(|t| !t.is_null());
    let name = args.get("name").filter(|n| !n.is_null());
    match (target, name) {
        (Some(target), None) => serde_json::from_value(target.clone()).map_err(|e| {
            tool_err(format!(
                "invalid `target`: {e}. It takes name (substring, `re:` pattern, or `id:<AutomationId>`), and optionally role, nth (0-based), container"
            ))
        }),
        (None, Some(name)) => name
            .as_str()
            .map(cua_engine_access::target::Target::by_name)
            .ok_or_else(|| tool_err("`name` must be a string")),
        (Some(_), Some(_)) => Err(tool_err(
            "give either `target` or `name`, not both: two addresses for one control is the ambiguity this tool refuses",
        )),
        (None, None) => Err(tool_err(
            "missing `target`: {\"name\": \"...\", optional role/nth/container}; the `name` shorthand still works for a bare name",
        )),
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Metadata that lets a caller (or a later action) place this image back on the
/// desktop. The image and the rectangle come from the same call, so the
/// coordinate space is self-describing rather than something the model has to
/// reconstruct from DPI.
fn window_capture_meta(cap: &cua_io::WindowCapture) -> Value {
    let mut meta = json!({
        "id": cap.info.id,
        "pid": cap.info.pid,
        "app": cap.info.app,
        "title": cap.info.title,
        "origin": { "x": cap.info.x, "y": cap.info.y },
        "window_size": { "width": cap.info.width, "height": cap.info.height },
        "model_size": {
            "width": cap.frame.observation.width,
            "height": cap.frame.observation.height
        },
        "physical": { "width": cap.frame.physical.0, "height": cap.frame.physical.1 },
        "image_scale": cap.frame.scale,
        "dpi": cap.frame.dpi,
        "source": format!("{:?}", cap.source),
        "hash": cap.hash,
        "mapping": "screen = origin + image * image_scale",
    });
    // When the compositor backend was tried and refused, say which stage refused:
    // `source` alone tells the caller a fallback happened, not why or what to fix.
    if let Some(stage) = &cap.backend_error {
        meta["backend_error"] = json!({ "backend": "GraphicsCapture", "stage": stage });
    }
    meta
}

/// Result of resolving a window selector against the live window list.
enum WindowPick<'a> {
    One(&'a cua_io::WindowInfo),
    None,
    Many(Vec<&'a cua_io::WindowInfo>),
}

/// Exact title/app matches win over substrings; a selector never guesses when
/// it matches more than one window.
fn pick_window<'a>(windows: &'a [cua_io::WindowInfo], selector: &str) -> WindowPick<'a> {
    let exact: Vec<&cua_io::WindowInfo> = windows
        .iter()
        .filter(|w| w.title.eq_ignore_ascii_case(selector) || w.app.eq_ignore_ascii_case(selector))
        .collect();
    if exact.len() == 1 {
        return WindowPick::One(exact[0]);
    }
    if exact.len() > 1 {
        return WindowPick::Many(exact);
    }
    let needle = selector.to_lowercase();
    let fuzzy: Vec<&cua_io::WindowInfo> = windows
        .iter()
        .filter(|w| {
            w.title.to_lowercase().contains(&needle) || w.app.to_lowercase().contains(&needle)
        })
        .collect();
    match fuzzy.len() {
        0 => WindowPick::None,
        1 => WindowPick::One(fuzzy[0]),
        _ => WindowPick::Many(fuzzy),
    }
}

fn window_brief(w: &cua_io::WindowInfo) -> Value {
    json!({
        "id": w.id,
        "pid": w.pid,
        "app": w.app,
        "title": w.title,
        "x": w.x,
        "y": w.y,
        "width": w.width,
        "height": w.height,
        "minimized": w.minimized,
        "focused": w.focused,
    })
}

fn window_arg_id(value: &Value) -> Option<u32> {
    if let Some(id) = value.as_u64() {
        return u32::try_from(id).ok();
    }
    if let Some(id) = value.as_str() {
        return id.parse::<u32>().ok();
    }
    let id = value.get("id")?;
    if let Some(id) = id.as_u64() {
        return u32::try_from(id).ok();
    }
    id.as_str()?.parse::<u32>().ok()
}

/// How many same-process popups (menus, dialogs, tooltips) an observation also
/// watches. A popup is its own window, so watching only the target window
/// reports "nothing changed" when a menu opens.
const MAX_RELATED: usize = 3;

/// Node budget for one popup's tree. Popups are small; the cap only protects
/// against a pathological owner that spawns a huge owned window.
const MAX_POPUP_NODES: usize = 120;

/// True when a popup's area is already represented in the observed tree, i.e.
/// one node covers at least 60% of it. Menus and their shadows come back as
/// separate HWNDs a few pixels larger than the UIA node for the same content,
/// so exact rectangle equality is not enough to avoid duplicating them.
fn covered_by(nodes: &[(i32, i32, i32, i32)], popup: (i32, i32, i32, i32)) -> bool {
    let (px, py, pw, ph) = popup;
    let area = pw as i64 * ph as i64;
    if area <= 0 {
        return true;
    }
    nodes.iter().any(|(x, y, w, h)| {
        let left = (*x).max(px);
        let top = (*y).max(py);
        let right = (*x + *w).min(px + pw);
        let bottom = (*y + *h).min(py + ph);
        if right <= left || bottom <= top {
            return false;
        }
        (right - left) as i64 * (bottom - top) as i64 * 10 >= area * 6
    })
}

/// Combined hash of a window and the popups its process currently owns.
fn window_state_hash(window: &cua_io::WindowInfo) -> std::result::Result<u64, Value> {
    let cap = cua_io::capture_window(&window.id.to_string(), 640, 60).map_err(tool_err)?;
    let mut composite = cap.hash;
    if let Ok(related) = cua_io::related_windows(window.pid, window.id, MAX_RELATED) {
        for rel in related {
            if let Ok(rel_cap) = cua_io::capture_window(&rel.id.to_string(), 640, 60) {
                composite = composite.wrapping_mul(31).wrapping_add(rel_cap.hash);
            }
        }
    }
    Ok(composite)
}

/// Result of attempting to snapshot a window's accessibility tree with a
/// single automatic recovery (rebuild client / reset wedged cooldown + refocus
/// window) when the initial read returns an error or an empty node list.
#[derive(Debug)]
enum SnapshotAttempt {
    Available {
        snapshot: cua_engine_access::UiSnapshot,
        retried: bool,
    },
    CoordinateOnly {
        retried: bool,
        reason: String,
    },
}

fn snapshot_with_retry<F, R>(mut fetch: F, mut recover: R) -> SnapshotAttempt
where
    F: FnMut() -> cua_core::Result<cua_engine_access::UiSnapshot>,
    R: FnMut(),
{
    let first_reason = match fetch() {
        Ok(snap) if !snap.nodes.is_empty() => {
            return SnapshotAttempt::Available {
                snapshot: snap,
                retried: false,
            };
        }
        Ok(_) => "accessibility snapshot returned 0 nodes".to_string(),
        Err(e) => e.to_string(),
    };

    recover();

    match fetch() {
        Ok(snap) if !snap.nodes.is_empty() => SnapshotAttempt::Available {
            snapshot: snap,
            retried: true,
        },
        Ok(_) => SnapshotAttempt::CoordinateOnly {
            retried: true,
            reason: format!("{first_reason}; retry also returned 0 nodes"),
        },
        Err(e) => SnapshotAttempt::CoordinateOnly {
            retried: true,
            reason: format!("{first_reason}; retry failed: {e}"),
        },
    }
}

fn coordinate_only_accessibility(retried: bool, reason: &str) -> Value {
    let notice = if retried {
        format!(
            "accessibility tree unavailable after 1 retry (rebuilt client and refocused window: {reason}); degraded to coordinate-only mode"
        )
    } else {
        format!("accessibility tree disabled ({reason}); operating in coordinate-only mode")
    };
    json!({
        "mode": "coordinate-only",
        "available": false,
        "retried": retried,
        "node_count": 0,
        "reason": reason,
        "notice": notice,
    })
}

fn related_title_summaries(related: &[cua_io::WindowInfo]) -> Vec<Value> {
    related
        .iter()
        .map(|rel| {
            json!({
                "id": rel.id,
                "title": rel.title,
            })
        })
        .collect()
}

fn append_related_titles_only(mut tree: String, related: &[cua_io::WindowInfo]) -> String {
    for rel in related {
        if !tree.is_empty() && !tree.ends_with('\n') {
            tree.push('\n');
        }
        tree.push_str(&format!("# related window {} \"{}\"\n", rel.id, rel.title));
    }
    tree
}

/// Default consecutive failures allowed on the same question before stopping.
const DEFAULT_MAX_QUESTION_RETRIES: usize = 3;

/// Default pixel padding around a target rectangle when cropping an
/// `unverifiable` verification screenshot.
const TARGET_CROP_MARGIN_PX: i32 = 48;

static QUESTION_FAILURE_COUNTS: std::sync::Mutex<Option<std::collections::HashMap<String, usize>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn reset_question_retries() {
    if let Ok(mut guard) = QUESTION_FAILURE_COUNTS.lock() {
        if let Some(map) = guard.as_mut() {
            map.clear();
        }
    }
}

fn extract_question_id(args: &Value) -> Option<String> {
    for key in ["question", "question_id", "question_number"] {
        if let Some(s) = args[key].as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(n) = args[key].as_u64() {
            return Some(n.to_string());
        }
    }
    None
}

fn max_retries_from_args(args: &Value) -> usize {
    args["max_retries"]
        .as_u64()
        .map(|n| n.clamp(1, 20) as usize)
        .unwrap_or(DEFAULT_MAX_QUESTION_RETRIES)
}

fn retry_limit_error(question: &str, attempts: usize, max_retries: usize) -> Value {
    tool_err(json!({
        "ok": false,
        "error": {
            "code": "retry_limit_exceeded",
            "question": question,
            "attempts": attempts,
            "max_retries": max_retries,
            "message": format!(
                "question {question} failed {attempts} consecutive times (limit {max_retries}); stopping at question {question} instead of blindly continuing or skipping"
            ),
            "retry": false,
        }
    }))
}

fn check_question_retry_budget(
    question: &str,
    max_retries: usize,
) -> std::result::Result<(), Value> {
    let attempts = QUESTION_FAILURE_COUNTS
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(question).copied()))
        .unwrap_or(0);
    if attempts >= max_retries {
        return Err(retry_limit_error(question, attempts, max_retries));
    }
    Ok(())
}

fn record_question_attempt(
    question: &str,
    succeeded: bool,
    max_retries: usize,
) -> std::result::Result<usize, Value> {
    let mut guard = QUESTION_FAILURE_COUNTS
        .lock()
        .map_err(|_| tool_err("question retry lock poisoned"))?;
    let map = guard.get_or_insert_with(Default::default);
    if succeeded {
        map.remove(question);
        return Ok(0);
    }
    let count = map.entry(question.to_string()).or_insert(0);
    *count += 1;
    let current = *count;
    if current >= max_retries {
        return Err(retry_limit_error(question, current, max_retries));
    }
    Ok(current)
}

fn action_result_succeeded(result: &Value) -> bool {
    if result["isError"].as_bool().unwrap_or(false) {
        return false;
    }
    if let Some(parts) = result["content"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                if text.contains("effect=unchanged") {
                    return false;
                }
            }
        }
    }
    true
}

/// Only `Effect::Unverifiable` warrants a verification screenshot; `Changed`
/// and `Unchanged` are already decided from accessibility state readback.
fn should_capture_verification_crop(effect: cua_engine_access::Effect) -> bool {
    matches!(effect, cua_engine_access::Effect::Unverifiable)
}

/// Compute a small bounding box around `target_rect` (in physical screen pixels)
/// within a captured surface at `screen_origin` of size `screen_size`. Never
/// returns a full-screen region when the target is a control.
fn compute_target_crop_bounds(
    screen_origin: (i32, i32),
    screen_size: (u32, u32),
    target_rect: (i32, i32, i32, i32),
    margin: i32,
) -> Option<(u32, u32, u32, u32)> {
    let (tx, ty, tw, th) = target_rect;
    if tw <= 0 || th <= 0 || screen_size.0 == 0 || screen_size.1 == 0 {
        return None;
    }
    let rel_x = tx - screen_origin.0;
    let rel_y = ty - screen_origin.1;
    let pad = margin.max(0);
    let left = (rel_x - pad).clamp(0, screen_size.0 as i32);
    let top = (rel_y - pad).clamp(0, screen_size.1 as i32);
    let right = (rel_x + tw + pad).clamp(0, screen_size.0 as i32);
    let bottom = (rel_y + th + pad).clamp(0, screen_size.1 as i32);
    if right <= left || bottom <= top {
        return None;
    }
    Some((
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

/// Extract a sub-rectangle `(cx, cy, cw, ch)` from an RGBA8 image buffer.
fn crop_rgba_region(
    rgba: &[u8],
    img_w: u32,
    img_h: u32,
    crop: (u32, u32, u32, u32),
) -> Option<Vec<u8>> {
    let (cx, cy, cw, ch) = crop;
    if cw == 0
        || ch == 0
        || cx.checked_add(cw)? > img_w
        || cy.checked_add(ch)? > img_h
        || rgba.len() < (img_w as usize) * (img_h as usize) * 4
    {
        return None;
    }
    let mut out = Vec::with_capacity((cw as usize) * (ch as usize) * 4);
    let stride = (img_w as usize) * 4;
    let row_bytes = (cw as usize) * 4;
    for row in 0..ch {
        let start = ((cy + row) as usize) * stride + (cx as usize) * 4;
        out.extend_from_slice(&rgba[start..start + row_bytes]);
    }
    Some(out)
}

fn crc32_png(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn adler32_zlib(bytes: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in bytes {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn write_png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let crc_start = out.len();
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let crc = crc32_png(&out[crc_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode a small RGBA8 buffer as a valid PNG image using stored deflate blocks.
fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw_scanlines = Vec::with_capacity((height as usize) * (1 + (width as usize) * 4));
    let row_bytes = (width as usize) * 4;
    for y in 0..(height as usize) {
        raw_scanlines.push(0u8); // filter type 0 (None)
        let start = y * row_bytes;
        raw_scanlines.extend_from_slice(&rgba[start..start + row_bytes]);
    }

    let mut zlib = Vec::with_capacity(raw_scanlines.len() + 64);
    zlib.extend_from_slice(&[0x78, 0x01]); // zlib header (CM=8, CINFO=7, no preset dict)
    let chunks: Vec<&[u8]> = if raw_scanlines.is_empty() {
        vec![&[][..]]
    } else {
        raw_scanlines.chunks(65_535).collect()
    };
    for (idx, block) in chunks.iter().enumerate() {
        let is_last = idx + 1 == chunks.len();
        zlib.push(if is_last { 0x01 } else { 0x00 });
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32_zlib(&raw_scanlines).to_be_bytes());

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    write_png_chunk(&mut png, b"IHDR", &ihdr);
    write_png_chunk(&mut png, b"IDAT", &zlib);
    write_png_chunk(&mut png, b"IEND", &[]);
    png
}

/// Build the MCP tool response for an accessibility action report:
/// - For `Changed` and `Unchanged`: returns text only (never captures a screenshot).
/// - For `Unverifiable`: captures ONLY a small cropped neighborhood around the
///   target control's rectangle (never a full-screen screenshot).
fn action_outcome_with_verification_crop(
    message: String,
    report: &cua_engine_access::ActionReport,
) -> Value {
    if !should_capture_verification_crop(report.effect) {
        return tool_ok(message);
    }
    if let Ok(screen) = cua_io::capture_primary() {
        if let Some(crop) = compute_target_crop_bounds(
            screen.origin,
            (screen.width, screen.height),
            report.node.rect,
            TARGET_CROP_MARGIN_PX,
        ) {
            if let Some(cropped_rgba) =
                crop_rgba_region(screen.image.as_raw(), screen.width, screen.height, crop)
            {
                let png = encode_rgba_png(crop.2, crop.3, &cropped_rgba);
                let crop_meta = json!({
                    "text": message,
                    "effect": "unverifiable",
                    "verification_crop": {
                        "x": screen.origin.0 + crop.0 as i32,
                        "y": screen.origin.1 + crop.1 as i32,
                        "width": crop.2,
                        "height": crop.3,
                        "full_screen": false,
                    }
                });
                return tool_ok(json!([
                    {
                        "type": "image",
                        "data": encode_base64(&png),
                        "mimeType": "image/png",
                    },
                    {
                        "type": "text",
                        "text": crop_meta.to_string(),
                    }
                ]));
            }
        }
    }
    tool_ok(message)
}

#[derive(Debug, Clone)]
struct SubmissionAudit {
    total_questions: usize,
    answered_count: usize,
    missing_questions: Vec<String>,
    differences: Vec<Value>,
    checked_controls: Vec<String>,
    filled_controls: Vec<String>,
    passed: bool,
}

fn normalize_answer_tokens(value: &Value) -> Vec<String> {
    match value {
        Value::Null => Vec::new(),
        Value::Bool(b) => vec![b.to_string()],
        Value::Number(n) => vec![n.to_string()],
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }
        }
        Value::Array(arr) => {
            let mut out: Vec<String> = arr
                .iter()
                .flat_map(normalize_answer_tokens)
                .filter(|s| !s.is_empty())
                .collect();
            out.sort();
            out.dedup();
            out
        }
        _ => Vec::new(),
    }
}

fn infer_question_key(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed
        .strip_prefix("第")
        .and_then(|s| s.split("题").next())
    {
        let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["question ", "question_", "question-", "q", "题"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
    }
    let leading_digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !leading_digits.is_empty() {
        let after = &trimmed[leading_digits.len()..];
        if after.starts_with('.')
            || after.starts_with('、')
            || after.starts_with(')')
            || after.starts_with('）')
            || after.starts_with(':')
        {
            return Some(leading_digits);
        }
    }
    None
}

fn extract_node_answer(node: &cua_engine_access::UiNode) -> Option<String> {
    if node.checked == Some(true) || node.selected {
        if !node.name.trim().is_empty() {
            return Some(node.name.trim().to_string());
        }
        if !node.value.trim().is_empty() {
            return Some(node.value.trim().to_string());
        }
        return Some("true".to_string());
    }
    if matches!(
        node.control_type.as_str(),
        "Edit" | "Document" | "ComboBox" | "Entry"
    ) && !node.value.trim().is_empty()
    {
        return Some(node.value.trim().to_string());
    }
    None
}

fn answers_match(expected: &[String], actual: &[String]) -> bool {
    if expected.is_empty() || actual.is_empty() || expected.len() != actual.len() {
        return false;
    }
    let mut exp_norm: Vec<String> = expected.iter().map(|s| s.trim().to_lowercase()).collect();
    let mut act_norm: Vec<String> = actual.iter().map(|s| s.trim().to_lowercase()).collect();
    exp_norm.sort();
    act_norm.sort();
    exp_norm
        .iter()
        .zip(act_norm.iter())
        .all(|(e, a)| e == a || a.starts_with(&format!("{e}.")) || a.starts_with(&format!("{e} ")))
}

/// Audit the current page's accessibility nodes against `planned_answers`:
/// 1) Full-page check: compares every selected/checked option and filled input
///    against the planned answers and lists all discrepancies in `differences`.
/// 2) Coverage check: counts `answered_count` vs `total_questions` and lists any
///    unanswered question numbers in `missing_questions`.
fn audit_submission_nodes(
    nodes: &[cua_engine_access::UiNode],
    planned_answers: &Value,
    total_questions_override: Option<usize>,
) -> SubmissionAudit {
    let mut checked_controls = Vec::new();
    let mut filled_controls = Vec::new();
    let mut by_question: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut current_question: Option<String> = None;

    for node in nodes {
        if let Some(q) =
            infer_question_key(&node.automation_id).or_else(|| infer_question_key(&node.name))
        {
            current_question = Some(q);
        }
        if node.checked == Some(true) || node.selected {
            checked_controls.push(format!(
                "[{}] {} {:?}",
                node.index, node.control_type, node.name
            ));
        } else if !node.value.trim().is_empty() {
            filled_controls.push(format!(
                "[{}] {} {:?} value={:?}",
                node.index, node.control_type, node.name, node.value
            ));
        }
        if let Some(ans) = extract_node_answer(node) {
            if let Some(q) = current_question.clone() {
                let entry = by_question.entry(q).or_default();
                if !entry.contains(&ans) {
                    entry.push(ans);
                }
            }
        }
    }

    struct PlannedItem {
        question: String,
        expected: Vec<String>,
        explicit_actual: Option<Vec<String>>,
        element_index: Option<usize>,
    }

    let mut planned: Vec<PlannedItem> = Vec::new();
    if let Some(arr) = planned_answers.as_array() {
        for (idx, item) in arr.iter().enumerate() {
            let q = item["question"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .or_else(|| item["number"].as_str())
                .map(str::to_string)
                .or_else(|| {
                    item["question"]
                        .as_u64()
                        .or_else(|| item["id"].as_u64())
                        .or_else(|| item["number"].as_u64())
                        .map(|n| n.to_string())
                })
                .unwrap_or_else(|| (idx + 1).to_string());
            let expected_val = item
                .get("expected")
                .or_else(|| item.get("answer"))
                .or_else(|| item.get("value"))
                .unwrap_or(&Value::Null);
            let expected = normalize_answer_tokens(expected_val);
            let explicit_actual = item.get("actual").map(normalize_answer_tokens);
            let element_index = item["element_index"]
                .as_u64()
                .or_else(|| item["index"].as_u64())
                .map(|n| n as usize);
            planned.push(PlannedItem {
                question: q,
                expected,
                explicit_actual,
                element_index,
            });
        }
    } else if let Some(map) = planned_answers.as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_by_key(|k| (k.parse::<usize>().unwrap_or(usize::MAX), (*k).clone()));
        for key in keys {
            planned.push(PlannedItem {
                question: key.clone(),
                expected: normalize_answer_tokens(&map[key]),
                explicit_actual: None,
                element_index: None,
            });
        }
    }

    let total_questions = total_questions_override
        .unwrap_or(planned.len())
        .max(planned.len());
    let mut missing_questions = Vec::new();
    let mut differences = Vec::new();
    let mut answered_count = 0usize;
    let mut seen_questions = std::collections::HashSet::new();

    for item in &planned {
        seen_questions.insert(item.question.clone());
        let actual: Vec<String> = if let Some(explicit) = &item.explicit_actual {
            explicit.clone()
        } else if let Some(index) = item.element_index {
            nodes
                .iter()
                .find(|n| n.index == index)
                .and_then(extract_node_answer)
                .into_iter()
                .collect()
        } else if let Some(found) = by_question.get(&item.question) {
            found.clone()
        } else if by_question.is_empty() {
            // Fallback when the page has no question headers: check if any active control matches
            nodes
                .iter()
                .filter_map(extract_node_answer)
                .filter(|ans| answers_match(&item.expected, std::slice::from_ref(ans)))
                .collect()
        } else {
            Vec::new()
        };

        if item.expected.is_empty() || actual.is_empty() {
            missing_questions.push(item.question.clone());
            differences.push(json!({
                "question": item.question,
                "expected": item.expected,
                "actual": actual,
                "reason": "unanswered",
            }));
        } else {
            answered_count += 1;
            if !answers_match(&item.expected, &actual) {
                differences.push(json!({
                    "question": item.question,
                    "expected": item.expected,
                    "actual": actual,
                    "reason": "mismatch",
                }));
            }
        }
    }

    for q_num in 1..=total_questions {
        let key = q_num.to_string();
        if !seen_questions.contains(&key) {
            if let Some(actual) = by_question.get(&key).filter(|v| !v.is_empty()) {
                let _ = actual;
                answered_count += 1;
            } else {
                missing_questions.push(key);
            }
        }
    }

    let passed =
        missing_questions.is_empty() && differences.is_empty() && answered_count >= total_questions;

    SubmissionAudit {
        total_questions,
        answered_count,
        missing_questions,
        differences,
        checked_controls,
        filled_controls,
        passed,
    }
}

fn submission_audit_to_json(audit: &SubmissionAudit) -> Value {
    json!({
        "ok": audit.passed,
        "can_submit": audit.passed,
        "coverage": {
            "answered": audit.answered_count,
            "total": audit.total_questions,
            "missing_questions": audit.missing_questions,
        },
        "differences": audit.differences,
        "checked_controls": audit.checked_controls,
        "filled_controls": audit.filled_controls,
    })
}

fn enforce_submission_gate(args: &Value) -> std::result::Result<Option<Value>, Value> {
    let Some(planned) = args
        .get("planned_answers")
        .or_else(|| args.get("expected_answers"))
        .filter(|v| !v.is_null())
    else {
        return Ok(None);
    };
    let total_override = args["total_questions"].as_u64().map(|n| n as usize);
    let nodes: Vec<cua_engine_access::UiNode> = if let Some(raw_nodes) = args.get("nodes") {
        serde_json::from_value(raw_nodes.clone()).unwrap_or_default()
    } else if !args["window"].is_null() {
        let win = resolve_window_arg(&args["window"])?;
        cua_engine_access::snapshot_window(win.id, &win.app, &win.title, win.pid, 600, 16)
            .map(|s| s.nodes)
            .unwrap_or_default()
    } else if let Some(obs_id) = args["observation_id"].as_str() {
        let Some(obs) = observations::get(obs_id) else {
            return Err(stale_err(format!(
                "observation `{obs_id}` is no longer held; re-run window_state before submitting"
            )));
        };
        obs.nodes
    } else {
        cua_engine_access::snapshot(600, 12)
            .map(|s| s.nodes)
            .unwrap_or_default()
    };

    let audit = audit_submission_nodes(&nodes, planned, total_override);
    let payload = submission_audit_to_json(&audit);
    if !audit.passed {
        return Err(tool_err(json!({
            "ok": false,
            "error": {
                "code": "submission_quality_gate_failed",
                "message": format!(
                    "submission blocked by quality gate: answered {}/{} questions, missing {:?}, differences: {}",
                    audit.answered_count,
                    audit.total_questions,
                    audit.missing_questions,
                    audit.differences.len()
                ),
                "audit": payload,
                "retry": false,
            }
        })));
    }
    Ok(Some(payload))
}

/// An index or pixel that came from an observation the server no longer holds,
/// or that does not exist in it. Treated as retryable but never silently
/// re-resolved: the caller must re-observe and choose again.
fn stale_err(message: impl std::fmt::Display) -> Value {
    tool_err(json!({
        "ok": false,
        "error": {
            "code": "stale_observation",
            "message": message.to_string(),
            "refetch": true,
            "retry": true,
        }
    }))
}

fn target_error(
    code: &str,
    message: impl std::fmt::Display,
    candidates: &[&cua_io::WindowInfo],
    refetch: bool,
) -> Value {
    let candidates: Vec<Value> = candidates.iter().take(8).map(|w| window_brief(w)).collect();
    let payload = json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message.to_string(),
            "refetch": refetch,
            "retry": false,
            "candidates": candidates,
        }
    });
    tool_err(payload)
}

/// Accept a window id (number or numeric string), a `{app,id}` object, or a
/// title/app selector. A selector must match exactly one window; otherwise the
/// error carries candidates instead of silently typing into an unrelated app.
fn resolve_window_arg(value: &Value) -> std::result::Result<cua_io::WindowInfo, Value> {
    let windows = cua_io::list_windows(None, true).map_err(tool_err)?;

    if let Some(id) = window_arg_id(value) {
        let Some(found) = windows.iter().find(|w| w.id == id) else {
            let all: Vec<&cua_io::WindowInfo> = windows.iter().collect();
            return Err(target_error(
                "target_not_found",
                format!("window id {id} is no longer available"),
                &all,
                true,
            ));
        };
        if let Some(app) = value.get("app").and_then(Value::as_str) {
            if !app.trim().is_empty()
                && !found.app.eq_ignore_ascii_case(app)
                && !found.title.eq_ignore_ascii_case(app)
            {
                return Err(target_error(
                    "target_mismatch",
                    format!("window id {id} belongs to `{}`, not `{app}`", found.app),
                    &[found],
                    false,
                ));
            }
        }
        return Ok(found.clone());
    }

    let Some(selector) = value.as_str() else {
        return Err(tool_err(
            "`window` must be a window id, a title/app selector, or {app,id}",
        ));
    };
    if selector.trim().is_empty() {
        return Err(tool_err("`window` selector is empty"));
    }
    match pick_window(&windows, selector) {
        WindowPick::One(window) => Ok(window.clone()),
        WindowPick::None => {
            let all: Vec<&cua_io::WindowInfo> = windows.iter().collect();
            Err(target_error(
                "target_not_found",
                format!("no window matches `{selector}`"),
                &all,
                true,
            ))
        }
        WindowPick::Many(matches) => Err(target_error(
            "target_ambiguous",
            format!("`{selector}` matches {} windows", matches.len()),
            &matches,
            false,
        )),
    }
}

/// Turn an indexed accessibility node back into a selector understood by
/// `invoke` / `set_value`. AutomationId wins because names are often dynamic.
fn selector_for_node(node: &cua_engine_access::UiNode) -> Option<String> {
    if !node.automation_id.is_empty() {
        Some(format!("id:{}", node.automation_id))
    } else if !node.name.is_empty() {
        Some(node.name.clone())
    } else {
        None
    }
}

/// An indexed node resolved against the observation it was read from.
struct ResolvedNode {
    node: cua_engine_access::UiNode,
    /// The window the node lives in. Differs from the observed window for nodes
    /// that came from a popup's tree.
    node_window: Option<u32>,
    /// The window to activate before acting, if any. Activating the owner of an
    /// open menu dismisses the menu, so popup nodes carry `None` here.
    focus_window: Option<u32>,
    /// True when the pre-action foreground check already verified (or refocused
    /// and re-resolved) `focus_window`.
    already_focused: bool,
}

/// Outcome of verifying that the target window still owns the foreground before
/// an action executes. If another window stole focus, the target window is
/// refocused and its accessibility tree is re-parsed so the target control's
/// rectangle and state are freshly resolved.
#[derive(Debug, Clone)]
struct ForegroundVerification {
    node: cua_engine_access::UiNode,
    #[allow(dead_code)]
    was_foreground: bool,
    #[allow(dead_code)]
    refocused: bool,
    #[allow(dead_code)]
    reresolved: bool,
}

/// Re-resolve a previously observed `UiNode` inside a freshly parsed window
/// snapshot after refocusing. Matches by structural signature first, then by
/// `(control_type, automation_id)` or `(control_type, name)`.
fn reresolve_node_in_snapshot(
    nodes: &[cua_engine_access::UiNode],
    original: &cua_engine_access::UiNode,
) -> Option<cua_engine_access::UiNode> {
    let found = if !original.signature.is_empty() {
        nodes
            .iter()
            .find(|n| n.signature == original.signature)
            .or_else(|| {
                (!original.automation_id.is_empty()).then(|| {
                    nodes.iter().find(|n| {
                        n.control_type == original.control_type
                            && n.automation_id == original.automation_id
                    })
                })?
            })
            .or_else(|| {
                (!original.name.is_empty()).then(|| {
                    nodes.iter().find(|n| {
                        n.control_type == original.control_type && n.name == original.name
                    })
                })?
            })
    } else if !original.automation_id.is_empty() {
        nodes.iter().find(|n| {
            n.control_type == original.control_type && n.automation_id == original.automation_id
        })
    } else if !original.name.is_empty() {
        nodes
            .iter()
            .find(|n| n.control_type == original.control_type && n.name == original.name)
    } else {
        nodes
            .iter()
            .find(|n| n.index == original.index && n.control_type == original.control_type)
    };
    found.map(|n| {
        let mut updated = n.clone();
        updated.index = original.index;
        updated
    })
}

fn ensure_foreground_and_reresolve_with<IsFg, Focus, Snap>(
    window_id: u32,
    node: &cua_engine_access::UiNode,
    mut is_fg: IsFg,
    mut focus: Focus,
    mut snapshot: Snap,
) -> std::result::Result<ForegroundVerification, Value>
where
    IsFg: FnMut(u32) -> bool,
    Focus: FnMut(u32) -> cua_core::Result<()>,
    Snap: FnMut(u32) -> cua_core::Result<cua_engine_access::UiSnapshot>,
{
    if is_fg(window_id) {
        return Ok(ForegroundVerification {
            node: node.clone(),
            was_foreground: true,
            refocused: false,
            reresolved: false,
        });
    }
    focus(window_id).map_err(|e| {
        tool_err(json!({
            "ok": false,
            "error": {
                "code": "focus_failed",
                "message": e.to_string(),
                "retry": true,
            }
        }))
    })?;
    match snapshot(window_id) {
        Ok(fresh) if !fresh.nodes.is_empty() => {
            if let Some(updated) = reresolve_node_in_snapshot(&fresh.nodes, node) {
                Ok(ForegroundVerification {
                    node: updated,
                    was_foreground: false,
                    refocused: true,
                    reresolved: true,
                })
            } else {
                Err(stale_err(format!(
                    "window {window_id} was not in the foreground; after refocusing and re-parsing, control {} {:?} is no longer present — re-run window_state",
                    node.control_type, node.name
                )))
            }
        }
        _ => Ok(ForegroundVerification {
            node: node.clone(),
            was_foreground: false,
            refocused: true,
            reresolved: false,
        }),
    }
}

fn ensure_foreground_and_reresolve(
    window_id: u32,
    node: &cua_engine_access::UiNode,
) -> std::result::Result<ForegroundVerification, Value> {
    ensure_foreground_and_reresolve_with(
        window_id,
        node,
        cua_engine_access::is_foreground_window,
        |id| {
            cua_engine_access::focus_window(id)?;
            std::thread::sleep(std::time::Duration::from_millis(60));
            Ok(())
        },
        |id| {
            let (app, title, pid) = cua_io::list_windows(None, true)
                .ok()
                .and_then(|wins| {
                    wins.into_iter()
                        .find(|w| w.id == id)
                        .map(|w| (w.app, w.title, w.pid))
                })
                .unwrap_or_default();
            cua_engine_access::snapshot_window(id, &app, &title, pid, 400, 12)
        },
    )
}

/// Resolve an element index, preferring the observation it was read from so the
/// index cannot drift onto a different control. Falls back to a fresh desktop
/// snapshot only when the caller did not pass an observation at all.
///
/// Before returning a window-owned target, verifies that its window is still in
/// the foreground; if not, refocuses the window and re-resolves the target
/// control from a fresh accessibility snapshot.
fn resolve_indexed_node(args: &Value, index: usize, tool: &str) -> Result<ResolvedNode, Value> {
    if let Some(id) = args["observation_id"].as_str() {
        let Some(obs) = observations::get(id) else {
            return Err(stale_err(format!(
                "{tool} was given observation `{id}` which is no longer held; re-run window_state"
            )));
        };
        let Some((node, node_window)) = observations::node_at(&obs, index) else {
            return Err(stale_err(format!(
                "observation {id} has no element_index {index}; re-run window_state with include_text"
            )));
        };
        let focus_window = (node_window == obs.window_id).then_some(obs.window_id);
        if let Some(win_id) = focus_window {
            let verified = ensure_foreground_and_reresolve(win_id, &node)?;
            return Ok(ResolvedNode {
                node: verified.node,
                node_window: Some(node_window),
                focus_window,
                already_focused: true,
            });
        }
        return Ok(ResolvedNode {
            node,
            node_window: Some(node_window),
            focus_window,
            already_focused: false,
        });
    }
    match cua_engine_access::node_at(index, 400, 8) {
        Ok(Some(node)) => {
            let window = window_arg_id(&args["window"]);
            if let Some(win_id) = window {
                let verified = ensure_foreground_and_reresolve(win_id, &node)?;
                return Ok(ResolvedNode {
                    node: verified.node,
                    node_window: window,
                    focus_window: window,
                    already_focused: true,
                });
            }
            Ok(ResolvedNode {
                node,
                node_window: window,
                focus_window: window,
                already_focused: false,
            })
        }
        Ok(None) => Err(tool_err(format!(
            "no control at index {index}; re-read accessibility_tree or pass observation_id"
        ))),
        Err(e) => Err(tool_err(e)),
    }
}

/// Model-driven and task-lifecycle tools. They plan with a model, can run for
/// minutes, and are what a chain is meant to replace: letting one of them be a
/// step would hide a whole agent loop inside a single call, with no intermediate
/// state left for the caller to see or stop.
const NON_CHAINABLE: [&str; 9] = [
    "run_task",
    "orchestrate",
    "browser_task",
    "start_task",
    "task_status",
    "task_cancel",
    "replay_run",
    "list_runs",
    "resume_chain",
];

/// A longer sequence belongs in a planned task. Past this many actions the
/// caller has almost certainly lost track of what the screen looks like.
const MAX_CHAIN_STEPS: usize = 12;

/// Pause between steps, so the previous action has landed before the next one
/// is aimed at it. A click that has to bring a window forward already waits for
/// the activation itself, so this stays short.
const DEFAULT_SETTLE_MS: u64 = 120;
const MAX_SETTLE_MS: u64 = 5_000;

/// Bind a step back to the observation the caller already read. The whole chain
/// is planned against one screen state, so repeating the token on every step
/// would only give it more chances to be mistyped.
const INHERITED_KEYS: [&str; 2] = ["observation_id", "window"];

fn chainable(tool: &str) -> bool {
    !NON_CHAINABLE.contains(&tool)
}

/// A validated `then` sequence. Validation happens before anything runs, so a
/// typo in step seven costs the machine nothing instead of performing six
/// actions and then failing.
struct ChainPlan {
    /// Head tool arguments, with the chain controls removed.
    head: Value,
    steps: Vec<(String, Value)>,
    settle_ms: u64,
    /// `Some(object)` is sent to `window_state` after the chain; `None` means
    /// the caller asked for the trace only.
    observe: Option<Value>,
}

fn plan_chain(name: &str, args: &Value) -> std::result::Result<ChainPlan, Value> {
    if !chainable(name) {
        return Err(tool_err(format!(
            "`{name}` plans with a model and can run for minutes, so it cannot carry `then`. \
             Chain primitives instead, or call it on its own."
        )));
    }
    let Some(steps) = args["then"].as_array() else {
        return Err(tool_err(
            "`then` must be an array of objects shaped { \"do\": \"<tool>\", ...that tool's arguments }",
        ));
    };
    if steps.len() > MAX_CHAIN_STEPS {
        return Err(tool_err(format!(
            "`then` allows at most {MAX_CHAIN_STEPS} steps, got {}; split the work or use run_task",
            steps.len()
        )));
    }
    let known: Vec<String> = tool_defs()
        .as_array()
        .map(|defs| {
            defs.iter()
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut parsed = Vec::with_capacity(steps.len());
    for (i, step) in steps.iter().enumerate() {
        let Some(tool) = step["do"].as_str() else {
            return Err(tool_err(format!(
                "chain step {i} needs a string `do` naming the tool to run"
            )));
        };
        if !known.iter().any(|name| name == tool) {
            return Err(tool_err(format!(
                "chain step {i} names `{tool}`, which is not a tool; check tools/list"
            )));
        }
        if !chainable(tool) {
            return Err(tool_err(format!(
                "chain step {i} is `{tool}`, which plans with a model and can run for minutes; \
                 it cannot be a step"
            )));
        }
        if step.get("then").is_some() {
            return Err(tool_err(format!(
                "chain step {i} (`{tool}`) carries its own `then`; a chain is flat, so list the \
                 actions in order instead"
            )));
        }
        let Some(mut map) = step.as_object().cloned() else {
            return Err(tool_err(format!("chain step {i} must be an object")));
        };
        map.remove("do");
        parsed.push((tool.to_string(), Value::Object(map)));
    }

    let mut head = args.as_object().cloned().unwrap_or_default();
    head.remove("then");
    head.remove("settle_ms");
    head.remove("observe");

    let observe = match &args["observe"] {
        Value::Bool(false) => None,
        Value::Object(object) => Some(Value::Object(object.clone())),
        _ => Some(json!({})),
    };
    Ok(ChainPlan {
        head: Value::Object(head),
        steps: parsed,
        settle_ms: args["settle_ms"]
            .as_u64()
            .unwrap_or(DEFAULT_SETTLE_MS)
            .min(MAX_SETTLE_MS),
        observe,
    })
}

fn inherit_from(step: &mut Value, head: &Value) {
    let Some(map) = step.as_object_mut() else {
        return;
    };
    for key in INHERITED_KEYS {
        if map.contains_key(key) {
            continue;
        }
        if let Some(value) = head.get(key).filter(|v| !v.is_null()) {
            map.insert(key.to_string(), value.clone());
        }
    }
}

/// The window to re-read once the chain is done: the last one any part of the
/// chain named, directly or through the observation token it referenced.
fn chain_window(parts: impl IntoIterator<Item = Value>) -> Option<u32> {
    parts.into_iter().fold(None, |found, args| {
        if let Some(id) = window_arg_id(&args["window"]) {
            return Some(id);
        }
        match args["observation_id"].as_str().and_then(observations::get) {
            Some(obs) => Some(obs.window_id),
            None => found,
        }
    })
}

/// One line of the consolidated answer: text kept, images counted and dropped.
/// The caller reads the screen once, at the end of the chain — that is the round
/// trip this saves.
fn step_record(tool: &str, result: &Value) -> Value {
    let mut texts = Vec::new();
    let mut images = 0usize;
    if let Some(parts) = result["content"].as_array() {
        for part in parts {
            match part["type"].as_str() {
                Some("text") => {
                    if let Some(text) = part["text"].as_str() {
                        texts.push(text.to_string());
                    }
                }
                Some("image") => images += 1,
                _ => {}
            }
        }
    }
    let mut record = json!({
        "do": tool,
        "ok": !result["isError"].as_bool().unwrap_or(false),
        "detail": texts.join("\n"),
    });
    if images > 0 {
        record["images_dropped"] = json!(images);
    }
    record
}

/// Maximum cursor drift (in physical pixels) tolerated between chain steps
/// before we treat the movement as external (non-agent) user mouse activity.
const EXTERNAL_CURSOR_TOLERANCE_PX: i32 = 6;

static NEXT_PAUSE_ID: AtomicU64 = AtomicU64::new(1);
static LAST_AGENT_CURSOR: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static EXTERNAL_INPUT_EVENTS: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);
static PAUSED_CHAINS: Mutex<Option<HashMap<String, PausedChain>>> = Mutex::new(None);

#[derive(Debug, Clone)]
struct PausedChain {
    token: String,
    window_id: Option<u32>,
    all_steps: Vec<(String, Value)>,
    stopped_at: usize,
    last_verified_step: Option<usize>,
    settle_ms: u64,
    observe: Option<Value>,
}

fn record_agent_cursor(expected: (i32, i32)) {
    let actual = cua_io::cursor_position().ok().unwrap_or(expected);
    if let Ok(mut guard) = LAST_AGENT_CURSOR.lock() {
        *guard = Some(actual);
    }
}

/// Signal that non-agent mouse or keyboard input was observed before `step_index`.
#[allow(dead_code)]
pub fn notify_external_input(step_index: usize, detail: impl Into<String>) {
    if let Ok(mut guard) = EXTERNAL_INPUT_EVENTS.lock() {
        guard
            .get_or_insert_with(Default::default)
            .insert(step_index, detail.into());
    }
}

#[cfg(test)]
fn reset_concurrency_state() {
    if let Ok(mut g) = LAST_AGENT_CURSOR.lock() {
        *g = None;
    }
    if let Ok(mut g) = EXTERNAL_INPUT_EVENTS.lock() {
        if let Some(m) = g.as_mut() {
            m.clear();
        }
    }
    if let Ok(mut g) = PAUSED_CHAINS.lock() {
        if let Some(m) = g.as_mut() {
            m.clear();
        }
    }
}

/// Check whether non-agent mouse or keyboard input occurred before `step_index`.
fn check_external_user_activity(
    step_index: usize,
    step_args: &Value,
    baseline_cursor: Option<(i32, i32)>,
) -> Option<String> {
    if let Some(reason) = step_args["external_input_detected"].as_str() {
        return Some(reason.to_string());
    }
    if step_args["external_input_detected"].as_bool() == Some(true) {
        return Some("external mouse/keyboard activity reported before step".to_string());
    }
    if let Ok(mut guard) = EXTERNAL_INPUT_EVENTS.lock() {
        if let Some(reason) = guard
            .as_mut()
            .and_then(|map| map.remove(&step_index).or_else(|| map.remove(&usize::MAX)))
        {
            return Some(reason);
        }
    }
    if let Some((bx, by)) = baseline_cursor {
        if let Ok((cx, cy)) = cua_io::cursor_position() {
            let dx = (cx - bx).abs();
            let dy = (cy - by).abs();
            if dx > EXTERNAL_CURSOR_TOLERANCE_PX || dy > EXTERNAL_CURSOR_TOLERANCE_PX {
                return Some(format!(
                    "non-agent mouse movement detected before step {step_index}: cursor moved from ({bx},{by}) to ({cx},{cy})"
                ));
            }
        }
    }
    None
}

/// Determine whether a completed step produced a verified effect on the target.
///
/// Only steps with an explicit verified state change (`effect=changed`, readback
/// `state` from `perform_secondary_action`, passed `verify_submission`, or
/// explicit `"verified_effect": true`) advance `last_verified_step`. Steps that
/// report `effect=unchanged`, `effect=unverifiable`, or unverified blind input
/// do NOT count as verified, so a resumed chain will re-run from the last
/// verified step rather than blindly continuing from where it paused.
fn step_has_verified_effect(tool: &str, step_args: &Value, result: &Value) -> bool {
    if result["isError"].as_bool().unwrap_or(false) {
        return false;
    }
    if let Some(explicit) = step_args["verified_effect"].as_bool() {
        return explicit;
    }
    if let Some(parts) = result["content"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                if text.contains("effect=unchanged") || text.contains("effect=unverifiable") {
                    return false;
                }
                if text.contains("effect=changed") {
                    return true;
                }
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    if parsed["verified_effect"].as_bool() == Some(false) {
                        return false;
                    }
                    if parsed["verified_effect"].as_bool() == Some(true) {
                        return true;
                    }
                    if tool == "perform_secondary_action"
                        && parsed["ok"].as_bool() == Some(true)
                        && !parsed["state"].is_null()
                    {
                        return true;
                    }
                    if tool == "verify_submission" && parsed["ok"].as_bool() == Some(true) {
                        return true;
                    }
                }
            }
        }
    }
    matches!(
        tool,
        "window_state"
            | "control_state"
            | "screen_info"
            | "list_windows"
            | "clipboard_get"
            | "clipboard_set"
    )
}

fn store_paused_chain(paused: PausedChain) {
    if let Ok(mut guard) = PAUSED_CHAINS.lock() {
        guard
            .get_or_insert_with(Default::default)
            .insert(paused.token.clone(), paused);
    }
}

fn take_paused_chain(token: &str) -> Option<PausedChain> {
    PAUSED_CHAINS
        .lock()
        .ok()
        .and_then(|mut guard| guard.as_mut()?.remove(token))
}

async fn resume_paused_chain(args: &Value) -> Value {
    let token = match required_str(args, "resume_token") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let Some(paused) = take_paused_chain(&token) else {
        return tool_err(json!({
            "ok": false,
            "error": {
                "code": "invalid_resume_token",
                "message": format!("no paused chain found for resume_token `{token}`"),
                "retry": false,
            }
        }));
    };

    // Resume strictly from the step after `last_verified_step`, never blindly
    // from `stopped_at` when an unverified step ran in between.
    let resume_from = paused.last_verified_step.map(|s| s + 1).unwrap_or(0);
    let win_id = paused
        .window_id
        .or_else(|| window_arg_id(&args["window"]))
        .unwrap_or(0);

    // Always take a brand-new observation (`disable_diffing: true`) before
    // resuming so indices and control states are freshly resolved.
    let fresh_obs_id = if let Some(raw_nodes) = args.get("fresh_nodes") {
        let nodes: Vec<cua_engine_access::UiNode> =
            serde_json::from_value(raw_nodes.clone()).unwrap_or_default();
        let target_win = if win_id == 0 { 1 } else { win_id };
        let mut snap = cua_engine_access::UiSnapshot {
            nodes: nodes.clone(),
            truncated: false,
            unreadable: Vec::new(),
        };
        let parts = vec![elements::TreePart {
            window_id: target_win,
            title: "resumed".to_string(),
            related: false,
            snapshot: &mut snap,
        }];
        let _ = elements::observe_group(target_win, parts, true);
        let indexed_nodes = snap.nodes;
        let node_windows = vec![target_win; indexed_nodes.len()];
        observations::record(observations::Observation {
            id: observations::next_id(),
            window_id: target_win,
            hash: None,
            shots: Vec::new(),
            nodes: indexed_nodes,
            node_windows,
        })
    } else if win_id != 0 {
        let obs_call = call_tool_single(
            "window_state",
            &json!({
                "window": win_id,
                "disable_diffing": true,
                "include_screenshot": false,
                "include_text": true,
            }),
        )
        .await;
        obs_call["content"]
            .as_array()
            .and_then(|parts| parts.first())
            .and_then(|p| p["text"].as_str())
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|v| v["observation_id"].as_str().map(str::to_string))
            .unwrap_or_else(|| {
                observations::record(observations::Observation {
                    id: observations::next_id(),
                    window_id: win_id,
                    hash: None,
                    shots: Vec::new(),
                    nodes: Vec::new(),
                    node_windows: Vec::new(),
                })
            })
    } else {
        observations::record(observations::Observation {
            id: observations::next_id(),
            window_id: 0,
            hash: None,
            shots: Vec::new(),
            nodes: Vec::new(),
            node_windows: Vec::new(),
        })
    };

    execute_chain_sequence(
        paused.all_steps,
        resume_from,
        paused.last_verified_step,
        paused.settle_ms,
        paused.observe,
        paused.window_id,
        Some(paused.stopped_at),
        Some(fresh_obs_id),
    )
    .await
}

/// Dispatch a tool call. Without `then` this is exactly the single tool, so no
/// existing caller changes behaviour. With `then`, the head action and every
/// step run in order on this machine and one consolidated result comes back:
/// a per-step trace, then a single observation of where things ended up.
///
/// If non-agent mouse or keyboard activity is detected before any step, the
/// chain immediately pauses, reports `stopped_at` and `last_verified_step`, and
/// returns a `resume_token` that `resume_chain` uses to take a fresh observation
/// and resume strictly from `last_verified_step + 1`.
async fn call_tool(name: &str, args: &Value) -> Value {
    if name == "resume_chain" {
        return resume_paused_chain(args).await;
    }
    if args.get("then").is_none() {
        return call_tool_single(name, args).await;
    }
    let plan = match plan_chain(name, args) {
        Ok(plan) => plan,
        Err(e) => return e,
    };

    let mut all_steps: Vec<(String, Value)> = Vec::with_capacity(plan.steps.len() + 1);
    all_steps.push((name.to_string(), plan.head.clone()));
    for (tool, step_args) in &plan.steps {
        let mut inherited = step_args.clone();
        inherit_from(&mut inherited, &plan.head);
        all_steps.push((tool.clone(), inherited));
    }

    let window = chain_window(all_steps.iter().map(|(_, a)| a.clone()));
    execute_chain_sequence(
        all_steps,
        0,
        None,
        plan.settle_ms,
        plan.observe,
        window,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_chain_sequence(
    all_steps: Vec<(String, Value)>,
    start_index: usize,
    initial_last_verified: Option<usize>,
    settle_ms: u64,
    observe: Option<Value>,
    window: Option<u32>,
    resumed_from_stopped_at: Option<usize>,
    fresh_observation_id: Option<String>,
) -> Value {
    let planned = all_steps.len();
    let mut trace: Vec<Value> = Vec::with_capacity(planned.saturating_sub(start_index));
    let mut failure: Option<(usize, String)> = None;
    let mut paused_info: Option<(usize, String, String)> = None;
    let mut last_verified_step: Option<usize> = initial_last_verified;
    let mut baseline_cursor: Option<(i32, i32)> = None;

    for (idx, (tool, raw_step_args)) in all_steps.iter().enumerate().skip(start_index) {
        if idx > start_index && settle_ms > 0 {
            baseline_cursor = LAST_AGENT_CURSOR
                .lock()
                .ok()
                .and_then(|g| *g)
                .or_else(|| cua_io::cursor_position().ok());
            tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
        }

        if let Some(activity) = check_external_user_activity(idx, raw_step_args, baseline_cursor) {
            let token = format!("pause-{}", NEXT_PAUSE_ID.fetch_add(1, Ordering::Relaxed));
            store_paused_chain(PausedChain {
                token: token.clone(),
                window_id: window,
                all_steps: all_steps.clone(),
                stopped_at: idx,
                last_verified_step,
                settle_ms,
                observe: observe.clone(),
            });
            paused_info = Some((idx, activity, token));
            break;
        }

        let mut step_args = raw_step_args.clone();
        if let Some(fresh_obs) = &fresh_observation_id {
            if let Some(map) = step_args.as_object_mut() {
                if map.contains_key("observation_id") {
                    map.insert("observation_id".to_string(), json!(fresh_obs));
                }
            }
        }

        let result = call_tool_single(tool, &step_args).await;
        let mut rec = step_record(tool, &result);
        rec["step"] = json!(idx);
        let verified = step_has_verified_effect(tool, &step_args, &result);
        rec["verified_effect"] = json!(verified);
        trace.push(rec);

        if result["isError"].as_bool().unwrap_or(false) {
            failure = Some((idx, error_text(&result)));
            break;
        }
        if verified {
            last_verified_step = Some(idx);
        }
        baseline_cursor = LAST_AGENT_CURSOR.lock().ok().and_then(|g| *g);
    }

    if let Some((stopped_at, activity, token)) = paused_info {
        let resume_from = last_verified_step.map(|s| s + 1).unwrap_or(0);
        let summary = json!({
            "ok": false,
            "paused": true,
            "pause_reason": "external_input_detected",
            "activity": activity,
            "ran": trace.len(),
            "planned": planned,
            "stopped_at": stopped_at,
            "last_verified_step": last_verified_step,
            "resume_from_step": resume_from,
            "resume_token": token,
            "chain": trace,
            "next": format!(
                "paused at step {stopped_at} due to non-agent input; call resume_chain with resume_token to take a fresh observation and resume from step {resume_from} (after last_verified_step {:?})",
                last_verified_step
            ),
        });
        return json!({
            "content": [{ "type": "text", "text": summary.to_string() }],
            "isError": true,
        });
    }

    let mut summary = json!({
        "ok": failure.is_none(),
        "ran": trace.len(),
        "planned": planned,
        // 0 is the tool named in this call; steps are numbered from 1.
        "stopped_at": failure.as_ref().map(|(at, _)| *at),
        "last_verified_step": last_verified_step,
        "chain": trace,
    });
    if let Some(prev_stop) = resumed_from_stopped_at {
        summary["resumed"] = json!(true);
        summary["resumed_from_step"] = json!(start_index);
        summary["previous_stopped_at"] = json!(prev_stop);
    }
    if let Some(obs_id) = &fresh_observation_id {
        summary["fresh_observation_id"] = json!(obs_id);
    }
    if let Some((_, message)) = &failure {
        summary["error"] = json!(message);
        summary["next"] = json!(
            "the actions before stopped_at really happened; read the observation below before retrying"
        );
    }

    let mut content = vec![json!({ "type": "text", "text": summary.to_string() })];
    if let Some(obs_args) = &observe {
        match window {
            Some(id) => {
                let mut args = obs_args.clone();
                let map = args.as_object_mut();
                if let Some(map) = map {
                    map.entry("window".to_string()).or_insert(json!(id));
                    map.entry("include_screenshot".to_string())
                        .or_insert(json!(false));
                    map.entry("include_text".to_string()).or_insert(json!(true));
                }
                let observation = call_tool_single("window_state", &args).await;
                if let Some(parts) = observation["content"].as_array() {
                    content.extend(parts.iter().cloned());
                }
            }
            None => content.push(json!({
                "type": "text",
                "text": json!({
                    "observation_skipped": "this chain named no window and no observation_id, \
                                            so there is nothing to re-read; pass one of them, or \
                                            set observe:false"
                })
                .to_string(),
            })),
        }
    }
    json!({ "content": content, "isError": failure.is_some() })
}

fn error_text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or("action failed")
        .to_string()
}

async fn call_tool_single(name: &str, args: &Value) -> Value {
    let question_id = extract_question_id(args);
    let max_retries = max_retries_from_args(args);
    if let Some(q) = &question_id {
        if let Err(limit_err) = check_question_retry_budget(q, max_retries) {
            return limit_err;
        }
    }
    if name != "verify_submission" {
        if let Err(gate_err) = enforce_submission_gate(args) {
            return gate_err;
        }
    }
    let result = call_tool_single_inner(name, args).await;
    if let Some(q) = &question_id {
        let succeeded = action_result_succeeded(&result);
        if let Err(limit_err) = record_question_attempt(q, succeeded, max_retries) {
            return limit_err;
        }
    }
    result
}

async fn call_tool_single_inner(name: &str, args: &Value) -> Value {
    match name {
        "verify_submission" => {
            let planned = args
                .get("planned_answers")
                .or_else(|| args.get("expected_answers"))
                .unwrap_or(&Value::Null);
            if planned.is_null() {
                return tool_err("verify_submission requires `planned_answers`");
            }
            let total_override = args["total_questions"].as_u64().map(|n| n as usize);
            let nodes: Vec<cua_engine_access::UiNode> = if let Some(raw_nodes) = args.get("nodes") {
                serde_json::from_value(raw_nodes.clone()).unwrap_or_default()
            } else if !args["window"].is_null() {
                let win = match resolve_window_arg(&args["window"]) {
                    Ok(w) => w,
                    Err(e) => return e,
                };
                cua_engine_access::snapshot_window(win.id, &win.app, &win.title, win.pid, 600, 16)
                    .map(|s| s.nodes)
                    .unwrap_or_default()
            } else if let Some(obs_id) = args["observation_id"].as_str() {
                let Some(obs) = observations::get(obs_id) else {
                    return stale_err(format!(
                        "observation `{obs_id}` is no longer held; re-run window_state"
                    ));
                };
                obs.nodes
            } else {
                cua_engine_access::snapshot(600, 12)
                    .map(|s| s.nodes)
                    .unwrap_or_default()
            };
            let audit = audit_submission_nodes(&nodes, planned, total_override);
            let payload = submission_audit_to_json(&audit);
            if audit.passed {
                tool_ok(payload)
            } else {
                tool_err(payload)
            }
        }
        "start_task" => {
            let opts = match run_options_from_args(args) {
                Ok(o) => o,
                Err(e) => return e,
            };
            let id = crate::jobs::start(opts);
            tool_ok(json!({ "job_id": id, "status": "running" }))
        }
        "task_status" => {
            let id = match required_str(args, "job_id") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match crate::jobs::status(&id) {
                Some(value) => tool_ok(value),
                None => tool_err(format!("unknown job: {id}")),
            }
        }
        "task_cancel" => {
            let id = match required_str(args, "job_id") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match crate::jobs::cancel(&id) {
                Some(status) => tool_ok(json!({ "job_id": id, "status": status })),
                None => tool_err(format!("unknown job: {id}")),
            }
        }
        "run_task" => {
            let opts = match run_options_from_args(args) {
                Ok(o) => o,
                Err(e) => return e,
            };
            match runner::run_task(opts).await {
                Ok(report) => {
                    let payload = json!({
                        "summary": report.summary(),
                        "success": report.is_success(),
                        "steps": report.steps,
                        "artifacts": report.run_dir.to_string_lossy(),
                    });
                    tool_ok(json!([{ "type": "text", "text": payload.to_string() }]))
                }
                Err(e) => tool_err(e),
            }
        }
        "orchestrate" => {
            let opts = match orchestrate_options_from_args(args) {
                Ok(o) => o,
                Err(e) => return e,
            };
            match crate::orchestrator::run(opts).await {
                Ok(report) => {
                    let payload = json!({
                        "summary": report.summary(),
                        "success": report.success,
                        "steps": report.results.len(),
                        "artifacts": report.run_dir.to_string_lossy(),
                    });
                    tool_ok(json!([{ "type": "text", "text": payload.to_string() }]))
                }
                Err(e) => tool_err(e),
            }
        }
        "browser_task" => {
            let task = match required_str(args, "task") {
                Ok(t) => t,
                Err(e) => return e,
            };
            let opts = cua_engine_browser::BrowserRunOptions {
                task,
                max_steps: args["max_steps"].as_u64().unwrap_or(15) as u32,
                dry_run: args["dry_run"].as_bool().unwrap_or(false),
                model: args["model"].as_str().map(str::to_string),
                headed: args["headed"].as_bool().unwrap_or(false),
                start_url: args["start_url"].as_str().map(str::to_string),
                run_dir: None,
                screenshot_quality: 70,
                // Same defaults as `run_task`: a caller that lets a model drive a real
                // browser has to say so, or be able to answer a confirmation.
                yes: args["yes"].as_bool().unwrap_or(false),
                allow_high: args["allow_high"].as_bool().unwrap_or(false),
                approver: approver_from_elicitor(),
                timeout: args["timeout_ms"]
                    .as_u64()
                    .map(std::time::Duration::from_millis),
                cancel: None,
            };
            match cua_engine_browser::run_task(opts).await {
                Ok(report) => {
                    let payload = json!({
                        "summary": report.outcome.summary(report.steps),
                        "success": report.outcome.is_success(),
                        "steps": report.steps,
                        "artifacts": report.run_dir.to_string_lossy(),
                    });
                    tool_ok(json!([{ "type": "text", "text": payload.to_string() }]))
                }
                Err(e) => tool_err(e),
            }
        }
        "list_runs" => {
            let limit = args["limit"].as_u64().unwrap_or(20) as usize;
            let root = args["root"].as_str().unwrap_or(".runs");
            match crate::replay::list_runs(std::path::Path::new(root), limit) {
                Ok(runs) => tool_ok(json!(runs)),
                Err(e) => tool_err(e),
            }
        }
        "replay_run" => {
            let dir = match required_str(args, "dir") {
                Ok(d) => d,
                Err(e) => return e,
            };
            let opts = crate::replay::ReplayOptions {
                dir: std::path::PathBuf::from(dir),
                dry_run: args["dry_run"].as_bool().unwrap_or(false),
                yes: args["yes"].as_bool().unwrap_or(false),
                allow_high: args["allow_high"].as_bool().unwrap_or(false),
                max_steps: args["max_steps"].as_u64().map(|n| n as u32),
                approver: approver_from_elicitor(),
            };
            match crate::replay::replay(opts).await {
                Ok(report) => tool_ok(json!({
                    "summary": report.summary(),
                    "executed": report.executed,
                    "skipped": report.skipped,
                    "dry_run": report.dry_run,
                })),
                Err(e) => tool_err(e),
            }
        }
        "capture_screen" => match cua_io::observe(1280, 75) {
            Ok(frame) => {
                use base64::Engine as _;
                let data =
                    base64::engine::general_purpose::STANDARD.encode(&frame.observation.image);
                tool_ok(json!([
                    { "type": "image", "data": data, "mimeType": frame.observation.media_type },
                    { "type": "text", "text": format!(
                        "{}x{} (physical {}x{})",
                        frame.observation.width, frame.observation.height,
                        frame.physical.0, frame.physical.1
                    ) }
                ]))
            }
            Err(e) => tool_err(e),
        },
        "list_windows" => {
            let query = args["query"].as_str();
            let include_minimized = args["include_minimized"].as_bool().unwrap_or(false);
            match cua_io::list_windows(query, include_minimized) {
                Ok(windows) => tool_ok(json!({
                    "count": windows.len(),
                    "windows": windows
                        .iter()
                        .map(|w| json!({
                            "id": w.id,
                            "pid": w.pid,
                            "app": w.app,
                            "title": w.title,
                            "x": w.x,
                            "y": w.y,
                            "width": w.width,
                            "height": w.height,
                            "minimized": w.minimized,
                            "focused": w.focused,
                        }))
                        .collect::<Vec<_>>()
                })),
                Err(e) => tool_err(e),
            }
        }
        "list_apps" => {
            let include_all = args["include_all"].as_bool().unwrap_or(false);
            match cua_io::list_apps(include_all) {
                Ok(apps) => tool_ok(json!({
                    "count": apps.len(),
                    "include_all": include_all,
                    "apps": apps.iter().map(|a| json!({
                        "id": a.id,
                        "display_name": a.display_name,
                        "pid": a.pid,
                        "running": a.running,
                        "source": if a.source == cua_io::AppSource::Running { "running" } else { "start_menu" },
                        "windows": a.windows.iter().map(|w| json!({
                            "id": w.id,
                            "title": w.title,
                            "minimized": w.minimized,
                            "focused": w.focused,
                        })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })),
                Err(e) => tool_err(e),
            }
        }
        "launch_app" => {
            let app = match required_str(args, "app") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(20_000);
            match cua_io::launch_app(&app, timeout_ms) {
                Ok(outcome) => tool_ok(json!({
                    "ok": true,
                    "launched": !outcome.already_running,
                    "already_running": outcome.already_running,
                    "app": outcome.id,
                    "display_name": outcome.display_name,
                    "pid": outcome.pid,
                    "matched_by": outcome.matched_by,
                    "ready": outcome.ready,
                    "windows": outcome.windows.iter().map(|w| json!({
                        "id": w.id,
                        "title": w.title,
                        "minimized": w.minimized,
                        "focused": w.focused,
                    })).collect::<Vec<_>>(),
                    "usage": if outcome.ready {
                        "observe one of these windows with window_state before acting"
                    } else {
                        "no window appeared within the timeout; run list_windows before retrying, and do not launch the same app again"
                    },
                })),
                Err(e) => tool_err(e),
            }
        }
        "capture_window" => {
            let selector = match required_str(args, "window") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let max_edge = args["max_edge"].as_u64().unwrap_or(1280) as u32;
            let quality = args["quality"].as_u64().unwrap_or(75) as u8;
            match cua_io::capture_window(&selector, max_edge, quality) {
                Ok(cap) => {
                    let meta = window_capture_meta(&cap);
                    tool_ok(json!([
                        { "type": "image", "data": encode_base64(&cap.frame.observation.image), "mimeType": cap.frame.observation.media_type },
                        { "type": "text", "text": meta.to_string() }
                    ]))
                }
                Err(e) => tool_err(e),
            }
        }
        "window_state" => {
            let window = match resolve_window_arg(&args["window"]) {
                Ok(window) => window,
                Err(e) => return e,
            };
            let include_screenshot = args["include_screenshot"].as_bool().unwrap_or(true);
            let include_text = args["include_text"].as_bool().unwrap_or(true);
            let include_related = args["include_related"].as_bool().unwrap_or(false);
            let disable_diffing = args["disable_diffing"].as_bool().unwrap_or(false);
            let max_edge = args["max_edge"].as_u64().unwrap_or(1280) as u32;
            let quality = args["quality"].as_u64().unwrap_or(75) as u8;
            let max_nodes = args["max_nodes"].as_u64().unwrap_or(200) as usize;
            let depth = args["depth"].as_u64().unwrap_or(12) as usize;

            let mut content: Vec<Value> = Vec::new();
            let mut hash = None;
            let mut screenshot: Option<Value> = None;
            let mut shots: Vec<observations::Shot> = Vec::new();
            let observation_id = observations::next_id();

            // Menus, tooltips and dialogs are separate windows owned by the same
            // process. By default only their titles are listed; pass
            // include_related=true to expand their full trees and screenshots.
            let related =
                cua_io::related_windows(window.pid, window.id, MAX_RELATED).unwrap_or_default();
            let related_titles = related_title_summaries(&related);

            if include_screenshot {
                match cua_io::capture_window(&window.id.to_string(), max_edge, quality) {
                    Ok(cap) => {
                        let shot_id = format!("{observation_id}:s0");
                        let mut meta = window_capture_meta(&cap);
                        if let Some(map) = meta.as_object_mut() {
                            map.insert("role".into(), json!("target"));
                            map.insert("screenshot_id".into(), json!(shot_id));
                        }
                        shots.push(observations::Shot {
                            id: shot_id,
                            window_id: window.id,
                            role: "target",
                            origin: (cap.info.x, cap.info.y),
                            scale: cap.frame.scale,
                            size: (cap.frame.observation.width, cap.frame.observation.height),
                            hash: cap.hash,
                        });
                        screenshot = Some(meta);
                        content.push(json!({
                            "type": "image",
                            "data": encode_base64(&cap.frame.observation.image),
                            "mimeType": cap.frame.observation.media_type,
                        }));
                        // Each popup image carries its own id and origin: a
                        // coordinate read off a menu is not a coordinate in the
                        // window that owns it.
                        let mut composite = cap.hash;
                        let mut related_meta = Vec::new();
                        if include_related {
                            for (n, rel) in related.iter().enumerate() {
                                if let Ok(rel_cap) =
                                    cua_io::capture_window(&rel.id.to_string(), max_edge, quality)
                                {
                                    let rel_shot_id = format!("{observation_id}:s{}", n + 1);
                                    composite =
                                        composite.wrapping_mul(31).wrapping_add(rel_cap.hash);
                                    let mut meta = window_capture_meta(&rel_cap);
                                    if let Some(map) = meta.as_object_mut() {
                                        map.insert("role".into(), json!("related"));
                                        map.insert("screenshot_id".into(), json!(rel_shot_id));
                                    }
                                    shots.push(observations::Shot {
                                        id: rel_shot_id,
                                        window_id: rel.id,
                                        role: "related",
                                        origin: (rel_cap.info.x, rel_cap.info.y),
                                        scale: rel_cap.frame.scale,
                                        size: (
                                            rel_cap.frame.observation.width,
                                            rel_cap.frame.observation.height,
                                        ),
                                        hash: rel_cap.hash,
                                    });
                                    related_meta.push(meta);
                                    content.push(json!({
                                        "type": "image",
                                        "data": encode_base64(&rel_cap.frame.observation.image),
                                        "mimeType": rel_cap.frame.observation.media_type,
                                    }));
                                }
                            }
                        }
                        if !related_meta.is_empty() {
                            if let Some(map) = screenshot.as_mut().and_then(Value::as_object_mut) {
                                map.insert("related".into(), json!(related_meta));
                            }
                        } else if !related_titles.is_empty() {
                            if let Some(map) = screenshot.as_mut().and_then(Value::as_object_mut) {
                                map.insert("related_titles".into(), json!(related_titles));
                            }
                        }
                        hash = Some(composite);
                    }
                    Err(e) => {
                        // A window that cannot be captured is still describable:
                        // keep going so the accessibility half of the state is
                        // not lost, but say why the image is missing.
                        content.push(json!({
                            "type": "text",
                            "text": json!({ "screenshot_error": e.to_string() }).to_string(),
                        }));
                    }
                }
            }

            let mut nodes: Vec<cua_engine_access::UiNode> = Vec::new();
            let mut node_windows: Vec<u32> = Vec::new();
            let mut accessibility: Value = if include_text {
                match snapshot_with_retry(
                    || {
                        cua_engine_access::snapshot_window(
                            window.id,
                            &window.app,
                            &window.title,
                            window.pid,
                            max_nodes,
                            depth,
                        )
                    },
                    || {
                        cua_engine_access::atspi::reset_wedged();
                        let _ = cua_engine_access::focus_window(window.id);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    },
                ) {
                    SnapshotAttempt::Available {
                        snapshot: mut snap,
                        retried,
                    } => {
                        // Popups contribute their own tree section under a header
                        // line only when include_related=true; otherwise only their
                        // titles are reported so unrelated windows do not bloat the
                        // observation.
                        let mut related_snaps: Vec<(
                            cua_io::WindowInfo,
                            cua_engine_access::UiSnapshot,
                        )> = Vec::new();
                        let shown: Vec<(i32, i32, i32, i32)> =
                            snap.nodes.iter().map(|n| n.rect).collect();
                        let target_area = window.width as i64 * window.height as i64;
                        let mut uncovered_related: Vec<cua_io::WindowInfo> = Vec::new();
                        for rel in &related {
                            if rel.width as i64 * rel.height as i64 > target_area {
                                continue;
                            }
                            let popup = (rel.x, rel.y, rel.width as i32, rel.height as i32);
                            if covered_by(&shown, popup) {
                                continue;
                            }
                            uncovered_related.push(rel.clone());
                            if include_related {
                                if let Ok(rel_snap) = cua_engine_access::snapshot_window(
                                    rel.id,
                                    &rel.app,
                                    &rel.title,
                                    rel.pid,
                                    MAX_POPUP_NODES,
                                    depth,
                                ) {
                                    related_snaps.push((rel.clone(), rel_snap));
                                }
                            }
                        }
                        let mut parts: Vec<elements::TreePart> =
                            Vec::with_capacity(1 + related_snaps.len());
                        parts.push(elements::TreePart {
                            window_id: window.id,
                            title: window.title.clone(),
                            related: false,
                            snapshot: &mut snap,
                        });
                        for (rel, rel_snap) in related_snaps.iter_mut() {
                            parts.push(elements::TreePart {
                                window_id: rel.id,
                                title: rel.title.clone(),
                                related: true,
                                snapshot: rel_snap,
                            });
                        }
                        let view = elements::observe_group(window.id, parts, disable_diffing);
                        let tree_text = if !include_related && !uncovered_related.is_empty() {
                            append_related_titles_only(view.text, &uncovered_related)
                        } else {
                            view.text
                        };
                        nodes = snap.nodes.clone();
                        node_windows = vec![window.id; snap.nodes.len()];
                        let mut truncated = snap.truncated;
                        let mut unreadable = snap.unreadable.clone();
                        for (rel, rel_snap) in &related_snaps {
                            nodes.extend(rel_snap.nodes.iter().cloned());
                            node_windows.extend(std::iter::repeat_n(rel.id, rel_snap.nodes.len()));
                            truncated |= rel_snap.truncated;
                            unreadable.extend(rel_snap.unreadable.iter().cloned());
                        }
                        let mut acc = json!({
                            "mode": view.mode.as_str(),
                            "tree": tree_text,
                            "node_count": view.node_count,
                            "changed_lines": view.changed_lines,
                            "truncated": truncated,
                            "retried": retried,
                            "include_related": include_related,
                            "related": related_titles,
                            "notice": view.notice,
                        });
                        if let Some(note) = cua_engine_access::unreadable_note(&unreadable) {
                            if let Some(map) = acc.as_object_mut() {
                                map.insert("unreadable".to_string(), json!(note.trim_end()));
                            }
                        }
                        let text_ctx =
                            cua_engine_access::text_context(window.id).unwrap_or_default();
                        let selected_lines = snap.selected_lines();
                        if let Some(map) = acc.as_object_mut() {
                            if let Some(selected) = text_ctx.selected_text {
                                map.insert("selected_text".into(), json!(selected));
                            }
                            if let Some(document) = text_ctx.document_text {
                                map.insert("document_text".into(), json!(document));
                            }
                            if !selected_lines.is_empty() {
                                map.insert("selected_elements".into(), json!(selected_lines));
                            }
                        }
                        acc
                    }
                    SnapshotAttempt::CoordinateOnly { retried, reason } => {
                        coordinate_only_accessibility(retried, &reason)
                    }
                }
            } else {
                coordinate_only_accessibility(false, "include_text=false")
            };
            if let Some(map) = accessibility.as_object_mut() {
                if !map.contains_key("related") && !related_titles.is_empty() {
                    map.insert("related".into(), json!(related_titles));
                }
            }

            // A null `focused_element` has two different meanings: "nothing is
            // focused" (a clean pre-flight) and "this platform cannot tell"
            // (no information at all). Say which one it was.
            let (focused, focus_note) = match cua_engine_access::focused_element() {
                Ok(node) => (node, None),
                Err(e) => (None, Some(e.to_string())),
            };
            let recorded = observations::record(observations::Observation {
                id: observation_id.clone(),
                window_id: window.id,
                hash,
                shots,
                nodes,
                node_windows,
            });
            let observation_id = recorded;

            let mut meta = json!({
                "ok": true,
                "observation_id": observation_id,
                "window": window_brief(&window),
                "foreground": window.focused,
                "focused_element": focused.map(|node| json!({
                    "control_type": node.control_type,
                    "name": node.name,
                    "automation_id": node.automation_id,
                    "rect": { "x": node.rect.0, "y": node.rect.1, "w": node.rect.2, "h": node.rect.3 },
                })),
                "accessibility": accessibility,
                "usage": "element_index and x/y are only valid with this observation_id. accessibility.tree is a unified diff against your previous observation of this window unless mode is full or baseline_reset: keep the mapping you have, apply the diff, and treat any index on a '-' line as retired. After acting, re-read this window.",
            });
            if let Some(note) = focus_note {
                if let Some(map) = meta.as_object_mut() {
                    map.insert("focused_element_note".to_string(), json!(note));
                }
            }
            if let Some(shot) = screenshot {
                if let Some(map) = meta.as_object_mut() {
                    map.insert("screenshot".to_string(), shot);
                }
            }
            content.push(json!({ "type": "text", "text": meta.to_string() }));
            tool_ok(content)
        }
        "wait_for_change" => {
            let window = match resolve_window_arg(&args["window"]) {
                Ok(window) => window,
                Err(e) => return e,
            };
            let requested_timeout_ms = args["timeout_ms"].as_u64().unwrap_or(3_000);
            const MAX_WAIT_FOR_CHANGE_TIMEOUT_MS: u64 = 60_000;
            let clamped = requested_timeout_ms > MAX_WAIT_FOR_CHANGE_TIMEOUT_MS;
            let timeout_ms = requested_timeout_ms.min(MAX_WAIT_FOR_CHANGE_TIMEOUT_MS);

            let baseline = match args["observation_id"].as_str().and_then(observations::get) {
                Some(obs) => obs.hash,
                None => None,
            };
            let baseline = match baseline.or(args["hash"].as_u64()) {
                Some(hash) => hash,
                None => match window_state_hash(&window) {
                    Ok(hash) => hash,
                    Err(e) => return e,
                },
            };
            let started = std::time::Instant::now();
            loop {
                let current = match window_state_hash(&window) {
                    Ok(hash) => hash,
                    Err(e) => return e,
                };
                if current != baseline {
                    let mut payload = json!({
                        "ok": true,
                        "changed": true,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "hash": current,
                    });
                    if clamped {
                        payload["clamped"] = json!(true);
                        payload["requested_timeout_ms"] = json!(requested_timeout_ms);
                    }
                    return tool_ok(payload);
                }
                if started.elapsed().as_millis() as u64 >= timeout_ms {
                    let mut payload = json!({
                        "ok": true,
                        "changed": false,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "hash": current,
                    });
                    if clamped {
                        payload["clamped"] = json!(true);
                        payload["requested_timeout_ms"] = json!(requested_timeout_ms);
                    }
                    return tool_ok(payload);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        "focus_window" => {
            let window = match resolve_window_arg(&args["window"]) {
                Ok(window) => window,
                Err(e) => return e,
            };
            match cua_engine_access::focus_window(window.id) {
                Ok(()) => tool_ok(json!({
                    "ok": true,
                    "foreground": true,
                    "window": window_brief(&window),
                })),
                Err(e) => tool_err(json!({
                    "ok": false,
                    "error": {
                        "code": "focus_failed",
                        "message": e.to_string(),
                        "retry": true,
                    }
                })),
            }
        }
        "window_input" => {
            let window = match resolve_window_arg(&args["window"]) {
                Ok(window) => window,
                Err(e) => return e,
            };
            let text = match required_str(args, "text") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let submit = args["submit"].as_bool().unwrap_or(false);
            let interval_ms = args["interval_ms"].as_u64().unwrap_or(8);
            // Validate the aim before touching the desktop: raising a window is a
            // side effect, and a call that is about to be refused should never have
            // changed what the user sees. It also means the containment answer
            // arrives on platforms where focusing is not implemented at all.
            let aim = match window_relative_point(&window, args["x"].as_i64(), args["y"].as_i64()) {
                Ok(aim) => aim,
                Err(e) => return e,
            };
            if let Err(e) = cua_engine_access::focus_window(window.id) {
                // A platform that has not implemented window activation is not a
                // transient failure: retrying `focus_failed` forever is how a caller
                // ends up believing the tool almost works. Say which one it is.
                let unsupported = matches!(e, cua_core::Error::Unsupported(_));
                return tool_err(json!({
                    "ok": false,
                    "error": {
                        "code": if unsupported { "unsupported" } else { "focus_failed" },
                        "message": e.to_string(),
                        "hint": unsupported.then_some("window_input needs window activation, which this platform has not implemented; use click with screen coordinates and type_text instead"),
                        "retry": !unsupported,
                    }
                }));
            }
            if let Some((px, py)) = aim {
                let action = Action::Click {
                    at: Point::new(px, py),
                    button: cua_core::MouseButton::Left,
                    double: false,
                };
                if let Err(e) = cua_io::apply(&action, 1.0) {
                    return tool_err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let chars = text.chars().count();
            let type_action = Action::Type { text, interval_ms };
            if let Err(e) = cua_io::apply(&type_action, 1.0) {
                return tool_err(e);
            }
            if submit {
                let enter = Action::Key {
                    combo: "Return".into(),
                };
                if let Err(e) = cua_io::apply(&enter, 1.0) {
                    return tool_err(e);
                }
            }
            tool_ok(json!({
                "ok": true,
                "foreground": true,
                "window": window_brief(&window),
                "typed": true,
                "chars": chars,
                "submitted": submit,
            }))
        }
        "accessibility_tree" => {
            let max_nodes = args["max_nodes"].as_u64().unwrap_or(120) as usize;
            let depth = args["depth"].as_u64().unwrap_or(2) as usize;
            match cua_engine_access::snapshot(max_nodes, depth) {
                Ok(snap) => tool_ok(snap.to_text()),
                Err(e) => tool_err(e),
            }
        }
        "find_control" => {
            let target = match target_arg(args) {
                Ok(target) => target,
                Err(e) => return e,
            };
            match cua_engine_access::find_target(&target, 400, 8) {
                Ok(node) => tool_ok(json!({
                    "name": node.name,
                    "control_type": node.control_type,
                    "automation_id": node.automation_id,
                    "rect": { "x": node.rect.0, "y": node.rect.1, "w": node.rect.2, "h": node.rect.3 },
                    "center": { "x": node.rect.0 + node.rect.2 / 2, "y": node.rect.1 + node.rect.3 / 2 }
                })),
                Err(e) => tool_err(e),
            }
        }
        "set_control_text" => {
            let target = match target_arg(args) {
                Ok(target) => target,
                Err(e) => return e,
            };
            let text = match required_str(args, "text") {
                Ok(t) => t,
                Err(e) => return e,
            };
            let (max_nodes, max_depth) = action_limits(args);
            match cua_engine_access::set_value_target(&target, &text, max_nodes, max_depth) {
                Ok(Some(report)) => action_outcome_with_verification_crop(
                    format!(
                        "set text of {} \"{}\" ({} chars){}",
                        report.node.control_type,
                        report.node.name,
                        text.chars().count(),
                        effect_note(&report, true)
                    ),
                    &report,
                ),
                Ok(None) => tool_err(format!(
                    "control {} has no Value/EditableText pattern; focus it and use type_text",
                    cua_engine_access::target::describe_target(&target)
                )),
                Err(e) => tool_err(e),
            }
        }
        "control_state" => {
            let target = match target_arg(args) {
                Ok(target) => target,
                Err(e) => return e,
            };
            match cua_engine_access::state_target(
                &target,
                cua_engine_access::DEFAULT_ACTION_NODES,
                cua_engine_access::DEFAULT_ACTION_DEPTH,
            ) {
                Ok(state) => tool_ok(json!(state)),
                Err(e) => tool_err(e),
            }
        }
        "invoke_element" => {
            let index = args["index"].as_u64().unwrap_or(u64::MAX) as usize;
            let resolved = match resolve_indexed_node(args, index, "invoke_element") {
                Ok(r) => r,
                Err(e) => return e,
            };
            if !resolved.already_focused {
                if let Some(id) = resolved.focus_window {
                    if let Err(e) = cua_engine_access::focus_window(id) {
                        return tool_err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
            }
            let Some(selector) = selector_for_node(&resolved.node) else {
                return tool_err(format!(
                    "control at index {index} has no accessible name or automation id"
                ));
            };
            match cua_engine_access::invoke_target(
                &cua_engine_access::target::Target::by_name(&selector),
                cua_engine_access::DEFAULT_ACTION_NODES,
                cua_engine_access::DEFAULT_ACTION_DEPTH,
            ) {
                Ok(Some(report)) => action_outcome_with_verification_crop(
                    format!(
                        "invoked {} \"{}\" via accessibility pattern{}",
                        report.node.control_type,
                        report.node.name,
                        effect_note(&report, false)
                    ),
                    &report,
                ),
                Ok(None) => {
                    let Some(center) = control_center(resolved.node.rect) else {
                        return tool_err(format!(
                            "control at index {index} has no usable rectangle"
                        ));
                    };
                    let action = Action::Click {
                        at: Point::new(center.0, center.1),
                        button: cua_core::MouseButton::Left,
                        double: false,
                    };
                    match cua_io::apply(&action, 1.0) {
                        Ok(()) => {
                            record_agent_cursor(center);
                            tool_ok(format!(
                                "clicked {} \"{}\" at ({},{})",
                                resolved.node.control_type, resolved.node.name, center.0, center.1
                            ))
                        }
                        Err(e) => tool_err(e),
                    }
                }
                Err(e) => tool_err(e),
            }
        }
        "set_element_value" => {
            let index = args["index"].as_u64().unwrap_or(u64::MAX) as usize;
            let text = match required_str(args, "text") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let resolved = match resolve_indexed_node(args, index, "set_element_value") {
                Ok(r) => r,
                Err(e) => return e,
            };
            let node = resolved.node;
            if !resolved.already_focused {
                if let Some(id) = resolved.focus_window {
                    if let Err(e) = cua_engine_access::focus_window(id) {
                        return tool_err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
            }
            let Some(selector) = selector_for_node(&node) else {
                return tool_err(format!(
                    "control at index {index} has no accessible name or automation id"
                ));
            };
            match cua_engine_access::set_value_target(
                &cua_engine_access::target::Target::by_name(&selector),
                &text,
                cua_engine_access::DEFAULT_ACTION_NODES,
                cua_engine_access::DEFAULT_ACTION_DEPTH,
            ) {
                Ok(Some(report)) => action_outcome_with_verification_crop(
                    format!(
                        "set {} \"{}\" via Value/EditableText{}",
                        report.node.control_type,
                        report.node.name,
                        effect_note(&report, true)
                    ),
                    &report,
                ),
                Ok(None) => tool_err(format!(
                    "control at index {index} has no Value/EditableText pattern; use type_text"
                )),
                Err(e) => tool_err(e),
            }
        }
        "perform_secondary_action" => {
            let index = args["element_index"]
                .as_u64()
                .or_else(|| args["index"].as_u64())
                .unwrap_or(u64::MAX) as usize;
            let action = match required_str(args, "action") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let resolved = match resolve_indexed_node(args, index, "perform_secondary_action") {
                Ok(r) => r,
                Err(e) => return e,
            };
            let node = resolved.node;
            // The signature is what makes the target unambiguous, and only nodes
            // that came from a window observation carry one.
            if node.signature.is_empty() {
                return tool_err(json!({
                    "ok": false,
                    "error": {
                        "code": "missing_observation",
                        "message": format!(
                            "element_index {index} did not come from window_state; pass observation_id and element_index"
                        ),
                        "refetch": true,
                        "retry": true,
                    }
                }));
            }
            // A popup node is looked up inside the popup's own tree; activating
            // the owner window first would dismiss the popup.
            let Some(window_id) = resolved
                .node_window
                .or_else(|| window_arg_id(&args["window"]))
            else {
                return tool_err(
                    "perform_secondary_action needs the window the element belongs to; pass observation_id from window_state",
                );
            };

            // Check the advertised list before touching the control: guessing an
            // action name is how an agent "expands" the wrong widget.
            let requested = action.trim();
            if !node
                .actions
                .iter()
                .any(|a| a.eq_ignore_ascii_case(requested))
            {
                return tool_err(json!({
                    "ok": false,
                    "error": {
                        "code": "unsupported_action",
                        "message": format!(
                            "`{requested}` is not exposed by {} \"{}\"",
                            node.control_type, node.name
                        ),
                        "supported_actions": node.actions,
                        "refetch": false,
                        "retry": false,
                    }
                }));
            }

            // Most patterns only work on the window that owns the foreground; a
            // popup is already up, so leave the foreground alone for its nodes.
            if !resolved.already_focused {
                if let Some(id) = resolved.focus_window {
                    if let Err(e) = cua_engine_access::focus_window(id) {
                        return tool_err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
            }

            match cua_engine_access::secondary_action(window_id, &node.signature, requested) {
                Ok(outcome) => tool_ok(json!({
                    "ok": true,
                    "action": outcome.action,
                    "element": {
                        "index": node.index,
                        "control_type": node.control_type,
                        "name": node.name,
                        "automation_id": node.automation_id,
                        "rect": { "x": node.rect.0, "y": node.rect.1, "w": node.rect.2, "h": node.rect.3 },
                    },
                    "state": outcome.state,
                    "supported_actions": outcome.supported,
                    "usage": "the state above is read back from the control; re-read window_state when the action was meant to change what is on screen",
                })),
                Err(e) => tool_err(e),
            }
        }
        "invoke_control" => {
            let target = match target_arg(args) {
                Ok(target) => target,
                Err(e) => return e,
            };
            // Prefer the accessibility action: no pointer movement and immune to
            // overlapping windows.
            let (max_nodes, max_depth) = action_limits(args);
            match cua_engine_access::invoke_target(&target, max_nodes, max_depth) {
                Ok(Some(report)) => {
                    return action_outcome_with_verification_crop(
                        format!(
                            "invoked {} \"{}\" via accessibility pattern{}",
                            report.node.control_type,
                            report.node.name,
                            effect_note(&report, false)
                        ),
                        &report,
                    )
                }
                Ok(None) => {}
                Err(e) => return tool_err(e),
            }
            match cua_engine_access::find_target(&target, 400, 8) {
                Ok(node) => {
                    let Some(center) = control_center(node.rect) else {
                        return tool_err(format!(
                            "control {} reports no usable rectangle, so there is nothing to click",
                            cua_engine_access::target::describe_target(&target)
                        ));
                    };
                    let action = Action::Click {
                        at: Point::new(center.0, center.1),
                        button: cua_core::MouseButton::Left,
                        double: false,
                    };
                    match cua_io::apply(&action, 1.0) {
                        Ok(()) => {
                            record_agent_cursor(center);
                            tool_ok(format!(
                                "clicked {} \"{}\" at ({},{})",
                                node.control_type, node.name, center.0, center.1
                            ))
                        }
                        Err(e) => tool_err(e),
                    }
                }
                Err(e) => tool_err(e),
            }
        }
        "click" => {
            let observation_id = args["observation_id"].as_str();
            let element_index = args["element_index"].as_u64();
            let target_window: Option<u32>;
            let mut already_focused = false;
            let point: (i32, i32) = if let Some(index) = element_index {
                let resolved = match resolve_indexed_node(args, index as usize, "click") {
                    Ok(r) => r,
                    Err(e) => return e,
                };
                let (x, y) = match control_center(resolved.node.rect) {
                    Some(center) => center,
                    None => {
                        return stale_err(format!("element_index {index} has no usable rectangle"))
                    }
                };
                // A node inside a popup must not trigger activation of the
                // owner window: that closes the menu before the click lands.
                target_window = resolved.focus_window;
                already_focused = resolved.already_focused;
                (x, y)
            } else if let (Some(x), Some(y)) = (args["x"].as_f64(), args["y"].as_f64()) {
                let shot_id = args["screenshot_id"].as_str();
                match observation_id.and_then(observations::get) {
                    Some(obs) => {
                        let Some((shot, mapped)) = observations::point_in(&obs, shot_id, x, y)
                        else {
                            let known = observations::shot_ids(&obs);
                            return stale_err(format!(
                                "observation {} cannot map ({x},{y}) against screenshot {:?}; its images are [{}], and the point must lie inside the one you name",
                                obs.id,
                                shot_id.unwrap_or("the target window"),
                                known.join(", ")
                            ));
                        };
                        // A popup is already up and belongs to the foreground
                        // app; activating its owner can dismiss it, so only the
                        // target window's own image triggers a focus.
                        target_window = (shot.role == "target").then_some(obs.window_id);
                        mapped
                    }
                    None => {
                        if shot_id.is_some() {
                            return stale_err(
                                "screenshot_id needs the observation_id that returned it; re-run window_state",
                            );
                        }
                        let (x, y) = (x as i32, y as i32);
                        if let Err(e) = ensure_on_screen(x as i64, y as i64) {
                            return e;
                        }
                        target_window = window_arg_id(&args["window"]);
                        (x, y)
                    }
                }
            } else {
                return tool_err(
                    "click needs element_index + observation_id, or x/y (optionally with observation_id)",
                );
            };
            if !already_focused {
                if let Some(id) = target_window {
                    if !cua_engine_access::is_foreground_window(id) {
                        if let Err(e) = cua_engine_access::focus_window(id) {
                            return tool_err(json!({
                                "ok": false,
                                "error": {
                                    "code": "focus_failed",
                                    "message": e.to_string(),
                                    "retry": true,
                                }
                            }));
                        }
                        // Let the activation settle before the pointer moves: a click
                        // delivered to a window that has not come forward yet lands on
                        // whatever was in front.
                        std::thread::sleep(std::time::Duration::from_millis(60));
                    }
                }
            }
            let button = if args["middle"].as_bool().unwrap_or(false) {
                cua_core::MouseButton::Middle
            } else if args["right"].as_bool().unwrap_or(false) {
                cua_core::MouseButton::Right
            } else {
                cua_core::MouseButton::Left
            };
            let action = Action::Click {
                at: Point::new(point.0, point.1),
                button,
                double: args["double"].as_bool().unwrap_or(false),
            };
            match cua_io::apply(&action, 1.0) {
                Ok(()) => {
                    record_agent_cursor(point);
                    tool_ok(json!({
                        "ok": true,
                        "x": point.0,
                        "y": point.1,
                        "window": target_window,
                        "element_index": element_index,
                    }))
                }
                Err(e) => tool_err(e),
            }
        }
        "type_text" => {
            let text = match required_str(args, "text") {
                Ok(t) => t,
                Err(e) => return e,
            };
            let action = Action::Type {
                text: text.clone(),
                interval_ms: args["interval_ms"].as_u64().unwrap_or(6),
            };
            match cua_io::apply(&action, 1.0) {
                Ok(()) => tool_ok(format!("typed {} chars", text.chars().count())),
                Err(e) => tool_err(e),
            }
        }
        "scroll" => {
            let (Some(x), Some(y)) = (args["x"].as_i64(), args["y"].as_i64()) else {
                return tool_err("scroll requires integer x and y (physical pixels)");
            };
            let dy = args["dy"].as_i64().unwrap_or(0) as i32;
            let dx = args["dx"].as_i64().unwrap_or(0) as i32;
            if dy == 0 && dx == 0 {
                return tool_err("scroll requires a non-zero dy or dx");
            }
            if let Err(e) = ensure_on_screen(x, y) {
                return e;
            }
            let action = Action::Scroll {
                at: Point::new(x as i32, y as i32),
                dx,
                dy,
            };
            match cua_io::apply(&action, 1.0) {
                Ok(()) => tool_ok(format!("scrolled dx={dx} dy={dy} at ({x},{y})")),
                Err(e) => tool_err(e),
            }
        }
        "move" => {
            let (Some(x), Some(y)) = (args["x"].as_i64(), args["y"].as_i64()) else {
                return tool_err("move requires integer x and y (physical pixels)");
            };
            if let Err(e) = ensure_on_screen(x, y) {
                return e;
            }
            let action = Action::Move {
                to: Point::new(x as i32, y as i32),
            };
            match cua_io::apply(&action, 1.0) {
                Ok(()) => tool_ok(format!("moved to ({x},{y})")),
                Err(e) => tool_err(e),
            }
        }
        "drag" => {
            let coords = (
                args["x"].as_i64(),
                args["y"].as_i64(),
                args["x2"].as_i64(),
                args["y2"].as_i64(),
            );
            let (Some(x), Some(y), Some(x2), Some(y2)) = coords else {
                return tool_err("drag requires integer x, y, x2, y2 (physical pixels)");
            };
            if let Err(e) = ensure_on_screen(x, y).and_then(|_| ensure_on_screen(x2, y2)) {
                return e;
            }
            let action = Action::Drag {
                from: Point::new(x as i32, y as i32),
                to: Point::new(x2 as i32, y2 as i32),
            };
            match cua_io::apply(&action, 1.0) {
                Ok(()) => tool_ok(format!("dragged ({x},{y}) -> ({x2},{y2})")),
                Err(e) => tool_err(e),
            }
        }
        "clipboard_get" => match cua_io::clipboard_get() {
            Ok(text) => tool_ok(text),
            Err(e) => tool_err(e),
        },
        "clipboard_set" => {
            let text = match required_str(args, "text") {
                Ok(t) => t,
                Err(e) => return e,
            };
            match cua_io::clipboard_set(&text) {
                Ok(()) => tool_ok(format!("copied {} chars", text.chars().count())),
                Err(e) => tool_err(e),
            }
        }
        "screen_info" => match cua_io::capture_primary() {
            Ok(screen) => {
                let cursor = cua_io::cursor_position().ok();
                tool_ok(json!({
                    "width": screen.width,
                    "height": screen.height,
                    "origin": { "x": screen.origin.0, "y": screen.origin.1 },
                    "scale_factor": screen.scale_factor,
                    "dpi": cua_io::dpi_scale(),
                    "cursor": cursor.map(|(x, y)| json!({ "x": x, "y": y })),
                }))
            }
            Err(e) => tool_err(e),
        },
        "mouse_down" | "mouse_up" => {
            let button = match args["button"].as_str().unwrap_or("left") {
                "left" => cua_core::MouseButton::Left,
                "right" => cua_core::MouseButton::Right,
                "middle" => cua_core::MouseButton::Middle,
                other => return tool_err(format!("unknown button: {other}")),
            };
            match cua_io::mouse_button(button, name == "mouse_down") {
                Ok(()) => tool_ok(name),
                Err(e) => tool_err(e),
            }
        }
        "key_down" | "key_up" => {
            let combo = match required_str(args, "combo") {
                Ok(c) => c,
                Err(e) => return e,
            };
            match cua_io::key_state(&combo, name == "key_down") {
                Ok(()) => tool_ok(format!("{} {combo}", name)),
                Err(e) => tool_err(e),
            }
        }
        "key" => {
            let combo = match required_str(args, "combo") {
                Ok(c) => c,
                Err(e) => return e,
            };
            match cua_io::apply(
                &Action::Key {
                    combo: combo.clone(),
                },
                1.0,
            ) {
                Ok(()) => tool_ok(format!("sent key {combo}")),
                Err(e) => tool_err(e),
            }
        }
        other => tool_err(format!("unknown tool: {other}")),
    }
}

fn tool_defs() -> Value {
    let mut defs = json!([
        {
            "name": "start_task",
            "description": "Start a task in the background and return a job_id immediately. Poll with task_status; cancel with task_cancel. Arguments match run_task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "max_steps": { "type": "integer", "default": 20 },
                    "dry_run": { "type": "boolean", "default": false },
                    "engine": { "type": "string", "enum": ["vision", "code", "access", "browser"] },
                    "model": { "type": "string" },
                    "yes": { "type": "boolean", "default": false, "description": "Run medium-risk actions without asking. Off by default: an unrecognised command needs a decision." },
                    "allow_high": { "type": "boolean", "default": false },
                    "ui_tree": { "type": "boolean", "default": false },
                    "settle_ms": { "type": "integer", "default": 600 }
                },
                "required": ["task"]
            }
        },
        {
            "name": "task_status",
            "description": "Status of a background task: running/completed/failed/cancelled, step count, summary and artifacts.",
            "inputSchema": {
                "type": "object",
                "properties": { "job_id": { "type": "string" } },
                "required": ["job_id"]
            }
        },
        {
            "name": "task_cancel",
            "description": "Cancel a running background task. Abort is immediate and may interrupt an action already in flight.",
            "inputSchema": {
                "type": "object",
                "properties": { "job_id": { "type": "string" } },
                "required": ["job_id"]
            }
        },
        {
            "name": "run_task",
            "description": "Run a natural-language task on this Windows desktop using the multi-engine agent (vision or code). Requires model credentials in the server environment. Returns a summary, success flag, step count and artifact directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "Goal in natural language" },
                    "max_steps": { "type": "integer", "default": 20 },
                    "dry_run": { "type": "boolean", "default": false, "description": "Plan one step without acting" },
                    "engine": { "type": "string", "enum": ["vision", "code", "access", "browser"] },
                    "model": { "type": "string", "description": "Override the model name" },
                    "yes": { "type": "boolean", "default": false, "description": "Run medium-risk actions without asking. Never allows a destructive one: that needs allow_high or a client that can answer." },
                    "allow_high": { "type": "boolean", "default": false, "description": "Allow destructive commands; off by default" },
                    "ui_tree": { "type": "boolean", "default": false, "description": "Attach the accessibility tree to each vision step" },
                    "settle_ms": { "type": "integer", "default": 600 }
                },
                "required": ["task"]
            }
        },
        {
            "name": "orchestrate",
            "description": "Plan a goal across engines (browser/code/vision), pass outputs between steps, and execute. Use for tasks that combine web reading with desktop or shell work. `dry_run` returns the plan only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": false },
                    "max_plan_steps": { "type": "integer", "default": 6 },
                    "max_steps": { "type": "integer", "default": 10, "description": "Budget per engine sub-run" },
                    "retries": { "type": "integer", "default": 1, "description": "Extra attempts per failed step" },
                    "max_replans": { "type": "integer", "default": 1, "description": "Re-plan attempts after repeated failure" },
                    "model": { "type": "string" },
                    "headed": { "type": "boolean", "default": false },
                    "yes": { "type": "boolean", "default": false, "description": "Run medium-risk steps without asking. The planner chooses the commands, so this is a real concession." },
                    "allow_high": { "type": "boolean", "default": false }
                },
                "required": ["task"]
            }
        },
        {
            "name": "browser_task",
            "description": "Drive a headless Chromium browser through a natural-language task using CDP (navigate, click, type, eval). Runs in an isolated temporary profile. Requires an OpenAI-compatible model key. Every step passes the same confirmation gate as the desktop engines: eval is high risk and needs allow_high, a cross-site navigate, a click on a submit-looking target and any Enter press are medium risk and need yes; a blocked step stops the run and names the switch that would have allowed it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "Goal in natural language" },
                    "start_url": { "type": "string", "description": "Optional page opened before the loop" },
                    "max_steps": { "type": "integer", "default": 15 },
                    "dry_run": { "type": "boolean", "default": false, "description": "Plan one step and report its risk tier without acting" },
                    "yes": { "type": "boolean", "default": false, "description": "Accept medium-risk steps without asking. Never buys a high-risk step" },
                    "allow_high": { "type": "boolean", "default": false, "description": "Permit high-risk steps (arbitrary JavaScript in the page)" },
                    "headed": { "type": "boolean", "default": false, "description": "Show the browser window" },
                    "model": { "type": "string" }
                },
                "required": ["task"]
            }
        },
        {
            "name": "list_runs",
            "description": "List recent recorded runs (newest first) with task, outcome and step count.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "default": 20 },
                    "root": { "type": "string", "default": ".runs" }
                }
            }
        },
        {
            "name": "replay_run",
            "description": "Replay a recorded steps.jsonl. Medium-risk actions need `yes`; destructive ones need `allow_high`. Use `dry_run` to preview.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dir": { "type": "string", "description": "Run directory containing steps.jsonl" },
                    "dry_run": { "type": "boolean", "default": false },
                    "yes": { "type": "boolean", "default": false },
                    "allow_high": { "type": "boolean", "default": false },
                    "max_steps": { "type": "integer" }
                },
                "required": ["dir"]
            }
        },
        {
            "name": "capture_screen",
            "description": "Capture the primary display and return a JPEG image content block plus its dimensions.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_windows",
            "description": "List capturable top-level windows with id, app, title, bounds and focused/minimized flags. Filter by a case-insensitive title/app substring.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Title or app substring" },
                    "include_minimized": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "capture_window",
            "description": "Capture one window by id or title/app selector. Prefers the window's own surface, so content hidden behind another window still comes back; reports the origin, scale and capture source needed to map image coordinates, plus a hash for change detection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Window id from list_windows, or a title/app substring" },
                    "max_edge": { "type": "integer", "default": 1280 },
                    "quality": { "type": "integer", "default": 75 }
                },
                "required": ["window"]
            }
        },
        {
            "name": "window_state",
            "description": "The observation to act on: one call returns the window's screenshot and/or its in-window accessibility tree with element indexes, plus which element has keyboard focus, what text is selected, and the selected controls. Element indexes are stable for a given window, so after the first observation the tree comes back as a unified diff (mode=diff) instead of a full listing; mode=unchanged means your mapping still applies, and mode=baseline_reset means every index you hold is invalid. Menus and dialogs appear in the tree with their own indexes; images that are not the target window carry their own screenshot_id and origin. Returns an observation_id; every element_index or screenshot coordinate you act with must cite it, and a rejected stale_observation means you must re-observe rather than retry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "description": "Window id, selector, or {app,id}", "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 }, { "type": "object", "properties": { "app": { "type": "string" }, "id": { "type": "integer", "minimum": 1 } }, "required": ["id"] } ] },
                    "include_screenshot": { "type": "boolean", "default": true },
                    "include_text": { "type": "boolean", "default": true, "description": "Add the window-scoped accessibility tree with element indexes (defaults to true; retries once on empty/null and explicitly reports coordinate-only mode when unavailable)" },
                    "include_related": { "type": "boolean", "default": false, "description": "Expand full accessibility trees and screenshots for related same-process windows; defaults to false (titles only)" },
                    "disable_diffing": { "type": "boolean", "default": false, "description": "Return the complete tree and rebuild the numbering baseline; use it when you no longer have the baseline needed to apply a diff" },
                    "max_edge": { "type": "integer", "default": 1280 },
                    "quality": { "type": "integer", "default": 75 },
                    "max_nodes": { "type": "integer", "default": 200 },
                    "depth": { "type": "integer", "default": 12 }
                },
                "required": ["window"]
            }
        },
        {
            "name": "list_apps",
            "description": "Applications grouped with the windows they own, plus the launchable id (exe path) for each. include_all adds Start Menu entries for apps that are not running, which is what you need before launch_app can start something closed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_all": { "type": "boolean", "default": false, "description": "Also list installed apps that are not currently running" }
                }
            }
        },
        {
            "name": "launch_app",
            "description": "Start an app and wait for its window, so the next step can be an observation instead of a guess. Accepts an id from list_apps, a full .exe or .lnk path, or an exact display name. An app that is already running is reported with its windows rather than started a second time. ready=false means no window appeared within timeout_ms: run list_windows before retrying instead of launching again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "App id, full .exe/.lnk path, or exact display name" },
                    "timeout_ms": { "type": "integer", "default": 20000, "description": "How long to wait for the first window" }
                },
                "required": ["app"]
            }
        },
        {
            "name": "perform_secondary_action",
            "description": "Invoke an accessibility action a control exposes besides a click: Expand, Collapse, Raise, Scroll Up/Down/Left/Right, Toggle, Select, Focus or Invoke. Only use names listed in that element's actions= in the window_state tree; an unlisted name returns unsupported_action with the real list instead of guessing. The element's state is read back and returned so the action can be verified.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "observation_id": { "type": "string", "description": "Observation the element_index came from" },
                    "element_index": { "type": "integer", "description": "Element index from that observation" },
                    "action": { "type": "string", "description": "Action label as advertised by the element, e.g. Expand, Collapse, Raise, Scroll Down, Toggle, Select, Focus, Invoke" },
                    "window": { "description": "Optional window id or selector when no observation is given", "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 } ] }
                },
                "required": ["element_index", "action"]
            }
        },
        {
            "name": "wait_for_change",
            "description": "Block until the window's pixels differ from a baseline hash (an observation's, an explicit hash, or a capture taken now). This is how an action is verified instead of assumed; changed=false with elapsed_ms means nothing moved within the timeout.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "description": "Window id, selector, or {app,id}", "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 } ] },
                    "observation_id": { "type": "string", "description": "Use this observation's screenshot hash as the baseline" },
                    "hash": { "type": "integer", "description": "Explicit baseline hash" },
                    "timeout_ms": { "type": "integer", "default": 3000 }
                },
                "required": ["window"]
            }
        },
        {
            "name": "focus_window",
            "description": "Bring a window to the foreground by id, exact-one title/app selector, or {app,id}. A selector that matches zero or several windows returns target_not_found/target_ambiguous with candidates instead of guessing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "description": "Window id, a title/app selector that must match exactly one window, or {app,id} from list_windows.",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "integer", "minimum": 1 },
                            {
                                "type": "object",
                                "properties": {
                                    "app": { "type": "string" },
                                    "id": { "oneOf": [{ "type": "string", "pattern": "^[0-9]+$" }, { "type": "integer", "minimum": 1 }] }
                                },
                                "required": ["id"]
                            }
                        ]
                    }
                },
                "required": ["window"]
            }
        },
        {
            "name": "window_input",
            "description": "Atomically focus a window, optionally click a window-relative physical point, type text, and optionally press Return. The window must resolve to exactly one target; ambiguous or missing targets return candidates and never fall back to an unrelated app. The window-relative point is enforced: it has to land inside that window's own rectangle, otherwise the call is refused with offset_outside_window rather than clicking something else, and both axes must be given together. Activation is implemented on Windows only; on other platforms the call answers code=unsupported with retry=false before it touches anything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "description": "Window id, a title/app selector that must match exactly one window, or {app,id} from list_windows.",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "integer", "minimum": 1 },
                            {
                                "type": "object",
                                "properties": {
                                    "app": { "type": "string" },
                                    "id": { "oneOf": [{ "type": "string", "pattern": "^[0-9]+$" }, { "type": "integer", "minimum": 1 }] }
                                },
                                "required": ["id"]
                            }
                        ]
                    },
                    "text": { "type": "string" },
                    "x": { "type": "integer", "description": "Window-relative physical X, measured from the window's own top-left; must be inside the window (0 .. width-1)" },
                    "y": { "type": "integer", "description": "Window-relative physical Y, measured from the window's own top-left; must be inside the window (0 .. height-1). Needs x as well." },
                    "submit": { "type": "boolean", "default": false },
                    "interval_ms": { "type": "integer", "default": 8 }
                },
                "required": ["window", "text"]
            }
        },
        {
            "name": "accessibility_tree",
            "description": "Return a text snapshot of the Windows accessibility tree (window titles, buttons, edits) with rectangles.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_nodes": { "type": "integer", "default": 120 },
                    "depth": { "type": "integer", "default": 2, "description": "Depth limit; deeper trees are slower" }
                }
            }
        },
        {
            "name": "find_control",
            "description": "Find the one control a target names and return its rectangle and center. A target is name (case-insensitive substring, `re:` pattern, or `id:<AutomationId>`) plus optional role, container and nth (0-based). Two matches without nth is an error that lists the candidates, never a pick; the old name-only form still works through the `name` shorthand.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "object",
                        "description": "The one control this call means.",
                        "properties": {
                            "name": { "type": "string", "description": "Case-insensitive substring, `re:` pattern, or `id:<AutomationId>`" },
                            "role": { "type": "string", "description": "Control type it must be, e.g. Button (case-insensitive exact)" },
                            "nth": { "type": "integer", "minimum": 0, "description": "Which of several matches, 0-based; absent means the match must be unique" },
                            "container": { "type": "string", "description": "Only controls inside something whose name matches this" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    },
                    "name": { "type": "string", "description": "Shorthand for target {\"name\": ...}; not together with target" }
                },
                "anyOf": [{ "required": ["target"] }, { "required": ["name"] }]
            }
        },
        {
            "name": "set_control_text",
            "description": "Set a control's text through its Value/EditableText accessibility pattern (no synthetic keystrokes). Use type_text when the control has no such pattern. A target that is ambiguous is an error; a walk that runs out of nodes says it did not reach the control; widen with max_nodes. The `name` shorthand still works.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "object",
                        "description": "The one control this call means.",
                        "properties": {
                            "name": { "type": "string", "description": "Case-insensitive substring, `re:` pattern, or `id:<AutomationId>`" },
                            "role": { "type": "string" },
                            "nth": { "type": "integer", "minimum": 0 },
                            "container": { "type": "string" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    },
                    "name": { "type": "string", "description": "Shorthand for target {\"name\": ...}; not together with target" },
                    "text": { "type": "string" },
                    "max_nodes": { "type": "integer", "minimum": 1 },
                    "max_depth": { "type": "integer", "minimum": 1 }
                },
                "required": ["text"],
                "anyOf": [{ "required": ["target"] }, { "required": ["name"] }]
            }
        },
        {
            "name": "control_state",
            "description": "Read a control's enabled/offscreen flags plus toggle, value, expanded and range values when available. A target that is ambiguous is an error that lists the candidates; the `name` shorthand still works.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "object",
                        "description": "The one control this call means.",
                        "properties": {
                            "name": { "type": "string", "description": "Case-insensitive substring, `re:` pattern, or `id:<AutomationId>`" },
                            "role": { "type": "string" },
                            "nth": { "type": "integer", "minimum": 0 },
                            "container": { "type": "string" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    },
                    "name": { "type": "string", "description": "Shorthand for target {\"name\": ...}; not together with target" }
                },
                "anyOf": [{ "required": ["target"] }, { "required": ["name"] }]
            }
        },
        {
            "name": "invoke_control",
            "description": "Invoke the one control a target names (accessibility action first, click-center fallback). A target that is ambiguous is an error, never a pick; a walk that stopped early reports \"did not reach\" rather than \"no action\"; raise max_nodes. The `name` shorthand still works.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "object",
                        "description": "The one control this call means.",
                        "properties": {
                            "name": { "type": "string", "description": "Case-insensitive substring, `re:` pattern, or `id:<AutomationId>`" },
                            "role": { "type": "string" },
                            "nth": { "type": "integer", "minimum": 0 },
                            "container": { "type": "string" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    },
                    "name": { "type": "string", "description": "Shorthand for target {\"name\": ...}; not together with target" },
                    "max_nodes": { "type": "integer", "minimum": 1 },
                    "max_depth": { "type": "integer", "minimum": 1 }
                },
                "anyOf": [{ "required": ["target"] }, { "required": ["name"] }]
            }
        },
        {
            "name": "invoke_element",
            "description": "Invoke a control by its snapshot-scoped index from accessibility_tree (Invoke pattern first, click-center fallback). Re-read the tree first if the UI changed.",
            "inputSchema": {
                "type": "object",
                "properties": { "index": { "type": "integer" } },
                "required": ["index"]
            }
        },
        {
            "name": "set_element_value",
            "description": "Set the text of a control by its snapshot-scoped index from accessibility_tree. Falls back to nothing if the control has no Value/EditableText pattern.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "index": { "type": "integer" },
                    "text": { "type": "string" }
                },
                "required": ["index", "text"]
            }
        },
        {
            "name": "click",
            "description": "Click a target. Preferred: element_index + observation_id from window_state, which resolves the control and activates its window for you. Coordinates: pass observation_id with x/y in that screenshot's pixels and the server maps them; add screenshot_id to name which image the coordinates came from (needed for popup and menu images, whose origin differs from the window's). Bare x/y are absolute physical pixels and are the last resort.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "observation_id": { "type": "string" },
                    "element_index": { "type": "integer" },
                    "screenshot_id": { "type": "string", "description": "Image id from window_state that x/y belong to; omit for the target window's image" },
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "window": { "description": "Window id or selector, for activation when clicking bare coordinates", "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 } ] },
                    "right": { "type": "boolean", "default": false },
                    "middle": { "type": "boolean", "default": false },
                    "double": { "type": "boolean", "default": false }
                }
            }
        },
        {
            "name": "clipboard_get",
            "description": "Read text from the system clipboard. Returns an error if the clipboard holds a non-text payload.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "clipboard_set",
            "description": "Write text to the system clipboard (persistent instance, so X11 keeps the contents).",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }
        },
        {
            "name": "screen_info",
            "description": "Primary display size, origin, DPI scale and current cursor position. Use it to validate coordinates.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "mouse_down",
            "description": "Press and hold a mouse button without releasing. Pair with mouse_up for hold-drag or modifier-click.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                }
            }
        },
        {
            "name": "mouse_up",
            "description": "Release a mouse button previously held with mouse_down.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                }
            }
        },
        {
            "name": "key_down",
            "description": "Press and hold a key or chord, e.g. `shift` or `ctrl+shift`. Pair with key_up.",
            "inputSchema": {
                "type": "object",
                "properties": { "combo": { "type": "string" } },
                "required": ["combo"]
            }
        },
        {
            "name": "key_up",
            "description": "Release a key or chord previously held with key_down.",
            "inputSchema": {
                "type": "object",
                "properties": { "combo": { "type": "string" } },
                "required": ["combo"]
            }
        },
        {
            "name": "type_text",
            "description": "Type arbitrary Unicode text into the focused window.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "interval_ms": { "type": "integer", "default": 6 }
                },
                "required": ["text"]
            }
        },
        {
            "name": "scroll",
            "description": "DEPRECATED FOR FORMS: Automatically handled inside click/type_text. Use only for exploratory viewing of unaddressable canvas/infinite streams. Positive dy scrolls down, positive dx scrolls right.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "dx": { "type": "integer", "default": 0 },
                    "dy": { "type": "integer", "default": 0 }
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "move",
            "description": "Move the pointer to physical coordinates without clicking.",
            "inputSchema": {
                "type": "object",
                "properties": { "x": { "type": "integer" }, "y": { "type": "integer" } },
                "required": ["x", "y"]
            }
        },
        {
            "name": "drag",
            "description": "Left-button drag from (x,y) to (x2,y2) in physical pixels.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "x2": { "type": "integer" },
                    "y2": { "type": "integer" }
                },
                "required": ["x", "y", "x2", "y2"]
            }
        },
        {
            "name": "key",
            "description": "Send a key chord such as `Return`, `ctrl+l`, `alt+Tab`.",
            "inputSchema": {
                "type": "object",
                "properties": { "combo": { "type": "string" } },
                "required": ["combo"]
            }
        },
        {
            "name": "verify_submission",
            "description": "Pre-submit quality gate for web quizzes/forms: re-reads all checked/selected/filled controls on the page, compares against `planned_answers`, checks coverage (`answered_count`/`total_questions`), and blocks submission if any questions are missing or mismatched.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": {
                        "description": "Window id or selector to re-read before submitting",
                        "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 } ]
                    },
                    "observation_id": { "type": "string", "description": "Fallback observation id when no live window is specified" },
                    "planned_answers": {
                        "description": "Planned answers keyed by question number/label (object or array), compared against actual checked/selected/value state on screen",
                        "oneOf": [ { "type": "object" }, { "type": "array" } ]
                    },
                    "total_questions": { "type": "integer", "minimum": 1, "description": "Total expected question count on the page for coverage verification" }
                },
                "required": ["planned_answers"]
            }
        },
        {
            "name": "resume_chain",
            "description": "Resume a `then` chain that was paused due to non-agent mouse/keyboard activity: performs a fresh observation first and resumes strictly from `last_verified_step + 1` (never blindly continuing from the unverified step or stale plan position).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "resume_token": { "type": "string", "description": "Token returned in `resume_token` when the chain paused" },
                    "window": {
                        "description": "Optional window id or selector to re-observe before resuming",
                        "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 } ]
                    }
                },
                "required": ["resume_token"]
            }
        }
    ]);
    add_chain_controls(&mut defs);
    defs
}

/// Advertise the chain controls on every chainable tool. Declared once here
/// rather than copied into 30 schemas: a tool added later is chainable by
/// default, and the schema cannot drift out of step with the executor.
fn add_chain_controls(defs: &mut Value) {
    let then_schema = json!({
        "type": "array",
        "maxItems": MAX_CHAIN_STEPS,
        "description": "Deterministic actions to run right after this one, in order, without another round trip: [{\"do\":\"<tool>\", ...its arguments}]. `observation_id` and `window` carry into any step that omits them. The chain stops at the first failure and still returns the trace plus the resulting state, so what comes back is what actually ran. Model-driven tools (run_task, orchestrate, browser_task) cannot be steps.",
        "items": {
            "type": "object",
            "properties": { "do": { "type": "string", "description": "Tool to run as this step" } },
            "required": ["do"]
        }
    });
    let observe_schema = json!({
        "description": "State returned after the chain: true (default) reads window_state once, diffed and without a screenshot; false returns the trace only; an object is passed straight to window_state, e.g. {\"include_screenshot\":true} to also get the image.",
        "oneOf": [ { "type": "boolean" }, { "type": "object" } ]
    });
    let settle_schema = json!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_SETTLE_MS,
        "default": DEFAULT_SETTLE_MS,
        "description": "Milliseconds to wait between chain steps, so the previous action has landed. Only used with `then`."
    });

    let question_schema = json!({
        "description": "Question identifier/number for bounded per-question retry tracking; stops after N consecutive failures on the same question.",
        "oneOf": [ { "type": "string" }, { "type": "integer", "minimum": 1 } ]
    });
    let max_retries_schema = json!({
        "type": "integer",
        "minimum": 1,
        "default": DEFAULT_MAX_QUESTION_RETRIES,
        "description": "Maximum consecutive failed attempts allowed on the same question before stopping and reporting the question number."
    });
    let planned_answers_schema = json!({
        "description": "When passed on a submit action, runs full-page pre-submit answer and coverage verification before allowing the action.",
        "oneOf": [ { "type": "object" }, { "type": "array" } ]
    });

    let Some(tools) = defs.as_array_mut() else {
        return;
    };
    for tool in tools.iter_mut() {
        let Some(tool) = tool.as_object_mut() else {
            continue;
        };
        let name = tool["name"].as_str().unwrap_or_default();
        if !chainable(name) {
            continue;
        };
        let Some(schema) = tool
            .entry("inputSchema")
            .or_insert_with(|| json!({ "type": "object" }))
            .as_object_mut()
        else {
            continue;
        };
        let Some(properties) = schema
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
        else {
            continue;
        };
        properties.insert("then".to_string(), then_schema.clone());
        properties.insert("observe".to_string(), observe_schema.clone());
        properties.insert("settle_ms".to_string(), settle_schema.clone());
        properties
            .entry("question".to_string())
            .or_insert_with(|| question_schema.clone());
        properties
            .entry("max_retries".to_string())
            .or_insert_with(|| max_retries_schema.clone());
        properties
            .entry("planned_answers".to_string())
            .or_insert_with(|| planned_answers_schema.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(x: i32, y: i32, width: u32, height: u32) -> cua_io::WindowInfo {
        cua_io::WindowInfo {
            id: 42,
            pid: 7,
            app: "probe".into(),
            title: "cua-probe".into(),
            x,
            y,
            width,
            height,
            minimized: false,
            focused: true,
        }
    }

    #[test]
    fn a_window_relative_point_stays_inside_the_window() {
        let win = window(100, 200, 400, 300);
        // Inside, including the first pixel of each edge.
        assert_eq!(
            window_relative_point(&win, Some(0), Some(0)).unwrap(),
            Some((100, 200))
        );
        assert_eq!(
            window_relative_point(&win, Some(399), Some(299)).unwrap(),
            Some((499, 499))
        );
        // No point at all is the documented "just type into the focused window".
        assert_eq!(window_relative_point(&win, None, None).unwrap(), None);
    }

    #[test]
    fn an_offset_that_would_escape_the_window_is_refused() {
        let win = window(100, 200, 400, 300);
        for (x, y) in [(400, 0), (0, 300), (-1, 10), (10, -1), (5000, 5000)] {
            let refused = window_relative_point(&win, Some(x), Some(y)).unwrap_err();
            assert_eq!(refused["isError"], true, "({x},{y}) must not be clicked");
            let text = refused["content"][0]["text"].as_str().unwrap_or_default();
            assert!(
                text.contains("offset_outside_window"),
                "the caller needs the code, not a prose apology: {text}"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_window_it_compared_against() {
        let win = window(100, 200, 400, 300);
        let text = window_relative_point(&win, Some(900), Some(10)).unwrap_err()["content"][0]
            ["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(text.contains("(900,10)"), "say which offset: {text}");
        assert!(text.contains("400x300"), "say against what: {text}");
        assert!(text.contains("cua-probe"), "say which window: {text}");
    }

    #[test]
    fn half_a_point_is_not_a_click() {
        // Before the fix, x alone was silently ignored and the text still went out
        // under an answer that read like the aiming had happened.
        let win = window(0, 0, 100, 100);
        for pair in [(Some(5), None), (None, Some(5))] {
            let refused = window_relative_point(&win, pair.0, pair.1).unwrap_err();
            let text = refused["content"][0]["text"].as_str().unwrap_or_default();
            assert!(text.contains("half_a_point"), "{text}");
        }
    }

    #[test]
    fn a_huge_offset_cannot_wrap_through_i32() {
        // `as i32` would fold 2^32+50 back to 50 -- inside the window -- after the
        // check above, which is the shape of the escape this test pins closed.
        let win = window(0, 0, 100, 100);
        assert!(window_relative_point(&win, Some(1 << 32), Some(1 << 32)).is_err());
        // A window sitting against the far edge of the coordinate space cannot
        // produce an absolute point the input layer can address.
        let far = window(i32::MAX - 50, 0, 400, 400);
        let refused = window_relative_point(&far, Some(300), Some(10)).unwrap_err();
        let text = refused["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("outside the coordinate range"), "{text}");
    }

    #[test]
    fn a_degenerate_control_rectangle_has_no_centre() {
        // (0,0,0,0) is what a toolkit answers for a control it never laid out. Its
        // "centre" is the top-left pixel of the screen, and invoke_control used to
        // click exactly there.
        assert_eq!(control_center((0, 0, 0, 0)), None);
        assert_eq!(control_center((12, 8, 0, 40)), None);
        assert_eq!(control_center((12, 8, 40, -3)), None);
        assert_eq!(control_center((10, 10, 20, 8)), Some((20, 14)));
    }

    #[test]
    fn an_unauthenticated_remote_bind_says_so_out_loud() {
        let token: Option<String> = None;
        let notice = unauthenticated_remote_notice("0.0.0.0:8765", false, &token)
            .expect("a remote bind without a token must warn");
        assert!(notice.contains("NO authentication"), "{notice}");
        assert!(notice.contains("0.0.0.0:8765"), "name the port at risk");
        assert!(notice.contains("CUA_HUB_MCP_TOKEN"), "and the way out");
    }

    #[test]
    fn a_local_or_token_protected_bind_is_not_warned_about() {
        // Flooding a false warning onto every normal start would train the operator
        // to scroll past the one that matters.
        let token: Option<String> = None;
        assert!(unauthenticated_remote_notice("127.0.0.1:8765", true, &token).is_none());
        let guarded = Some("secret".to_string());
        assert!(unauthenticated_remote_notice("0.0.0.0:8765", false, &guarded).is_none());
    }

    #[test]
    fn responses_carry_envelope() {
        let r = success_response(json!(7), json!({"ok": true}));
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["ok"], true);
    }

    #[test]
    fn tool_errors_are_results_not_protocol_errors() {
        let e = tool_err("boom");
        assert_eq!(e["isError"], true);
        assert_eq!(e["content"][0]["type"], "text");
    }

    fn test_window(id: u32, app: &str, title: &str) -> cua_io::WindowInfo {
        cua_io::WindowInfo {
            id,
            pid: id,
            app: app.to_string(),
            title: title.to_string(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            minimized: false,
            focused: false,
        }
    }

    #[test]
    fn window_selector_requires_one_match() {
        let windows = vec![
            test_window(1, "FreeCAD", "* Unnamed - FreeCAD"),
            test_window(2, "FreeCAD", "gui_block - FreeCAD"),
            test_window(3, "Notepad", "Untitled"),
        ];

        assert!(matches!(pick_window(&windows, "Untitled"), WindowPick::One(w) if w.id == 3));
        assert!(matches!(pick_window(&windows, "gui_block"), WindowPick::One(w) if w.id == 2));
        assert!(matches!(
            pick_window(&windows, "FreeCAD"),
            WindowPick::Many(_)
        ));
        assert!(matches!(pick_window(&windows, "chrome"), WindowPick::None));
    }

    #[test]
    fn window_arg_accepts_id_forms() {
        assert_eq!(window_arg_id(&json!(42)), Some(42));
        assert_eq!(window_arg_id(&json!("42")), Some(42));
        assert_eq!(window_arg_id(&json!({"id": "42"})), Some(42));
        assert_eq!(window_arg_id(&json!({"id": 42})), Some(42));
        assert_eq!(window_arg_id(&json!({"app": "FreeCAD"})), None);
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let req = json!({"jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {}});
        let resp = handle_request(req).await.unwrap();
        assert_eq!(resp["id"], 9);
        assert_eq!(resp["result"]["serverInfo"]["name"], "cua-hub");
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let req = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_request(req).await.is_none());
    }

    #[test]
    fn tool_list_includes_run_task_and_capture() {
        let names: Vec<String> = tool_defs()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"run_task".to_string()));
        assert!(names.contains(&"orchestrate".to_string()));
        assert!(names.contains(&"browser_task".to_string()));
        assert!(names.contains(&"capture_screen".to_string()));
        assert!(names.contains(&"list_windows".to_string()));
        assert!(names.contains(&"capture_window".to_string()));
        assert!(names.contains(&"window_state".to_string()));
        assert!(names.contains(&"wait_for_change".to_string()));
        assert!(names.contains(&"focus_window".to_string()));
        assert!(names.contains(&"window_input".to_string()));
        assert!(names.contains(&"invoke_element".to_string()));
        assert!(names.contains(&"set_element_value".to_string()));
        assert!(names.contains(&"list_runs".to_string()));
        assert!(names.contains(&"replay_run".to_string()));
        assert!(names.contains(&"type_text".to_string()));
        assert!(names.contains(&"scroll".to_string()));
        assert!(names.contains(&"drag".to_string()));
        assert!(names.contains(&"clipboard_get".to_string()));
        assert!(names.contains(&"mouse_down".to_string()));
        assert!(names.contains(&"screen_info".to_string()));
        assert!(names.contains(&"set_control_text".to_string()));
        assert!(names.contains(&"control_state".to_string()));
        assert!(names.contains(&"start_task".to_string()));
        assert!(names.contains(&"task_status".to_string()));
        assert!(names.contains(&"invoke_control".to_string()));
    }

    #[test]
    fn tool_list_includes_app_and_secondary_action_tools() {
        let names: Vec<String> = tool_defs()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"list_apps".to_string()));
        assert!(names.contains(&"launch_app".to_string()));
        assert!(names.contains(&"perform_secondary_action".to_string()));
    }

    #[test]
    fn a_popup_already_shown_in_the_tree_is_not_read_twice() {
        // The UIA node for an open menu, and the popup HWNDs for the same menu
        // plus its shadow.
        let shown = [(863, 210, 183, 54), (730, 148, 808, 729)];
        assert!(covered_by(&shown, (863, 210, 183, 54)), "identical rect");
        assert!(
            covered_by(&shown, (863, 210, 188, 59)),
            "menu with its shadow border"
        );
        assert!(
            !covered_by(&shown, (40, 40, 300, 200)),
            "an unrelated window"
        );
        assert!(covered_by(&shown, (0, 0, 0, 0)), "an empty rect");
    }

    #[test]
    fn window_state_exposes_diff_control() {
        let defs = tool_defs();
        let def = defs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "window_state")
            .expect("window_state should be declared");
        assert!(def["inputSchema"]["properties"]["disable_diffing"].is_object());
        assert!(def["description"]
            .as_str()
            .expect("description")
            .contains("baseline_reset"));
    }

    #[test]
    fn every_tool_is_described_with_an_object_schema() {
        for tool in tool_defs().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap_or("<unnamed>");
            assert!(
                tool["description"].as_str().is_some_and(|d| d.len() > 20),
                "{name} needs a description a model can act on"
            );
            assert_eq!(tool["inputSchema"]["type"], "object", "{name} schema");
        }
    }

    fn first_json_text(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().expect("a text block");
        serde_json::from_str(text).expect("the chain summary should be JSON")
    }

    #[test]
    fn chain_controls_are_advertised_on_primitives_only() {
        for tool in tool_defs().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap_or("<unnamed>");
            let has_then = tool["inputSchema"]["properties"]["then"].is_object();
            assert_eq!(
                has_then,
                chainable(name),
                "{name} should advertise `then` exactly when it can chain"
            );
        }
        assert!(!chainable("run_task"));
        assert!(chainable("click"));
    }

    #[tokio::test]
    async fn initialize_tells_the_caller_chains_exist() {
        let response = handle_request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        }))
        .await
        .expect("initialize gets a response");
        let instructions = response["result"]["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("then:"),
            "the server should state that actions can be chained"
        );
    }

    #[tokio::test]
    async fn chain_validates_the_whole_sequence_before_running_anything() {
        // The head would fail anyway, but the point is that the answer names the
        // bad step: an invalid step seven must not perform actions one to six.
        let result = call_tool(
            "invoke_control",
            &json!({ "then": [{ "do": "run_task", "task": "x" }] }),
        )
        .await;
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("step 0") && text.contains("run_task"),
            "should blame the step, got {text}"
        );
        assert!(
            !text.contains("no control"),
            "the head must not have run, got {text}"
        );
    }

    #[tokio::test]
    async fn chain_rejects_a_nested_then_instead_of_nesting() {
        let result = call_tool(
            "invoke_control",
            &json!({ "then": [{ "do": "invoke_control", "then": [] }] }),
        )
        .await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("flat"));
    }

    #[tokio::test]
    async fn chain_stops_at_the_first_failing_action() {
        // `type_text` without text fails before touching the keyboard, so this
        // asserts the stop behaviour without driving the machine.
        let result = call_tool(
            "type_text",
            &json!({
                "observe": false,
                "then": [{ "do": "key", "combo": "Return" }]
            }),
        )
        .await;
        assert_eq!(result["isError"], true);
        let summary = first_json_text(&result);
        assert_eq!(summary["ok"], false);
        assert_eq!(summary["ran"], 1);
        assert_eq!(summary["planned"], 2);
        assert_eq!(summary["stopped_at"], 0);
        assert_eq!(summary["chain"].as_array().unwrap().len(), 1);
        assert!(summary["error"]
            .as_str()
            .unwrap()
            .contains("missing string argument"));
    }

    #[tokio::test]
    async fn chain_step_limit_is_enforced() {
        let steps: Vec<Value> = (0..MAX_CHAIN_STEPS + 1)
            .map(|_| json!({ "do": "key", "combo": "Return" }))
            .collect();
        let result = call_tool("key", &json!({ "then": steps })).await;
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("at most"));
    }

    #[test]
    fn chain_observation_follows_the_last_window_named() {
        let observed = observations::record(observations::Observation {
            id: observations::next_id(),
            window_id: 9_999,
            hash: None,
            shots: Vec::new(),
            nodes: Vec::new(),
            node_windows: Vec::new(),
        });
        let head = json!({ "observation_id": observed });
        let last = json!({ "window": 4321u32 });
        assert_eq!(
            chain_window(vec![head, json!({ "window": 7u32 }), last]),
            Some(4321),
            "a step that names its own window decides where to look next"
        );
        assert_eq!(
            chain_window(vec![json!({}), json!({ "observation_id": observed })]),
            Some(9_999),
            "otherwise the observed window is the one to re-read"
        );
        assert_eq!(chain_window(vec![json!({})]), None);
    }

    #[test]
    fn step_record_counts_images_without_returning_them() {
        let record = step_record(
            "window_state",
            &json!({
                "content": [
                    { "type": "image", "data": "AAAA", "mimeType": "image/jpeg" },
                    { "type": "text", "text": "the tree" }
                ],
                "isError": false
            }),
        );
        assert_eq!(record["do"], "window_state");
        assert_eq!(record["ok"], true);
        assert_eq!(record["detail"], "the tree");
        assert_eq!(record["images_dropped"], 1);
        assert!(serde_json::to_string(&record).unwrap().len() < 200);
    }

    #[test]
    fn model_driven_entry_points_default_to_asking() {
        // Both of these let a model choose the command, so silence cannot mean
        // consent: a caller has to pass `yes`, or be able to answer a prompt.
        let run = run_options_from_args(&json!({ "task": "x" })).expect("run options");
        assert!(!run.yes, "run_task must not default to auto-yes");
        assert!(!run.allow_high);
        let orchestrated =
            orchestrate_options_from_args(&json!({ "task": "x" })).expect("orchestrate options");
        assert!(
            !orchestrated.yes,
            "orchestrate must not default to auto-yes"
        );
        assert!(!orchestrated.allow_high);
    }

    #[test]
    fn consent_is_honoured_when_it_is_given() {
        let run = run_options_from_args(&json!({ "task": "x", "yes": true, "allow_high": true }))
            .expect("run options");
        assert!(run.yes);
        assert!(run.allow_high);
        let orchestrated = orchestrate_options_from_args(&json!({ "task": "x", "yes": true }))
            .expect("orchestrate options");
        assert!(orchestrated.yes);
    }

    fn sample_ui_node(index: usize, control_type: &str, name: &str) -> cua_engine_access::UiNode {
        cua_engine_access::UiNode {
            index,
            depth: 0,
            control_type: control_type.into(),
            name: name.into(),
            automation_id: String::new(),
            rect: (10, 20, 80, 30),
            signature: format!("/{index}:{control_type}"),
            value: String::new(),
            actions: Vec::new(),
            focused: false,
            selected: false,
            checked: None,
        }
    }

    #[test]
    fn accessibility_null_retries_once_and_recovers_or_degrades_to_coordinate_only() {
        let mut attempts = 0usize;
        let mut recovered = 0usize;
        let recovered_attempt = snapshot_with_retry(
            || {
                attempts += 1;
                if attempts == 1 {
                    Ok(cua_engine_access::UiSnapshot {
                        nodes: Vec::new(),
                        truncated: false,
                        unreadable: Vec::new(),
                    })
                } else {
                    Ok(cua_engine_access::UiSnapshot {
                        nodes: vec![sample_ui_node(0, "Window", "Quiz")],
                        truncated: false,
                        unreadable: Vec::new(),
                    })
                }
            },
            || {
                recovered += 1;
            },
        );
        assert_eq!(
            attempts, 2,
            "must retry once when initial snapshot is empty"
        );
        assert_eq!(
            recovered, 1,
            "must invoke client rebuild/refocus before retry"
        );
        match recovered_attempt {
            SnapshotAttempt::Available { snapshot, retried } => {
                assert!(retried);
                assert_eq!(snapshot.nodes.len(), 1);
            }
            other => panic!("expected recovered snapshot, got {other:?}"),
        }

        let mut fail_attempts = 0usize;
        let mut fail_recovered = 0usize;
        let degraded = snapshot_with_retry(
            || {
                fail_attempts += 1;
                Err(cua_core::Error::Capture("UIA root unavailable".into()))
            },
            || {
                fail_recovered += 1;
            },
        );
        assert_eq!(fail_attempts, 2);
        assert_eq!(fail_recovered, 1);
        match degraded {
            SnapshotAttempt::CoordinateOnly { retried, reason } => {
                assert!(retried);
                let payload = coordinate_only_accessibility(retried, &reason);
                assert!(
                    !payload.is_null(),
                    "accessibility must never be silent null"
                );
                assert_eq!(payload["mode"], "coordinate-only");
                assert_eq!(payload["available"], false);
                assert_eq!(payload["retried"], true);
                assert!(payload["notice"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("coordinate-only"));
            }
            other => panic!("expected coordinate-only fallback, got {other:?}"),
        }
    }

    #[test]
    fn related_windows_default_to_titles_only_until_include_related_is_true() {
        let defs = tool_defs();
        let def = defs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "window_state")
            .expect("window_state tool");
        assert_eq!(
            def["inputSchema"]["properties"]["include_related"]["default"],
            json!(false)
        );
        assert_eq!(
            def["inputSchema"]["properties"]["include_text"]["default"],
            json!(true)
        );

        let related = vec![
            test_window(91, "browser", "Confirm Submission"),
            test_window(92, "browser", "Helper Tooltip"),
        ];
        let titles = related_title_summaries(&related);
        assert_eq!(
            titles,
            vec![
                json!({ "id": 91, "title": "Confirm Submission" }),
                json!({ "id": 92, "title": "Helper Tooltip" }),
            ]
        );
        let tree_with_titles =
            append_related_titles_only("[0] Window \"Quiz\"\n".to_string(), &related);
        assert!(tree_with_titles.contains("# related window 91 \"Confirm Submission\"\n"));
        assert!(tree_with_titles.contains("# related window 92 \"Helper Tooltip\"\n"));
        assert!(!tree_with_titles.contains("[1]"));
    }

    #[test]
    fn submission_quality_gate_reports_missing_questions_and_answer_differences() {
        let mut q1 = sample_ui_node(1, "Group", "1. Rust ownership");
        q1.depth = 1;
        let mut q1_opt_a = sample_ui_node(2, "RadioButton", "A. Borrow checker");
        q1_opt_a.depth = 2;
        q1_opt_a.checked = Some(false);
        let mut q1_opt_b = sample_ui_node(3, "RadioButton", "B. Garbage collector");
        q1_opt_b.depth = 2;
        q1_opt_b.checked = Some(true);

        let mut q2 = sample_ui_node(4, "Group", "2. Memory safety");
        q2.depth = 1;
        let mut q2_opt_true = sample_ui_node(5, "RadioButton", "True");
        q2_opt_true.depth = 2;
        q2_opt_true.checked = Some(true);

        let mut q3 = sample_ui_node(6, "Group", "3. Fill in the blank");
        q3.depth = 1;
        let mut q3_edit = sample_ui_node(7, "Edit", "Question 3 answer");
        q3_edit.depth = 2;
        q3_edit.value = String::new();

        let nodes = vec![q1, q1_opt_a, q1_opt_b, q2, q2_opt_true.clone(), q3, q3_edit];
        let planned = json!({
            "1": "A",
            "2": "True",
            "3": "Result<T, E>",
            "4": "B"
        });

        let audit = audit_submission_nodes(&nodes, &planned, Some(4));
        assert!(
            !audit.passed,
            "audit must fail when questions are missing or mismatched"
        );
        assert_eq!(audit.total_questions, 4);
        assert_eq!(audit.answered_count, 2);
        assert_eq!(
            audit.missing_questions,
            vec!["3".to_string(), "4".to_string()]
        );
        assert_eq!(audit.differences.len(), 3);
        assert_eq!(audit.differences[0]["question"], json!("1"));

        // Verify that verify_submission tool returns an error listing missing_questions & differences,
        // and passes once all questions match the plan.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let failed_gate = rt.block_on(call_tool_single(
            "verify_submission",
            &json!({
                "nodes": nodes,
                "planned_answers": planned,
                "total_questions": 4
            }),
        ));
        assert_eq!(failed_gate["isError"], json!(true));
        let failed_body: Value =
            serde_json::from_str(failed_gate["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(failed_body["ok"], json!(false));
        assert_eq!(failed_body["can_submit"], json!(false));
        assert_eq!(
            failed_body["coverage"]["missing_questions"],
            json!(["3", "4"])
        );
        assert_eq!(failed_body["differences"].as_array().unwrap().len(), 3);

        // Fix answers and verify gate passes.
        let mut fixed_q1_a = sample_ui_node(2, "RadioButton", "A. Borrow checker");
        fixed_q1_a.depth = 2;
        fixed_q1_a.checked = Some(true);
        let mut fixed_q3_edit = sample_ui_node(7, "Edit", "Question 3 answer");
        fixed_q3_edit.depth = 2;
        fixed_q3_edit.value = "Result<T, E>".into();
        let fixed_nodes = vec![
            sample_ui_node(1, "Group", "1. Rust ownership"),
            fixed_q1_a,
            sample_ui_node(4, "Group", "2. Memory safety"),
            q2_opt_true,
            sample_ui_node(6, "Group", "3. Fill in the blank"),
            fixed_q3_edit,
        ];
        let passed_gate = rt.block_on(call_tool_single(
            "verify_submission",
            &json!({
                "nodes": fixed_nodes,
                "planned_answers": {
                    "1": "A. Borrow checker",
                    "2": "True",
                    "3": "Result<T, E>"
                },
                "total_questions": 3
            }),
        ));
        assert_eq!(passed_gate["isError"], json!(false));
        let passed_body: Value =
            serde_json::from_str(passed_gate["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(passed_body["ok"], json!(true));
        assert_eq!(passed_body["can_submit"], json!(true));
        assert_eq!(passed_body["coverage"]["answered"], json!(3));
        assert_eq!(passed_body["coverage"]["total"], json!(3));
        assert_eq!(passed_body["coverage"]["missing_questions"], json!([]));
        assert_eq!(passed_body["differences"], json!([]));
    }

    #[test]
    fn bounded_question_retry_stops_after_n_failures_and_reports_question_number() {
        reset_question_retries();

        // Fail twice on question 7 with max_retries = 2.
        assert!(record_question_attempt("7", false, 2).is_ok());
        let err = record_question_attempt("7", false, 2)
            .expect_err("second failure on question 7 must hit retry limit 2");
        assert_eq!(err["isError"], json!(true));
        let body: Value =
            serde_json::from_str(err["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["error"]["code"], json!("retry_limit_exceeded"));
        assert_eq!(body["error"]["question"], json!("7"));
        assert_eq!(body["error"]["attempts"], json!(2));
        assert_eq!(body["error"]["max_retries"], json!(2));

        // Any subsequent attempt on question 7 is blocked before execution.
        let blocked = check_question_retry_budget("7", 2)
            .expect_err("further attempts on exhausted question must be blocked");
        let blocked_body: Value =
            serde_json::from_str(blocked["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(blocked_body["error"]["code"], json!("retry_limit_exceeded"));
        assert_eq!(blocked_body["error"]["question"], json!("7"));

        // A different question is unaffected, and a success resets its counter.
        assert!(record_question_attempt("8", false, 2).is_ok());
        assert!(record_question_attempt("8", true, 2).is_ok());
        assert!(record_question_attempt("8", false, 2).is_ok());
        reset_question_retries();
    }

    #[test]
    fn verification_crop_only_fires_on_unverifiable_and_crops_target_neighborhood() {
        let mut node = sample_ui_node(12, "RadioButton", "Option C");
        node.rect = (200, 300, 80, 30);

        assert!(!should_capture_verification_crop(
            cua_engine_access::Effect::Changed
        ));
        assert!(!should_capture_verification_crop(
            cua_engine_access::Effect::Unchanged
        ));
        assert!(should_capture_verification_crop(
            cua_engine_access::Effect::Unverifiable
        ));

        // Screen is 1920x1080; target is (200, 300, 80, 30).
        // Crop must be a small bounded box around the target, never the full 1920x1080 screen.
        let (cx, cy, cw, ch) =
            compute_target_crop_bounds((0, 0), (1920, 1080), node.rect, TARGET_CROP_MARGIN_PX)
                .expect("valid crop bounds");
        assert!(cw <= 400 && ch <= 400);
        assert!(cw < 1920 && ch < 1080);
        assert!(cx <= 200 && cy <= 300);
        assert!(cx + cw >= 280 && cy + ch >= 330);

        // Verify RGBA sub-region extraction + PNG encoding produces a valid small PNG.
        let full_w = 100u32;
        let full_h = 100u32;
        let raw = vec![128u8; (full_w * full_h * 4) as usize];
        let cropped =
            crop_rgba_region(&raw, full_w, full_h, (10, 20, 24, 16)).expect("valid rgba crop");
        assert_eq!(cropped.len(), (24 * 16 * 4) as usize);
        let png = encode_rgba_png(24, 16, &cropped);
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn external_input_pauses_chain_with_step_number_and_resumes_from_last_verified_effect() {
        reset_concurrency_state();
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Build a 3-step chain:
        // - Step 0 (head): verified effect (`verified_effect: true`)
        // - Step 1 (`then[0]`): unverified effect (`verified_effect: false`)
        // - Step 2 (`then[1]`): external mouse/keyboard input detected before step 2 runs!
        //
        // When external input occurs before Step 2:
        // 1. Chain must pause immediately and report `stopped_at: 2`, `last_verified_step: 0`.
        // 2. When resumed via `resume_chain`, it must take a fresh observation (`fresh_observation_id`)
        //    and resume from step 1 (`last_verified_step + 1`), NOT blindly from step 2 (`stopped_at`)!
        notify_external_input(2, "user moved mouse during quiz chain");

        let stale_obs = observations::record(observations::Observation {
            id: observations::next_id(),
            window_id: 77,
            hash: None,
            shots: Vec::new(),
            nodes: Vec::new(),
            node_windows: Vec::new(),
        });
        let paused_response = rt.block_on(call_tool(
            "verify_submission",
            &json!({
                "nodes": [],
                "planned_answers": {},
                "total_questions": 0,
                "verified_effect": true,
                "settle_ms": 0,
                "observe": false,
                "then": [
                    {
                        "do": "verify_submission",
                        "nodes": [],
                        "planned_answers": {},
                        "total_questions": 0,
                        "verified_effect": false
                    },
                    {
                        "do": "verify_submission",
                        "nodes": [],
                        "planned_answers": {},
                        "total_questions": 0,
                        "verified_effect": true
                    }
                ]
            }),
        ));

        assert_eq!(paused_response["isError"], json!(true));
        let paused_summary: Value =
            serde_json::from_str(paused_response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(paused_summary["ok"], json!(false));
        assert_eq!(paused_summary["paused"], json!(true));
        assert_eq!(
            paused_summary["pause_reason"],
            json!("external_input_detected")
        );
        assert_eq!(paused_summary["stopped_at"], json!(2));
        assert_eq!(paused_summary["last_verified_step"], json!(0));
        assert_eq!(paused_summary["resume_from_step"], json!(1));
        assert_eq!(paused_summary["ran"], json!(2));
        assert_eq!(paused_summary["planned"], json!(3));

        let resume_token = paused_summary["resume_token"]
            .as_str()
            .expect("resume_token must be returned")
            .to_string();

        // Resume the paused chain with fresh nodes for the new observation.
        let mut fresh_node = sample_ui_node(0, "RadioButton", "Option A");
        fresh_node.signature = "/0:RadioButton#opt-a".into();
        let resumed_response = rt.block_on(call_tool(
            "resume_chain",
            &json!({
                "resume_token": resume_token,
                "fresh_nodes": [fresh_node]
            }),
        ));

        assert_eq!(resumed_response["isError"], json!(false));
        let resumed_summary: Value =
            serde_json::from_str(resumed_response["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(resumed_summary["ok"], json!(true));
        assert_eq!(resumed_summary["resumed"], json!(true));
        assert_eq!(
            resumed_summary["resumed_from_step"],
            json!(1),
            "must resume from last_verified_step + 1 (step 1), never blindly from stopped_at (step 2)"
        );
        assert_eq!(resumed_summary["previous_stopped_at"], json!(2));
        let fresh_obs_id = resumed_summary["fresh_observation_id"]
            .as_str()
            .expect("fresh_observation_id must be minted on resume");
        assert_ne!(
            fresh_obs_id, stale_obs,
            "resume must create a brand-new observation instead of reusing the stale one"
        );
        // Steps 1 and 2 must both have run during the resume.
        let chain = resumed_summary["chain"].as_array().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0]["step"], json!(1));
        assert_eq!(chain[1]["step"], json!(2));
        reset_concurrency_state();
    }

    #[test]
    fn pre_action_foreground_check_refocuses_and_reresolves_when_window_lost_focus() {
        let mut stale_node = sample_ui_node(5, "RadioButton", "A. Ownership");
        stale_node.signature = "/0:Group/1:RadioButton#opt-a".into();
        stale_node.rect = (100, 100, 80, 24);

        // Case 1: Window is already in the foreground -> no refocus or re-parse needed.
        let mut focus_calls = 0usize;
        let mut snap_calls = 0usize;
        let already_fg = ensure_foreground_and_reresolve_with(
            42,
            &stale_node,
            |_| true,
            |_| {
                focus_calls += 1;
                Ok(())
            },
            |_| {
                snap_calls += 1;
                Ok(cua_engine_access::UiSnapshot {
                    nodes: Vec::new(),
                    truncated: false,
                    unreadable: Vec::new(),
                })
            },
        )
        .expect("foreground window succeeds");
        assert!(already_fg.was_foreground);
        assert!(!already_fg.refocused);
        assert!(!already_fg.reresolved);
        assert_eq!(focus_calls, 0);
        assert_eq!(snap_calls, 0);

        // Case 2: Window lost foreground status -> must refocus AND re-resolve target node
        // from a fresh snapshot (picking up its new moved rectangle).
        let mut moved_node = stale_node.clone();
        moved_node.index = 99;
        moved_node.rect = (340, 520, 80, 24);

        let recovered = ensure_foreground_and_reresolve_with(
            42,
            &stale_node,
            |_| false,
            |win_id| {
                assert_eq!(win_id, 42);
                focus_calls += 1;
                Ok(())
            },
            |win_id| {
                assert_eq!(win_id, 42);
                snap_calls += 1;
                Ok(cua_engine_access::UiSnapshot {
                    nodes: vec![sample_ui_node(0, "Window", "Quiz"), moved_node.clone()],
                    truncated: false,
                    unreadable: Vec::new(),
                })
            },
        )
        .expect("refocused and reresolved");
        assert!(!recovered.was_foreground);
        assert!(recovered.refocused);
        assert!(recovered.reresolved);
        assert_eq!(focus_calls, 1);
        assert_eq!(snap_calls, 1);
        assert_eq!(
            recovered.node.rect,
            (340, 520, 80, 24),
            "re-resolved node must carry updated rectangle from fresh snapshot"
        );
        assert_eq!(recovered.node.index, 5);
    }

    fn error_text(value: &Value) -> &str {
        value["content"][0]["text"]
            .as_str()
            .expect("tool errors carry text")
    }

    #[test]
    fn a_target_and_a_name_together_are_refused() {
        // Double addressing is the failure this stage exists to remove: one of
        // the two would silently win and the caller would never learn which.
        let err = target_arg(&json!({
            "target": {"name": "ok"},
            "name": "ok"
        }))
        .expect_err("both addresses must be refused");
        assert!(
            error_text(&err).contains("not both"),
            "{}",
            error_text(&err)
        );
    }

    #[test]
    fn the_name_shorthand_is_still_a_complete_target() {
        let target = target_arg(&json!({"name": "probe-toggle"})).expect("shorthand resolves");
        assert_eq!(
            target,
            cua_engine_access::target::Target::by_name("probe-toggle")
        );
    }

    #[test]
    fn a_missing_address_says_what_to_send() {
        let err = target_arg(&json!({})).expect_err("neither address is not a target");
        assert!(
            error_text(&err).contains("missing `target`"),
            "{}",
            error_text(&err)
        );
    }

    #[test]
    fn a_target_refuses_the_fields_that_double_address_it() {
        // `index` inside a target is the stale-index shortcut in new clothing.
        let err = target_arg(&json!({"target": {"name": "ok", "index": 4}}))
            .expect_err("unknown fields must not be ignored");
        assert!(
            error_text(&err).contains("index"),
            "say which field was refused: {}",
            error_text(&err)
        );
    }

    #[tokio::test]
    async fn wait_for_change_clamps_timeout_above_60000ms() {
        let windows = cua_io::list_windows(None, true).unwrap_or_default();
        if let Some(win) = windows.first() {
            let resp = call_tool(
                "wait_for_change",
                &json!({
                    "window": win.id,
                    "hash": 999_999_999_u64,
                    "timeout_ms": 120_000,
                }),
            )
            .await;
            assert_eq!(resp["isError"], false);
            let text = resp["content"][0]["text"].as_str().unwrap();
            let parsed: Value = serde_json::from_str(text).unwrap();
            assert_eq!(parsed["ok"], true);
            assert_eq!(parsed["changed"], true);
            assert_eq!(parsed["clamped"], true);
            assert_eq!(parsed["requested_timeout_ms"], 120_000);
            assert!(parsed["elapsed_ms"].is_number());
            assert!(parsed["hash"].is_number());
        }
    }

    #[tokio::test]
    async fn wait_for_change_normal_timeout_does_not_clamp() {
        let windows = cua_io::list_windows(None, true).unwrap_or_default();
        if let Some(win) = windows.first() {
            let resp = call_tool(
                "wait_for_change",
                &json!({
                    "window": win.id,
                    "hash": 999_999_999_u64,
                    "timeout_ms": 5_000,
                }),
            )
            .await;
            assert_eq!(resp["isError"], false);
            let text = resp["content"][0]["text"].as_str().unwrap();
            let parsed: Value = serde_json::from_str(text).unwrap();
            assert_eq!(parsed["ok"], true);
            assert_eq!(parsed["changed"], true);
            assert!(parsed.get("clamped").is_none());
            assert!(parsed.get("requested_timeout_ms").is_none());
        }
    }
}
