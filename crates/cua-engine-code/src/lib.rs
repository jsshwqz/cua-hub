//! Deterministic-work engine: the model plans one shell command per step.
//!
//! Far cheaper and more reliable than pixel-level clicking for anything that is
//! expressible as a command (disk usage, file moves, service status, registry
//! reads). The runner executes [`Action::RunCommand`] and feeds the output back
//! through `StepRecord::note`.

use std::time::Duration;

use async_trait::async_trait;
use cua_core::{Action, Engine, EngineKind, Error, Observation, Result, ShellKind, StepRecord};
use cua_engine_vision::OpenAiConfig;
use serde_json::{json, Value};

pub struct CodeEngine {
    http: reqwest::Client,
    config: OpenAiConfig,
}

impl CodeEngine {
    pub fn new(config: OpenAiConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_default();
        Self { http, config }
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    async fn complete(&self, messages: Value) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = json!({
            "model": self.config.model,
            "max_tokens": 1200,
            "temperature": 0,
            "messages": messages,
        });

        let mut last_err = String::new();
        for attempt in 0..3u32 {
            let result = self
                .http
                .post(&url)
                .bearer_auth(&self.config.api_key)
                .json(&body)
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
                        // Reasoning models may spend the whole budget thinking
                        // and leave `content` null; the JSON action is often
                        // embedded in `reasoning`. Let the parser decide.
                        if let Some(reasoning) = message["reasoning"].as_str() {
                            if !reasoning.trim().is_empty() {
                                tracing::warn!("code model returned reasoning only; extracting");
                                return Ok(reasoning.to_string());
                            }
                        }
                        return Err(Error::Provider(format!(
                            "empty content: {}",
                            text.chars().take(400).collect::<String>()
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
            tokio::time::sleep(Duration::from_millis(700 * 2u64.pow(attempt))).await;
        }
        Err(Error::Provider(last_err))
    }
}

#[async_trait]
impl Engine for CodeEngine {
    fn name(&self) -> &'static str {
        "llm-code"
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Code
    }

    async fn next_action(
        &mut self,
        task: &str,
        _observation: &Observation,
        history: &[StepRecord],
    ) -> Result<Action> {
        let mut trail = String::new();
        for step in history
            .iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            trail.push_str(&format!(
                "#{} {}\n{}\n",
                step.index,
                summary(&step.action),
                step.note.as_deref().unwrap_or("(no output)")
            ));
        }

        let shell_name = if cfg!(windows) {
            "PowerShell"
        } else {
            "/bin/sh"
        };
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let user = format!(
            "TASK: {task}\n\nOS: {}\nShell: {shell_name}\nWorking dir: {cwd}\n\
             Recent steps:\n{}\nReply with JSON only.",
            std::env::consts::OS,
            if trail.is_empty() { "(none)\n" } else { &trail }
        );

        let messages = json!([
            { "role": "system", "content": system_prompt(shell_name) },
            { "role": "user", "content": user }
        ]);
        let content = self.complete(messages).await?;
        parse_command(&content)
    }
}

fn system_prompt(shell: &str) -> String {
    format!(
        "You are a shell automation agent on {} using {}. Choose exactly ONE action per turn.\n\
         Reply with a single JSON object, no markdown, no prose:\n\
         {{\"command\":\"<one shell command>\"}}   run it and observe the output\n\
         {{\"done\":true,\"summary\":\"<one line>\"}} task finished\n\
         {{\"fail\":\"<reason>\"}}                  impossible or blocked\n\
         Rules:\n\
         - One command per turn; inspect output before the next step.\n\
         - Prefer read-only commands. Never delete files, format disks, change the registry, or shut down unless the task explicitly asks.\n\
         - Use non-interactive flags. Do not start GUI programs; use a command that exits.\n\
         - If a command failed, do not repeat it; adapt.\n\
         - When the goal is reached, reply with done.",
        std::env::consts::OS,
        shell
    )
}

fn summary(action: &Action) -> String {
    match action {
        Action::RunCommand { command, .. } => format!("run `{command}`"),
        other => format!("{other:?}"),
    }
}

/// Parse `{"command":..}` / `{"done":..}` / `{"fail":..}` from a model reply.
pub fn parse_command(content: &str) -> Result<Action> {
    let value = extract_object(content).ok_or(Error::NoActionableContent)?;

    if value.get("command").is_some() {
        let command = value["command"]
            .as_str()
            .ok_or_else(|| Error::InvalidAction("command is not a string".into()))?
            .trim()
            .to_string();
        if command.is_empty() {
            return Err(Error::InvalidAction("empty command".into()));
        }
        return Ok(Action::RunCommand {
            command,
            shell: ShellKind::Auto,
        });
    }
    if value.get("done").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(Action::Done {
            summary: value["summary"]
                .as_str()
                .unwrap_or("task complete")
                .to_string(),
        });
    }
    if let Some(reason) = value.get("fail").and_then(Value::as_str) {
        return Ok(Action::Fail {
            reason: reason.to_string(),
        });
    }
    Err(Error::InvalidAction(
        "reply must contain command, done, or fail".into(),
    ))
}

fn extract_object(s: &str) -> Option<Value> {
    let trimmed = s
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```");
    if let Ok(v) = serde_json::from_str::<Value>(trimmed.trim()) {
        if v.is_object() {
            return Some(v);
        }
    }
    // Balanced-brace scan for JSON embedded in prose/reasoning.
    let bytes = trimmed.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'{' {
            continue;
        }
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
                        if let Ok(v) = serde_json::from_str::<Value>(&trimmed[i..=j]) {
                            if v.get("command").is_some()
                                || v.get("done").is_some()
                                || v.get("fail").is_some()
                            {
                                return Some(v);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command() {
        let a = parse_command(r#"{"command":"Get-PSDrive C"}"#).unwrap();
        assert!(matches!(a, Action::RunCommand { command, .. } if command == "Get-PSDrive C"));
    }

    #[test]
    fn parses_fenced_done() {
        let a = parse_command("```json\n{\"done\":true,\"summary\":\"ok\"}\n```").unwrap();
        assert!(matches!(a, Action::Done { summary } if summary == "ok"));
    }

    #[test]
    fn parses_fail() {
        let a = parse_command(r#"{"fail":"no permission"}"#).unwrap();
        assert!(matches!(a, Action::Fail { reason } if reason == "no permission"));
    }

    #[test]
    fn extracts_from_prose() {
        let a = parse_command("I will run {\"command\":\"echo hi\"} now").unwrap();
        assert!(matches!(a, Action::RunCommand { command, .. } if command == "echo hi"));
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_command("nothing here").is_err());
    }
}
