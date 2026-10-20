//! Minimal MCP server: newline-delimited JSON-RPC 2.0 over stdio, plus a
//! streamable-HTTP style POST endpoint (`/mcp`) for remote or containerised
//! clients.
//!
//! Exposes the machine as tools so other agents can drive it: run a task,
//! capture the screen, read the accessibility tree, find/invoke controls, and
//! inject mouse/keyboard input.

use std::io::{BufRead, Write};

use anyhow::Result;
use cua_core::{Action, EngineKind, Point};
use serde_json::{json, Value};

use crate::runner::{self, RunOptions};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn serve() -> Result<()> {
    // Read stdin on a dedicated blocking thread: tokio's stdin on Windows does
    // not reliably wake for messages that arrive after the first read while
    // the pipe stays open (verified with an MCP client: requests 1-2 answered,
    // request 3 hung until EOF).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    while let Some(line) = rx.recv().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = error_response(Value::Null, -32700, &format!("parse error: {e}"));
                write_msg(&resp)?;
                continue;
            }
        };

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

fn required_str(args: &Value, key: &str) -> std::result::Result<String, Value> {
    args[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| tool_err(format!("missing string argument `{key}`")))
}

async fn call_tool(name: &str, args: &Value) -> Value {
    match name {
        "run_task" => {
            let task = match required_str(args, "task") {
                Ok(t) => t,
                Err(e) => return e,
            };
            let engine = match args["engine"].as_str() {
                Some("vision") => Some(EngineKind::Vision),
                Some("code") => Some(EngineKind::Code),
                Some("access") => Some(EngineKind::Access),
                Some("browser") => Some(EngineKind::Browser),
                _ => None,
            };
            let opts = RunOptions {
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
        "invoke_control" => {
            let name = match required_str(args, "name") {
                Ok(n) => n,
                Err(e) => return e,
            };
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
            let action = Action::Click {
                at: Point::new(x as i32, y as i32),
                button: if args["right"].as_bool().unwrap_or(false) {
                    cua_core::MouseButton::Right
                } else {
                    cua_core::MouseButton::Left
                },
                double: args["double"].as_bool().unwrap_or(false),
            };
            match cua_io::apply(&action, 1.0 / cua_io::dpi_scale()) {
                Ok(()) => tool_ok(format!("clicked ({x},{y})")),
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
            "name": "invoke_control",
            "description": "Find a control by accessible name and click its center.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
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
                    "double": { "type": "boolean", "default": false }
                },
                "required": ["x", "y"]
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
        assert!(names.contains(&"list_runs".to_string()));
        assert!(names.contains(&"replay_run".to_string()));
        assert!(names.contains(&"invoke_control".to_string()));
    }
}
