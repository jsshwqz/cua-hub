//! Anthropic Computer Use provider and the vision engine wrapper.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use cua_core::{
    Action, Engine, EngineKind, Error, MouseButton, Observation, Point, Result, StepRecord,
};
use serde_json::{json, Value};

const API_VERSION: &str = "2023-06-01";
const COMPUTER_BETA: &str = "computer-use-2025-01-24";

/// Configuration for the Anthropic provider.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

impl AnthropicConfig {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| Error::Config("ANTHROPIC_API_KEY is not set".into()))?;
        Ok(Self {
            api_key,
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into()),
            model: std::env::var("CUA_HUB_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".into()),
            max_tokens: 2048,
        })
    }
}

/// Wraps the vision engine into the generic [`Engine`] trait.
pub struct VisionEngine {
    provider: AnthropicProvider,
}

impl VisionEngine {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            provider: AnthropicProvider::new(config),
        }
    }
}

#[async_trait]
impl Engine for VisionEngine {
    fn name(&self) -> &'static str {
        "anthropic-computer-use"
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Vision
    }

    async fn next_action(
        &mut self,
        task: &str,
        observation: &Observation,
        _history: &[StepRecord],
    ) -> Result<Action> {
        self.provider.next(task, observation).await
    }

    async fn on_action_result(&mut self, record: &StepRecord) {
        self.provider.note_result(record);
    }
}

/// Stateful Anthropic Messages API client implementing the computer tool loop.
pub struct AnthropicProvider {
    http: reqwest::Client,
    config: AnthropicConfig,
    messages: Vec<Value>,
    display: (u32, u32),
    /// Tool uses awaiting a result, as (tool_use_id, parsed action).
    pending: VecDeque<(String, Action)>,
    /// Extra tool_use ids from the same turn that we intentionally skip.
    skipped: Vec<String>,
    /// Result note for the tool use we just executed.
    pending_note: Option<String>,
    started: bool,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            http,
            config,
            messages: Vec::new(),
            display: (0, 0),
            pending: VecDeque::new(),
            skipped: Vec::new(),
            pending_note: None,
            started: false,
        }
    }

    /// Record the outcome of the last executed action for the next tool_result.
    pub fn note_result(&mut self, record: &StepRecord) {
        if let Some(note) = &record.note {
            self.pending_note = Some(note.clone());
        }
    }

    pub async fn next(&mut self, task: &str, obs: &Observation) -> Result<Action> {
        if !self.started {
            self.display = (obs.width, obs.height);
            self.messages.push(json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": format!("TASK: {task}") },
                    image_block(obs)
                ]
            }));
            self.started = true;
        } else if let Some((tool_use_id, _)) = self.pending.pop_front() {
            // Attach the screenshot and any execution note to the tool result,
            // and acknowledge any tool uses we deliberately skipped.
            let note = self.pending_note.take().unwrap_or_else(|| "done".into());
            let mut blocks = vec![json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": [
                    { "type": "text", "text": note },
                    image_block(obs)
                ]
            })];
            for id in self.skipped.drain(..) {
                blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": "skipped: only one action per step is executed"
                }));
            }
            self.messages
                .push(json!({ "role": "user", "content": blocks }));
        } else if matches!(
            self.messages.last().and_then(|m| m["role"].as_str()),
            Some("assistant")
        ) {
            self.messages.push(json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "Continue with the task. Current screen attached." },
                    image_block(obs)
                ]
            }));
        }

        let body = self.request_body();
        let response = self.send_with_retry(&body).await?;

        let content = response["content"]
            .as_array()
            .ok_or_else(|| Error::Provider("missing content array".into()))?
            .clone();

        self.messages
            .push(json!({ "role": "assistant", "content": content }));

        // Parse all tool uses, queue them, and execute the first.
        let mut tool_uses = Vec::new();
        let mut texts = Vec::new();
        for block in &content {
            match block["type"].as_str() {
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let input = &block["input"];
                    match parse_tool_input(input) {
                        Ok(action) => tool_uses.push((id, action)),
                        Err(e) => {
                            tracing::warn!(error = %e, "ignoring unparsable tool call");
                        }
                    }
                }
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        texts.push(t.to_string());
                    }
                }
                _ => {}
            }
        }

        // Completion markers win when the model stopped calling tools.
        if tool_uses.is_empty() {
            let joined = texts.join("\n");
            if let Some(rest) = joined.split("TASK_COMPLETE").nth(1) {
                let summary = rest.trim_start_matches([':', ' ']).trim().to_string();
                return Ok(Action::Done {
                    summary: if summary.is_empty() {
                        "task complete".into()
                    } else {
                        summary
                    },
                });
            }
            if let Some(rest) = joined.split("TASK_FAILED").nth(1) {
                let reason = rest.trim_start_matches([':', ' ']).trim().to_string();
                return Ok(Action::Fail {
                    reason: if reason.is_empty() {
                        "model reported failure".into()
                    } else {
                        reason
                    },
                });
            }
            tracing::warn!("model returned no tool call; nudging");
            return Ok(Action::Wait { ms: 500 });
        }

        let mut queued = tool_uses.into_iter();
        let (first_id, first_action) = queued.next().expect("non-empty");
        // Extra tool uses in the same turn are acknowledged in the next user
        // message so the conversation stays valid; they are not executed.
        self.skipped.extend(queued.map(|(id, _)| id));
        self.pending.push_back((first_id, first_action.clone()));
        Ok(first_action)
    }

    fn request_body(&self) -> Value {
        json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "system": system_prompt(self.display.0, self.display.1),
            "tools": [{
                "type": "computer_20250124",
                "name": "computer",
                "display_width_px": self.display.0,
                "display_height_px": self.display.1,
                "display_number": 1
            }],
            "messages": self.messages
        })
    }

    async fn send_with_retry(&mut self, body: &Value) -> Result<Value> {
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
        let mut last_err = None;
        for attempt in 0..3u32 {
            let result = self
                .http
                .post(&url)
                .header("x-api-key", &self.config.api_key)
                .header("anthropic-version", API_VERSION)
                .header("anthropic-beta", COMPUTER_BETA)
                .json(body)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        let value: Value = serde_json::from_str(&text).map_err(|e| {
                            Error::Provider(format!("invalid JSON response: {e}; body={text}"))
                        })?;
                        if value["type"] == "error" {
                            let msg = value["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown API error")
                                .to_string();
                            return Err(Error::Provider(msg));
                        }
                        return Ok(value);
                    }

                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    let msg = format!(
                        "HTTP {}: {}",
                        status,
                        text.chars().take(600).collect::<String>()
                    );
                    if !retryable || attempt == 2 {
                        return Err(Error::Provider(msg));
                    }
                    last_err = Some(msg);
                }
                Err(e) => {
                    if attempt == 2 {
                        return Err(Error::Provider(e.to_string()));
                    }
                    last_err = Some(e.to_string());
                }
            }
            let backoff = Duration::from_millis(800 * 2u64.pow(attempt));
            tracing::warn!(attempt, error = ?last_err, "retrying provider call");
            tokio::time::sleep(backoff).await;
        }
        Err(Error::Provider(
            last_err.unwrap_or_else(|| "provider call failed".into()),
        ))
    }
}

fn image_block(obs: &Observation) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": obs.media_type.clone(),
            "data": obs.base64()
        }
    })
}

fn system_prompt(width: u32, height: u32) -> String {
    format!(
        "You are a computer-use agent controlling a {width}x{height} desktop through mouse and keyboard.\n\
         Rules:\n\
         - Use the computer tool for every interaction. Coordinates are in the {width}x{height} screenshot space.\n\
         - Prefer keyboard shortcuts over clicking when reliable (e.g. ctrl+l to focus a browser address bar).\n\
         - After each action you receive a fresh screenshot. Verify the result before continuing.\n\
         - Keep actions minimal; do not repeat a failed action more than twice; try a different approach instead.\n\
         - Never run destructive commands (deleting files, formatting disks, shutting down) unless the task explicitly asks.\n\
         - When the task is complete, do not call tools; reply exactly `TASK_COMPLETE: <one-line summary>`.\n\
         - If the task is impossible, reply exactly `TASK_FAILED: <reason>`."
    )
}

fn parse_tool_input(input: &Value) -> Result<Action> {
    let action = input["action"]
        .as_str()
        .ok_or_else(|| Error::InvalidAction("tool input has no action".into()))?;

    let coord = |key: &str| -> Option<Point> {
        input[key]
            .as_array()
            .and_then(|a| match (a.first()?.as_i64(), a.get(1)?.as_i64()) {
                (Some(x), Some(y)) => Some(Point::new(x as i32, y as i32)),
                _ => None,
            })
    };

    Ok(match action {
        "screenshot" => Action::Wait { ms: 300 },
        "wait" => {
            let secs = input["duration"].as_f64().unwrap_or(1.0);
            Action::Wait {
                ms: (secs * 1000.0) as u64,
            }
        }
        "mouse_move" => Action::Move {
            to: coord("coordinate")
                .ok_or_else(|| Error::InvalidAction("mouse_move without coordinate".into()))?,
        },
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
            let button = match action {
                "right_click" => MouseButton::Right,
                "middle_click" => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            Action::Click {
                at: coord("coordinate")
                    .ok_or_else(|| Error::InvalidAction(format!("{action} without coordinate")))?,
                button,
                double: matches!(action, "double_click" | "triple_click"),
            }
        }
        "left_click_drag" => Action::Drag {
            from: coord("start_coordinate")
                .ok_or_else(|| Error::InvalidAction("drag without start_coordinate".into()))?,
            to: coord("coordinate")
                .ok_or_else(|| Error::InvalidAction("drag without coordinate".into()))?,
        },
        "type" => Action::Type {
            text: input["text"].as_str().unwrap_or_default().to_string(),
            interval_ms: 8,
        },
        "key" => Action::Key {
            combo: input["text"].as_str().unwrap_or_default().to_string(),
        },
        "hold_key" => Action::Key {
            combo: input["text"].as_str().unwrap_or_default().to_string(),
        },
        "scroll" => {
            let dir = input["scroll_direction"].as_str().unwrap_or("down");
            let amount = input["scroll_amount"].as_i64().unwrap_or(3) as i32;
            let (dx, dy) = match dir {
                "up" => (0, -amount),
                "down" => (0, amount),
                "left" => (-amount, 0),
                "right" => (amount, 0),
                _ => (0, amount),
            };
            let at = coord("coordinate").unwrap_or(Point::new(0, 0));
            Action::Scroll {
                at,
                dx: dx * 3,
                dy: dy * 3,
            }
        }
        other => {
            return Err(Error::InvalidAction(format!(
                "unsupported computer action: {other}"
            )))
        }
    })
}
