use anyhow::Result;
use axum::{routing::post, Json, Router};
use base64::Engine;
use clap::Parser;
use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings, Keyboard};
use image::{codecs::jpeg::JpegEncoder, DynamicImage};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use xcap::Monitor;

// ==================== 底层系统驱动 ====================
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum Action {
    MouseMove { x: i32, y: i32 },
    LeftClick { x: i32, y: i32 },
    Type { text: String },
}

pub struct CuaDriver {
    enigo: Enigo,
}

impl CuaDriver {
    pub fn new() -> Result<Self> {
        let enigo = Enigo::new(&Settings::default()).map_err(|e| anyhow::anyhow!("{:?}", e))?;
        Ok(Self { enigo })
    }

    pub fn take_screenshot_base64(&self) -> Result<String> {
        let monitors = Monitor::all()?;
        let primary = monitors.into_iter().find(|m| m.is_primary()).unwrap();
        let image_buffer = primary.capture_image()?;

        let dyn_img = DynamicImage::ImageRgba8(image_buffer);

        let mut jpeg_bytes = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_bytes, 80);
        encoder.encode(dyn_img.as_bytes(), dyn_img.width(), dyn_img.height(), dyn_img.color().into())?;

        Ok(base64::engine::general_purpose::STANDARD.encode(jpeg_bytes))
    }

    pub fn execute_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::MouseMove { x, y } => self.enigo.move_mouse(x, y, Coordinate::Abs).unwrap_or(()),
            Action::LeftClick { x, y } => {
                self.enigo.move_mouse(x, y, Coordinate::Abs).unwrap_or(());
                std::thread::sleep(std::time::Duration::from_millis(50));
                self.enigo.button(Button::Left, Direction::Click).unwrap_or(());
            }
            Action::Type { text } => self.enigo.text(&text).unwrap_or(()),
        }
        Ok(())
    }
}

// ==================== HTTP API 模式 ====================
#[derive(Deserialize)]
struct HttpActionReq { action: Action }

#[derive(Serialize)]
struct HttpRes { status: String, data: Option<String>, error: Option<String> }

async fn handle_action(
    axum::extract::State(driver): axum::extract::State<Arc<Mutex<CuaDriver>>>,
    Json(payload): Json<HttpActionReq>,
) -> Json<HttpRes> {
    let mut d = driver.lock().await;
    match d.execute_action(payload.action) {
        Ok(_) => Json(HttpRes { status: "success".to_string(), data: None, error: None }),
        Err(e) => Json(HttpRes { status: "error".to_string(), data: None, error: Some(e.to_string()) }),
    }
}

async fn handle_screenshot(
    axum::extract::State(driver): axum::extract::State<Arc<Mutex<CuaDriver>>>,
) -> Json<HttpRes> {
    let d = driver.lock().await;
    match d.take_screenshot_base64() {
        Ok(b64) => Json(HttpRes { status: "success".to_string(), data: Some(b64), error: None }),
        Err(e) => Json(HttpRes { status: "error".to_string(), data: None, error: Some(e.to_string()) }),
    }
}

async fn run_http_server(port: u16) -> Result<()> {
    let driver = Arc::new(Mutex::new(CuaDriver::new()?));
    let app = Router::new()
        .route("/api/v1/action", post(handle_action))
        .route("/api/v1/screenshot", post(handle_screenshot))
        .with_state(driver);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ==================== MCP 协议模式 ====================
fn run_mcp_stdio_loop() -> Result<()> {
    let mut driver = CuaDriver::new()?;
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = req["method"].as_str().unwrap_or("");
        let id = req["id"].clone();
        let mut response = serde_json::json!({ "jsonrpc": "2.0", "id": id });

        match method {
            "take_screenshot" => {
                match driver.take_screenshot_base64() {
                    Ok(b64) => response["result"] = serde_json::json!({ "base64": b64 }),
                    Err(e) => response["error"] = serde_json::json!({ "code": -32603, "message": e.to_string() }),
                }
            }
            "execute_action" => {
                if let Ok(action) = serde_json::from_value::<Action>(req["params"]["action"].clone()) {
                    match driver.execute_action(action) {
                        Ok(_) => response["result"] = serde_json::json!({ "success": true }),
                        Err(e) => response["error"] = serde_json::json!({ "code": -32603, "message": e.to_string() }),
                    }
                } else {
                    response["error"] = serde_json::json!({ "code": -32602, "message": "Invalid params" });
                }
            }
            _ => { response["error"] = serde_json::json!({ "code": -32601, "message": "Method not found" }); }
        }

        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
    Ok(())
}

// ==================== 引擎入口 ====================
#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value = "http")]
    mode: String,
    #[arg(short, long, default_value_t = 3030)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.mode.as_str() {
        "http" => run_http_server(args.port).await?,
        "mcp" => run_mcp_stdio_loop()?,
        _ => std::process::exit(1),
    }
    Ok(())
}
