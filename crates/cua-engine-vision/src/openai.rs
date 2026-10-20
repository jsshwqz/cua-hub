//! OpenAI-compatible vision provider.
//!
//! Works with OpenRouter, OpenAI, Ollama's `/v1`, vLLM, and any gateway that
//! speaks the chat-completions schema with image URLs. The model is asked for a
//! single JSON action per step; parsing is tolerant of markdown fences.

use std::time::Duration;

use async_trait::async_trait;
use cua_core::{
    Action, Engine, EngineKind, Error, MouseButton, Observation, Point, Result, StepRecord,
};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

impl OpenAiConfig {
    /// Resolve from the environment:
    ///
    /// * key: `OPENAI_API_KEY` or `OPENROUTER_API_KEY`
    /// * base: `CUA_HUB_OPENAI_BASE_URL`, `OPENROUTER_BASE_URL`, then OpenAI
    /// * model: `CUA_HUB_MODEL`, `OPENROUTER_MODEL`, then `gpt-4o-mini`
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .map_err(|_| {
                Error::Config("neither OPENAI_API_KEY nor OPENROUTER_API_KEY is set".into())
            })?;
        let base_url = std::env::var("CUA_HUB_OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("CUA_HUB_MODEL")
            .or_else(|_| std::env::var("OPENROUTER_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());
        Ok(Self {
            api_key,
            base_url,
            model,
            max_tokens: 1024,
        })
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        if let Some(m) = model {
            self.model = m;
        }
        self
    }
}

pub struct OpenAiVisionEngine {
    http: reqwest::Client,
    config: OpenAiConfig,
}

impl OpenAiVisionEngine {
    pub fn new(config: OpenAiConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self { http, config }
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    async fn ask(&self, body: &Value) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut last_err = String::new();
        for attempt in 0..3u32 {
            let result = self
                .http
                .post(&url)
                .bearer_auth(&self.config.api_key)
                .json(body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        let value: Value = serde_json::from_str(&text)
                            .map_err(|e| Error::Provider(format!("invalid JSON: {e}")))?;
                        let message = &value["choices"][0]["message"];
                        if let Some(s) = message["content"].as_str() {
                            if !s.trim().is_empty() {
                                return Ok(s.to_string());
                            }
                        }
                        // Some gateways return content as an array of parts, or
                        // put the answer in `reasoning`.
                        if let Some(parts) = message["content"].as_array() {
                            let joined = parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !joined.trim().is_empty() {
                                return Ok(joined);
                            }
                        }
                        // Reasoning models may spend the whole budget thinking
                        // and leave `content` empty; the action is often still
                        // embedded in `reasoning`.
                        if let Some(reasoning) = message["reasoning"].as_str() {
                            if reasoning.contains("\"action\"") {
                                tracing::warn!("model returned reasoning only; extracting action");
                                return Ok(reasoning.to_string());
                            }
                        }
                        let snapshot: String = value.to_string().chars().take(800).collect();
                        return Err(Error::Provider(format!(
                            "response has no content: {snapshot}"
                        )));
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    last_err = format!(
                        "HTTP {status}: {}",
                        text.chars().take(400).collect::<String>()
                    );
                    if !retryable || attempt == 2 {
                        return Err(Error::Provider(last_err));
                    }
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt == 2 {
                        return Err(Error::Provider(last_err));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(800 * 2u64.pow(attempt))).await;
        }
        Err(Error::Provider(last_err))
    }
}

#[async_trait]
impl Engine for OpenAiVisionEngine {
    fn name(&self) -> &'static str {
        "openai-compatible-vision"
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Vision
    }

    async fn next_action(
        &mut self,
        task: &str,
        observation: &Observation,
        history: &[StepRecord],
    ) -> Result<Action> {
        let mut trail = String::new();
        for step in history
            .iter()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            trail.push_str(&format!(
                "#{} {} {}\n",
                step.index,
                describe(&step.action),
                step.note.as_deref().unwrap_or("")
            ));
        }

        let tree = observation
            .ui_tree
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| format!("\nAccessibility tree (partial, indentation = depth):\n{t}"))
            .unwrap_or_default();
        let user_text = format!(
            "TASK: {task}\n\nScreen: {}x{} pixels.\nRecent steps:\n{}{}\n\
             Decide the single next action. Reply with JSON only.",
            observation.width,
            observation.height,
            if trail.is_empty() { "(none)\n" } else { &trail },
            tree
        );

        let body = json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system_prompt() },
                { "role": "user", "content": [
                    { "type": "text", "text": user_text },
                    { "type": "image_url", "image_url": { "url": format!("data:{};base64,{}", observation.media_type, observation.base64()) } }
                ]}
            ]
        });

        let content = self.ask(&body).await?;
        parse_action(&content)
    }
}

fn system_prompt() -> &'static str {
    "You are a computer-use agent. You see a screenshot of a desktop and choose ONE next action.\n\
Reply with a single JSON object, no markdown, no prose. Schema:\n\
{\"action\":\"click\",\"x\":int,\"y\":int}            left click at pixel\n\
{\"action\":\"double_click\",\"x\":int,\"y\":int}\n\
{\"action\":\"right_click\",\"x\":int,\"y\":int}\n\
{\"action\":\"move\",\"x\":int,\"y\":int}\n\
{\"action\":\"drag\",\"x\":int,\"y\":int,\"x2\":int,\"y2\":int}\n\
{\"action\":\"type\",\"text\":\"...\"}                type text into the focused field\n\
{\"action\":\"key\",\"text\":\"ctrl+c\"}               key chord: Return, Tab, Escape, ctrl+l, alt+Tab, ...\n\
{\"action\":\"scroll\",\"x\":int,\"y\":int,\"dy\":int}  positive dy scrolls down\n\
{\"action\":\"wait\",\"ms\":int}\n\
{\"action\":\"done\",\"summary\":\"...\"}              task finished\n\
{\"action\":\"fail\",\"reason\":\"...\"}               impossible or blocked\n\
Rules: coordinates are in screenshot pixels (origin top-left). Prefer keyboard shortcuts. \
Do not repeat a failed action more than twice; try another approach. \
Never run destructive commands unless explicitly requested. \
Click a text field before typing into it."
}

/// Parse a model reply into an action. Tolerates fenced blocks and stray prose.
pub fn parse_action(content: &str) -> Result<Action> {
    let trimmed = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: Value = match strict_parse(trimmed) {
        Some(v) => v,
        None => extract_embedded_action(trimmed).ok_or_else(|| {
            Error::Provider(format!(
                "model did not return a JSON action: {}",
                content.chars().take(300).collect::<String>()
            ))
        })?,
    };

    let action = value["action"]
        .as_str()
        .ok_or_else(|| Error::InvalidAction("missing action field".into()))?
        .to_ascii_lowercase();

    let coord = |x: &str, y: &str| -> Result<Point> {
        Ok(Point::new(
            value[x]
                .as_i64()
                .ok_or_else(|| Error::InvalidAction(format!("missing {x}")))? as i32,
            value[y]
                .as_i64()
                .ok_or_else(|| Error::InvalidAction(format!("missing {y}")))? as i32,
        ))
    };

    Ok(match action.as_str() {
        "click" | "left_click" => Action::Click {
            at: coord("x", "y")?,
            button: MouseButton::Left,
            double: false,
        },
        "double_click" => Action::Click {
            at: coord("x", "y")?,
            button: MouseButton::Left,
            double: true,
        },
        "right_click" => Action::Click {
            at: coord("x", "y")?,
            button: MouseButton::Right,
            double: false,
        },
        "middle_click" => Action::Click {
            at: coord("x", "y")?,
            button: MouseButton::Middle,
            double: false,
        },
        "move" => Action::Move {
            to: coord("x", "y")?,
        },
        "drag" => Action::Drag {
            from: coord("x", "y")?,
            to: coord("x2", "y2")?,
        },
        "type" => Action::Type {
            text: value["text"].as_str().unwrap_or_default().to_string(),
            interval_ms: 6,
        },
        "key" => Action::Key {
            combo: value["text"].as_str().unwrap_or_default().to_string(),
        },
        "scroll" => Action::Scroll {
            at: Point::new(
                value["x"].as_i64().unwrap_or(0) as i32,
                value["y"].as_i64().unwrap_or(0) as i32,
            ),
            dx: 0,
            dy: value["dy"].as_i64().unwrap_or(3) as i32 * 3,
        },
        "wait" => Action::Wait {
            ms: value["ms"].as_u64().unwrap_or(500),
        },
        "done" => Action::Done {
            summary: value["summary"].as_str().unwrap_or("task complete").into(),
        },
        "fail" => Action::Fail {
            reason: value["reason"]
                .as_str()
                .unwrap_or("model reported failure")
                .into(),
        },
        other => {
            return Err(Error::InvalidAction(format!(
                "unsupported action from model: {other}"
            )))
        }
    })
}

fn strict_parse(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    let value: Value = serde_json::from_str(s.get(start..=end)?).ok()?;
    value.get("action")?;
    Some(value)
}

/// Scan for balanced `{...}` objects and return the first one with an `action`
/// field. Used when a reasoning model wraps the action in prose.
fn extract_embedded_action(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escaped = false;
            for (j, &b) in bytes.iter().enumerate().skip(i) {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        in_string = false;
                    }
                    continue;
                }
                match b {
                    b'"' => in_string = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            if let Ok(v) = serde_json::from_str::<Value>(&s[i..=j]) {
                                if v.get("action").is_some() {
                                    return Some(v);
                                }
                            }
                            i = j;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    None
}

fn describe(action: &Action) -> String {
    match action {
        Action::Click { at, double, .. } => format!(
            "{}click ({},{})",
            if *double { "double " } else { "" },
            at.x,
            at.y
        ),
        Action::Move { to } => format!("move ({},{})", to.x, to.y),
        Action::Drag { from, to } => format!("drag ({},{})->({},{})", from.x, from.y, to.x, to.y),
        Action::Type { text, .. } => {
            format!("type {:?}", text.chars().take(40).collect::<String>())
        }
        Action::Key { combo } => format!("key {combo}"),
        Action::Scroll { dy, .. } => format!("scroll dy={dy}"),
        Action::Wait { ms } => format!("wait {ms}ms"),
        Action::RunCommand { command, .. } => format!("run {command}"),
        Action::Screenshot => "screenshot".into(),
        Action::Done { summary } => format!("done {summary}"),
        Action::Fail { reason } => format!("fail {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let a = parse_action(r#"{"action":"click","x":10,"y":20}"#).unwrap();
        assert!(matches!(a, Action::Click { at, .. } if at == Point::new(10, 20)));
    }

    #[test]
    fn parses_fenced_json() {
        let a = parse_action("```json\n{\"action\":\"key\",\"text\":\"ctrl+l\"}\n```").unwrap();
        assert!(matches!(a, Action::Key { combo } if combo == "ctrl+l"));
    }

    #[test]
    fn parses_done() {
        let a = parse_action(r#"{"action":"done","summary":"ok"}"#).unwrap();
        assert!(matches!(a, Action::Done { summary } if summary == "ok"));
    }

    #[test]
    fn rejects_prose() {
        assert!(parse_action("I cannot do that").is_err());
    }
}
