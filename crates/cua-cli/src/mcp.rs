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
    Some(Arc::new(move |action| {
        let elicitor = elicitor.clone();
        Box::pin(async move {
            let description = runner::describe(&action);
            elicitor
                .ask(&format!("Approve high-risk action? {description}"))
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
        yes: args["yes"].as_bool().unwrap_or(true),
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
                "instructions": "Tools to see and control this desktop. Prefer accessibility_tree + invoke_control for precise work; use run_task for natural-language goals."
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

fn required_str(args: &Value, key: &str) -> std::result::Result<String, Value> {
    args[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| tool_err(format!("missing string argument `{key}`")))
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

async fn call_tool(name: &str, args: &Value) -> Value {
    match name {
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
            let task = match required_str(args, "task") {
                Ok(t) => t,
                Err(e) => return e,
            };
            let opts = crate::orchestrator::OrchestrateOptions {
                task,
                dry_run: args["dry_run"].as_bool().unwrap_or(false),
                max_plan_steps: args["max_plan_steps"].as_u64().unwrap_or(6) as usize,
                max_steps_per_engine: args["max_steps"].as_u64().unwrap_or(10) as u32,
                retries_per_step: args["retries"].as_u64().unwrap_or(1) as u32,
                max_replans: args["max_replans"].as_u64().unwrap_or(1) as usize,
                model: args["model"].as_str().map(str::to_string),
                headed: args["headed"].as_bool().unwrap_or(false),
                yes: true,
                allow_high: args["allow_high"].as_bool().unwrap_or(false),
                run_dir: None,
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
        "capture_window" => {
            let selector = match required_str(args, "window") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let max_edge = args["max_edge"].as_u64().unwrap_or(1280) as u32;
            let quality = args["quality"].as_u64().unwrap_or(75) as u8;
            match cua_io::capture_window(&selector, max_edge, quality) {
                Ok(cap) => {
                    use base64::Engine as _;
                    let data = base64::engine::general_purpose::STANDARD
                        .encode(&cap.frame.observation.image);
                    let meta = json!({
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
                        "mapping": "screen = origin + image * image_scale",
                    });
                    tool_ok(json!([
                        { "type": "image", "data": data, "mimeType": cap.frame.observation.media_type },
                        { "type": "text", "text": meta.to_string() }
                    ]))
                }
                Err(e) => tool_err(e),
            }
        }
        "focus_window" => {
            let selector = match required_str(args, "window") {
                Ok(v) => v,
                Err(e) => return e,
            };
            let wanted_id = selector.parse::<u32>().ok();
            let windows = if wanted_id.is_some() {
                cua_io::list_windows(None, true)
            } else {
                cua_io::list_windows(Some(&selector), true)
            };
            match windows {
                Ok(windows) => {
                    let chosen = wanted_id
                        .and_then(|id| windows.iter().find(|w| w.id == id))
                        .or_else(|| {
                            windows.iter().find(|w| {
                                w.title.eq_ignore_ascii_case(&selector)
                                    || w.app.eq_ignore_ascii_case(&selector)
                            })
                        })
                        .or_else(|| windows.first());
                    match chosen {
                        Some(window) => match cua_engine_access::focus_window(window.id) {
                            Ok(()) => tool_ok(json!({
                                "id": window.id,
                                "title": window.title,
                                "app": window.app,
                            })),
                            Err(e) => tool_err(e),
                        },
                        None => tool_err(format!("no window matches `{selector}`")),
                    }
                }
                Err(e) => tool_err(e),
            }
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
            let name = match required_str(args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            match cua_engine_access::find(&name, 400, 8) {
                Ok(Some(node)) => tool_ok(json!({
                    "name": node.name,
                    "control_type": node.control_type,
                    "automation_id": node.automation_id,
                    "rect": { "x": node.rect.0, "y": node.rect.1, "w": node.rect.2, "h": node.rect.3 },
                    "center": { "x": node.rect.0 + node.rect.2 / 2, "y": node.rect.1 + node.rect.3 / 2 }
                })),
                Ok(None) => tool_err(format!("no control matching {name:?}")),
                Err(e) => tool_err(e),
            }
        }
        "set_control_text" => {
            let name = match required_str(args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            let text = match required_str(args, "text") {
                Ok(t) => t,
                Err(e) => return e,
            };
            match cua_engine_access::set_value(&name, &text) {
                Ok(Some(node)) => tool_ok(format!(
                    "set text of {} \"{}\" ({} chars)",
                    node.control_type,
                    node.name,
                    text.chars().count()
                )),
                Ok(None) => tool_err(format!(
                    "control {name:?} has no Value/EditableText pattern; focus it and use type_text"
                )),
                Err(e) => tool_err(e),
            }
        }
        "control_state" => {
            let name = match required_str(args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            match cua_engine_access::state(&name) {
                Ok(Some(state)) => tool_ok(json!(state)),
                Ok(None) => tool_err(format!("no state available for control {name:?}")),
                Err(e) => tool_err(e),
            }
        }
        "invoke_element" => {
            let index = args["index"].as_u64().unwrap_or(u64::MAX) as usize;
            match cua_engine_access::node_at(index, 400, 8) {
                Ok(Some(node)) => {
                    let Some(selector) = selector_for_node(&node) else {
                        return tool_err(format!(
                            "control at index {index} has no accessible name or automation id"
                        ));
                    };
                    match cua_engine_access::invoke(&selector) {
                        Ok(Some(invoked)) => {
                            return tool_ok(format!(
                                "invoked {} \"{}\" via accessibility pattern",
                                invoked.control_type, invoked.name
                            ))
                        }
                        Ok(None) => {}
                        Err(e) => return tool_err(e),
                    }
                    let (x, y, w, h) = node.rect;
                    if w <= 0 || h <= 0 {
                        return tool_err(format!(
                            "control at index {index} has no usable rectangle"
                        ));
                    }
                    let action = Action::Click {
                        at: Point::new(x + w / 2, y + h / 2),
                        button: cua_core::MouseButton::Left,
                        double: false,
                    };
                    match cua_io::apply(&action, 1.0) {
                        Ok(()) => tool_ok(format!(
                            "clicked {} \"{}\" at ({},{})",
                            node.control_type,
                            node.name,
                            x + w / 2,
                            y + h / 2
                        )),
                        Err(e) => tool_err(e),
                    }
                }
                Ok(None) => tool_err(format!(
                    "no control at index {index}; re-read accessibility_tree"
                )),
                Err(e) => tool_err(e),
            }
        }
        "set_element_value" => {
            let index = args["index"].as_u64().unwrap_or(u64::MAX) as usize;
            let text = match required_str(args, "text") {
                Ok(v) => v,
                Err(e) => return e,
            };
            match cua_engine_access::node_at(index, 400, 8) {
                Ok(Some(node)) => {
                    let Some(selector) = selector_for_node(&node) else {
                        return tool_err(format!(
                            "control at index {index} has no accessible name or automation id"
                        ));
                    };
                    match cua_engine_access::set_value(&selector, &text) {
                        Ok(Some(set)) => tool_ok(format!(
                            "set {} \"{}\" via Value/EditableText",
                            set.control_type, set.name
                        )),
                        Ok(None) => tool_err(format!(
                            "control at index {index} has no Value/EditableText pattern; use type_text"
                        )),
                        Err(e) => tool_err(e),
                    }
                }
                Ok(None) => tool_err(format!(
                    "no control at index {index}; re-read accessibility_tree"
                )),
                Err(e) => tool_err(e),
            }
        }
        "invoke_control" => {
            let name = match required_str(args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
            // Prefer the accessibility Invoke pattern: no pointer movement and
            // immune to overlapping windows.
            match cua_engine_access::invoke(&name) {
                Ok(Some(node)) => {
                    return tool_ok(format!(
                        "invoked {} \"{}\" via accessibility pattern",
                        node.control_type, node.name
                    ))
                }
                Ok(None) => {}
                Err(e) => return tool_err(e),
            }
            match cua_engine_access::find(&name, 400, 8) {
                Ok(Some(node)) => {
                    let (x, y, w, h) = node.rect;
                    let action = Action::Click {
                        at: Point::new(x + w / 2, y + h / 2),
                        button: cua_core::MouseButton::Left,
                        double: false,
                    };
                    match cua_io::apply(&action, 1.0) {
                        Ok(()) => tool_ok(format!(
                            "clicked {} \"{}\" at ({},{})",
                            node.control_type,
                            node.name,
                            x + w / 2,
                            y + h / 2
                        )),
                        Err(e) => tool_err(e),
                    }
                }
                Ok(None) => tool_err(format!("no control matching {name:?}")),
                Err(e) => tool_err(e),
            }
        }
        "click" => {
            let (Some(x), Some(y)) = (args["x"].as_i64(), args["y"].as_i64()) else {
                return tool_err("click requires integer x and y (physical pixels)");
            };
            if let Err(e) = ensure_on_screen(x, y) {
                return e;
            }
            let button = if args["middle"].as_bool().unwrap_or(false) {
                cua_core::MouseButton::Middle
            } else if args["right"].as_bool().unwrap_or(false) {
                cua_core::MouseButton::Right
            } else {
                cua_core::MouseButton::Left
            };
            let action = Action::Click {
                at: Point::new(x as i32, y as i32),
                button,
                double: args["double"].as_bool().unwrap_or(false),
            };
            match cua_io::apply(&action, 1.0 / cua_io::dpi_scale()) {
                Ok(()) => tool_ok(format!("clicked ({x},{y})")),
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
            match cua_io::apply(&action, 1.0 / cua_io::dpi_scale()) {
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
            match cua_io::apply(&action, 1.0 / cua_io::dpi_scale()) {
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
            match cua_io::apply(&action, 1.0 / cua_io::dpi_scale()) {
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
    json!([
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
                    "yes": { "type": "boolean", "default": true },
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
                    "yes": { "type": "boolean", "default": true, "description": "Allow medium-risk actions" },
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
                    "allow_high": { "type": "boolean", "default": false }
                },
                "required": ["task"]
            }
        },
        {
            "name": "browser_task",
            "description": "Drive a headless Chromium browser through a natural-language task using CDP (navigate, click, type, eval). Runs in an isolated temporary profile. Requires an OpenAI-compatible model key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "Goal in natural language" },
                    "start_url": { "type": "string", "description": "Optional page opened before the loop" },
                    "max_steps": { "type": "integer", "default": 15 },
                    "dry_run": { "type": "boolean", "default": false },
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
            "description": "Capture one window by id or title/app selector. Window-scoped capture still sees content covered by other windows; returns the origin and scale needed to map image coordinates back to the screen.",
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
            "name": "focus_window",
            "description": "Bring a window to the foreground by id or title/app. Call this before typing or clicking when another app may have stolen focus.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Window id from list_windows, or a title/app substring" }
                },
                "required": ["window"]
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
            "description": "Find a control by exact accessible name and return its rectangle and center. Prefers buttons/menu items over same-named containers.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "set_control_text",
            "description": "Set a control's text through its Value/EditableText accessibility pattern (no synthetic keystrokes). Use type_text when the control has no such pattern.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["name", "text"]
            }
        },
        {
            "name": "control_state",
            "description": "Read a control's enabled/offscreen flags plus toggle, value, expanded and range values when available.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "invoke_control",
            "description": "Invoke a control by accessible name (Invoke pattern first, click-center fallback).",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
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
            "description": "Click at physical screen pixels. Prefer invoke_control when the target has an accessible name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "right": { "type": "boolean", "default": false },
                    "middle": { "type": "boolean", "default": false },
                    "double": { "type": "boolean", "default": false }
                },
                "required": ["x", "y"]
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
            "description": "Scroll the wheel at physical coordinates. Positive dy scrolls down, positive dx scrolls right.",
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
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(names.contains(&"focus_window".to_string()));
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
}
