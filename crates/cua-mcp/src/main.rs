//! cua-mcp：Computer Use 的 MCP server（stdio 传输，JSON-RPC 2.0）。
//!
//! 定位：只做"手"，不做"脑"。把 cua-runtime 的截屏/键鼠能力包装成 MCP 工具，
//! 任何 MCP 客户端（Claude Code / Qoder / Cursor …）里的模型都能直接驱动桌面，
//! 不需要为本程序单独开通任何模型 API key。
//!
//! 安全：所有工具调用都经 cua-runtime 的 Policy 围栏并写审计日志。
//! - `--mode dry-run`：全部只审计不执行
//! - `--mode supervised`（默认）：Safe 动作（截屏/移动/等待）放行，
//!   Input/Risky 动作一律拒绝（stdio 被协议占用，无法交互确认）
//! - `--mode autonomous`：全部放行；其中 launch_app 还需 `--allow-launch`
//!   显式解锁（它能拉起任意程序）
//!
//! 注意：stdout 是 MCP 协议通道，日志只能写 stderr。

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use cua_runtime::guard::DenyConfirmer;
use cua_runtime::{AutoConfirmer, Policy, RunMode, Runtime};
use cua_types::{Action, MouseButton};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cua-mcp", about = "Computer Use MCP server（stdio）")]
struct Args {
    /// 运行模式（默认 supervised：输入类动作拒绝，需显式 autonomous 才放行）
    #[arg(long, value_enum, default_value_t = Mode::Supervised)]
    mode: Mode,
    /// autonomous 模式下允许 launch_app 工具（拉起任意程序，高风险）
    #[arg(long, default_value_t = false)]
    allow_launch: bool,
    /// 审计日志路径（JSONL）
    #[arg(long, default_value = "cua-mcp-audit.jsonl")]
    audit: PathBuf,
    /// 截图输出目录
    #[arg(long, default_value = "cua-mcp-screenshots")]
    shots: PathBuf,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    DryRun,
    Supervised,
    Autonomous,
}

impl From<Mode> for RunMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::DryRun => RunMode::DryRun,
            Mode::Supervised => RunMode::Supervised,
            Mode::Autonomous => RunMode::Autonomous,
        }
    }
}

struct Server {
    runtime: Runtime,
    mode: Mode,
    allow_launch: bool,
    auto: AutoConfirmer,
    deny: DenyConfirmer,
}

impl Server {
    fn handle(&mut self, req: &Value) -> Option<Value> {
        let method = req.get("method")?.as_str()?.to_string();
        let id = req.get("id").cloned();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // 无 id 的是通知：MCP 约定不回响应。
        let id = id?;

        let result: Result<Value> = match method.as_str() {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => Ok(self.call_tool(&params)),
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                }))
            }
        };

        Some(match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": format!("{e:#}") }
            }),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        let proto = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("2024-11-05");
        json!({
            "protocolVersion": proto,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "cua-mcp", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Computer Use tools: call screenshot to see the screen, \
                             then click / type_text / key_press to act. Coordinates are \
                             absolute pixels with (0,0) at the top-left."
        })
    }

    fn call_tool(&mut self, params: &Value) -> Value {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let action = match build_action(name, &args) {
            Ok(a) => a,
            Err(msg) => return tool_result(vec![text_content(&msg)], true),
        };

        // launch_app 是 Risky 动作中的最高档：autonomous 之外还要显式解锁。
        if matches!(action, Action::LaunchApp { .. }) && !self.allow_launch {
            return tool_result(
                vec![text_content(
                    "launch_app is locked: restart cua-mcp with --allow-launch to enable it",
                )],
                true,
            );
        }

        let describe = action.describe();
        // confirmer 与 runtime 是 Server 的不同字段，内联取用以满足借用检查。
        let outcome = match self.mode {
            Mode::Supervised => self.runtime.execute(&action, &mut self.deny),
            _ => self.runtime.execute(&action, &mut self.auto),
        };
        match outcome {
            Ok(outcome) if outcome.executed => {
                if let Some(obs) = outcome.observation {
                    // 截屏：把 PNG 以 MCP image 内容回给大脑。
                    let mut content = Vec::new();
                    if let Some(path) = &obs.screenshot_path {
                        match std::fs::read(path) {
                            Ok(bytes) => content.push(json!({
                                "type": "image",
                                "data": base64_encode(&bytes),
                                "mimeType": "image/png"
                            })),
                            Err(e) => {
                                return tool_result(
                                    vec![text_content(&format!(
                                        "screenshot saved but unreadable: {e}"
                                    ))],
                                    true,
                                )
                            }
                        }
                    }
                    content.push(text_content(&format!(
                        "screenshot {}x{} taken at {}",
                        obs.width,
                        obs.height,
                        obs.screenshot_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    )));
                    tool_result(content, false)
                } else {
                    tool_result(vec![text_content(&format!("{describe} -> executed"))], false)
                }
            }
            Ok(_) => tool_result(
                vec![text_content(&format!(
                    "{describe} -> SKIPPED by guard (mode={:?}); \
                     input actions need --mode autonomous",
                    self.mode
                ))],
                false,
            ),
            Err(e) => tool_result(vec![text_content(&format!("{describe} -> error: {e:#}"))], true),
        }
    }
}

fn text_content(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

fn tool_result(content: Vec<Value>, is_error: bool) -> Value {
    json!({ "content": content, "isError": is_error })
}

fn build_action(name: &str, args: &Value) -> Result<Action, String> {
    let req_i = |k: &str| -> Result<i32, String> {
        args.get(k)
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .ok_or_else(|| format!("tool {name}: missing integer argument '{k}'"))
    };
    let opt_i = |k: &str| args.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);

    Ok(match name {
        "screenshot" => Action::Screenshot,
        "mouse_move" => Action::MouseMove {
            x: req_i("x")?,
            y: req_i("y")?,
        },
        "click" => {
            let button = match args.get("button").and_then(|v| v.as_str()).unwrap_or("left") {
                "left" => MouseButton::Left,
                "right" => MouseButton::Right,
                "middle" => MouseButton::Middle,
                other => return Err(format!("tool click: unknown button '{other}'")),
            };
            let at = match (opt_i("x"), opt_i("y")) {
                (Some(x), Some(y)) => Some((x, y)),
                (None, None) => None,
                _ => return Err("tool click: x and y must be given together".to_string()),
            };
            Action::Click { button, at }
        }
        "double_click" => Action::DoubleClick {
            at: (req_i("x")?, req_i("y")?),
        },
        "scroll" => Action::Scroll {
            dx: opt_i("dx").unwrap_or(0),
            dy: opt_i("dy").unwrap_or(0),
        },
        "drag" => Action::Drag {
            from: (req_i("from_x")?, req_i("from_y")?),
            to: (req_i("to_x")?, req_i("to_y")?),
        },
        "type_text" => {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("tool type_text: missing string argument 'text'")?;
            Action::TypeText {
                text: text.to_string(),
            }
        }
        "key_press" => {
            let combo = args
                .get("combo")
                .and_then(|v| v.as_str())
                .ok_or("tool key_press: missing string argument 'combo'")?;
            Action::KeyPress {
                combo: combo.to_string(),
            }
        }
        "wait" => {
            let ms = args
                .get("ms")
                .and_then(|v| v.as_u64())
                .ok_or("tool wait: missing integer argument 'ms'")?;
            Action::Wait { ms }
        }
        "launch_app" => {
            let program = args
                .get("program")
                .and_then(|v| v.as_str())
                .ok_or("tool launch_app: missing string argument 'program'")?;
            let argv: Vec<String> = args
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Action::LaunchApp {
                program: program.to_string(),
                args: argv,
            }
        }
        other => return Err(format!("unknown tool: {other}")),
    })
}

fn tool_defs() -> Value {
    let obj = |props: Value, required: Value| {
        json!({ "type": "object", "properties": props, "required": required })
    };
    let xy = json!({ "x": { "type": "integer" }, "y": { "type": "integer" } });
    json!([
        {
            "name": "screenshot",
            "description": "Take a screenshot of the current desktop. Returns the image plus screen size. Always start by looking at the screen.",
            "inputSchema": obj(json!({}), json!([]))
        },
        {
            "name": "mouse_move",
            "description": "Move the mouse cursor to absolute pixel coordinates (0,0 = top-left).",
            "inputSchema": obj(xy.clone(), json!(["x", "y"]))
        },
        {
            "name": "click",
            "description": "Click a mouse button. Give x and y to click at that point, or omit both to click at the current cursor position.",
            "inputSchema": obj(
                json!({
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" }
                }),
                json!([])
            )
        },
        {
            "name": "double_click",
            "description": "Double-click the left mouse button at the given coordinates.",
            "inputSchema": obj(xy.clone(), json!(["x", "y"]))
        },
        {
            "name": "scroll",
            "description": "Scroll the wheel. dy positive scrolls down, dx positive scrolls right.",
            "inputSchema": obj(
                json!({ "dx": { "type": "integer", "default": 0 }, "dy": { "type": "integer", "default": 0 } }),
                json!([])
            )
        },
        {
            "name": "drag",
            "description": "Drag with the left button held, from one point to another.",
            "inputSchema": obj(
                json!({
                    "from_x": { "type": "integer" }, "from_y": { "type": "integer" },
                    "to_x": { "type": "integer" }, "to_y": { "type": "integer" }
                }),
                json!(["from_x", "from_y", "to_x", "to_y"])
            )
        },
        {
            "name": "type_text",
            "description": "Type a string of text at the current keyboard focus.",
            "inputSchema": obj(json!({ "text": { "type": "string" } }), json!(["text"]))
        },
        {
            "name": "key_press",
            "description": "Press a key or combination, e.g. \"Return\", \"Tab\", \"ctrl+l\", \"alt+F4\".",
            "inputSchema": obj(json!({ "combo": { "type": "string" } }), json!(["combo"]))
        },
        {
            "name": "wait",
            "description": "Wait for the given number of milliseconds (e.g. for a window to open).",
            "inputSchema": obj(json!({ "ms": { "type": "integer", "minimum": 0 } }), json!(["ms"]))
        },
        {
            "name": "launch_app",
            "description": "Launch a program by name/path with arguments. High risk; only available when the server was started with --allow-launch.",
            "inputSchema": obj(
                json!({
                    "program": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" }, "default": [] }
                }),
                json!(["program"])
            )
        }
    ])
}

/// 标准 base64 编码（带 padding），避免为一个函数引入新依赖。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let policy = Policy {
        mode: args.mode.into(),
    };
    let runtime = Runtime::new(policy, args.audit.clone(), args.shots.clone())
        .context("init runtime (needs an X11 session; is DISPLAY set?)")?;
    let mut server = Server {
        runtime,
        mode: args.mode,
        allow_launch: args.allow_launch,
        auto: AutoConfirmer,
        deny: DenyConfirmer,
    };
    tracing::info!(mode = ?args.mode, allow_launch = args.allow_launch, "cua-mcp ready");

    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock()).lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in lines.by_ref() {
        let line = line.context("read stdin")?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                writeln!(out, "{resp}").context("write stdout")?;
                out.flush().context("flush stdout")?;
                continue;
            }
        };
        if let Some(resp) = server.handle(&req) {
            writeln!(out, "{resp}").context("write stdout")?;
            out.flush().context("flush stdout")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc4648() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn build_actions() {
        let a = build_action("click", &json!({ "x": 3, "y": 4 })).unwrap();
        assert_eq!(
            a,
            Action::Click {
                button: MouseButton::Left,
                at: Some((3, 4))
            }
        );
        assert!(build_action("click", &json!({ "x": 3 })).is_err());
        assert!(build_action("nope", &json!({})).is_err());
    }
}
