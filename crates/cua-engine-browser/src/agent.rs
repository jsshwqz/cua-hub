//! Model-driven browser loop: one JSON action per step.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cua_engine_vision::OpenAiConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::{self, LaunchOptions, PageSummary};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserAction {
    Navigate {
        url: String,
    },
    Click {
        selector: String,
    },
    Type {
        selector: String,
        text: String,
    },
    Press {
        key: String,
    },
    Eval {
        script: String,
    },
    Wait {
        ms: u64,
    },
    Scroll {
        #[serde(default)]
        dx: i32,
        #[serde(default = "default_scroll")]
        dy: i32,
    },
    WaitFor {
        selector: String,
        #[serde(default = "default_wait_ms")]
        timeout_ms: u64,
    },
    TabList,
    TabNew {
        url: String,
    },
    TabSwitch {
        index: usize,
    },
    TabClose {
        #[serde(default)]
        index: Option<usize>,
    },
    Done {
        summary: String,
    },
    Fail {
        reason: String,
    },
}

fn default_scroll() -> i32 {
    600
}

fn default_wait_ms() -> u64 {
    5000
}

impl BrowserAction {
    pub fn describe(&self) -> String {
        match self {
            BrowserAction::Navigate { url } => format!("navigate {url}"),
            BrowserAction::Click { selector } => format!("click {selector}"),
            BrowserAction::Type { selector, text } => {
                let preview: String = text.chars().take(40).collect();
                format!("type into {selector}: {preview:?}")
            }
            BrowserAction::Press { key } => format!("press {key}"),
            BrowserAction::Eval { script } => {
                let preview: String = script.chars().take(80).collect();
                format!("eval {preview}")
            }
            BrowserAction::Wait { ms } => format!("wait {ms}ms"),
            BrowserAction::Scroll { dx, dy } => format!("scroll dx={dx} dy={dy}"),
            BrowserAction::WaitFor {
                selector,
                timeout_ms,
            } => format!("wait_for {selector} (≤{timeout_ms}ms)"),
            BrowserAction::TabList => "tab_list".into(),
            BrowserAction::TabNew { url } => format!("tab_new {url}"),
            BrowserAction::TabSwitch { index } => format!("tab_switch {index}"),
            BrowserAction::TabClose { index } => match index {
                Some(i) => format!("tab_close {i}"),
                None => "tab_close current".into(),
            },
            BrowserAction::Done { summary } => format!("done: {summary}"),
            BrowserAction::Fail { reason } => format!("fail: {reason}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrowserRunOptions {
    pub task: String,
    pub max_steps: u32,
    pub dry_run: bool,
    pub model: Option<String>,
    pub headed: bool,
    pub start_url: Option<String>,
    pub run_dir: Option<PathBuf>,
    pub screenshot_quality: u8,
}

impl Default for BrowserRunOptions {
    fn default() -> Self {
        Self {
            task: String::new(),
            max_steps: 15,
            dry_run: false,
            model: None,
            headed: false,
            start_url: None,
            run_dir: None,
            screenshot_quality: 70,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum BrowserOutcome {
    Completed(String),
    Failed(String),
    StepLimit,
    /// Planned action, described as text (dry-run).
    DryRun(String),
}

impl BrowserOutcome {
    pub fn summary(&self, steps: usize) -> String {
        match self {
            BrowserOutcome::Completed(s) => format!("completed: {s}"),
            BrowserOutcome::Failed(s) => format!("failed: {s}"),
            BrowserOutcome::StepLimit => format!("stopped after {steps} steps"),
            BrowserOutcome::DryRun(s) => format!("dry-run planned: {s}"),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(
            self,
            BrowserOutcome::Completed(_) | BrowserOutcome::DryRun(_)
        )
    }
}

#[derive(Debug)]
pub struct BrowserReport {
    pub outcome: BrowserOutcome,
    pub run_dir: PathBuf,
    pub steps: usize,
}

pub async fn run_task(opts: BrowserRunOptions) -> Result<BrowserReport> {
    let config = OpenAiConfig::from_env()?.with_model(opts.model.clone());
    eprintln!("provider: browser model={}", config.model);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default();

    let mut session = cdp::launch(LaunchOptions {
        headed: opts.headed,
        start_url: opts.start_url.clone(),
        ..Default::default()
    })
    .await
    .context("launch browser")?;

    if let Some(url) = &opts.start_url {
        session.navigate(url).await?;
    }

    let run_dir = opts.run_dir.clone().unwrap_or_else(default_run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let mut log = std::fs::File::create(run_dir.join("steps.jsonl"))?;
    let mut history: Vec<String> = Vec::new();
    let mut outcome = BrowserOutcome::StepLimit;
    let mut executed_steps = 0usize;

    for index in 0..opts.max_steps {
        let summary = session.page_summary().await.unwrap_or(PageSummary {
            url: String::new(),
            title: String::new(),
            text: String::new(),
            ax: String::new(),
        });
        let shot = session.screenshot_jpeg(opts.screenshot_quality).await?;
        std::fs::write(run_dir.join(format!("browser-step-{index:02}.jpg")), &shot)?;

        let action = ask(&http, &config, &opts.task, &summary, &history, &shot)
            .await
            .with_context(|| format!("model failed at browser step {index}"))?;
        eprintln!("[{index:02}] {}", action.describe());

        match &action {
            BrowserAction::Done { summary } => {
                record(&mut log, index, &action, "completed")?;
                outcome = BrowserOutcome::Completed(summary.clone());
                break;
            }
            BrowserAction::Fail { reason } => {
                record(&mut log, index, &action, "failed")?;
                outcome = BrowserOutcome::Failed(reason.clone());
                break;
            }
            _ => {}
        }

        if opts.dry_run {
            record(&mut log, index, &action, "dry-run")?;
            outcome = BrowserOutcome::DryRun(action.describe());
            break;
        }

        let result = execute(&mut session, &action).await;
        let note = match result {
            Ok(note) => note,
            Err(e) => format!("error: {e}"),
        };
        eprintln!("      -> {note}");
        record(&mut log, index, &action, &note)?;
        history.push(format!("#{index} {} => {}", action.describe(), note));
        executed_steps += 1;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    let report = BrowserReport {
        outcome,
        run_dir: run_dir.clone(),
        steps: executed_steps,
    };
    std::fs::write(
        run_dir.join("outcome.json"),
        serde_json::to_vec_pretty(&json!({
            "task": opts.task,
            "outcome": report.outcome.summary(report.steps),
            "steps": report.steps,
        }))?,
    )?;
    eprintln!("\n{}", report.outcome.summary(report.steps));
    eprintln!("artifacts: {}", run_dir.display());
    Ok(report)
}

async fn execute(session: &mut cdp::BrowserSession, action: &BrowserAction) -> Result<String> {
    match action {
        BrowserAction::Navigate { url } => {
            session.navigate(url).await?;
            let s = session.page_summary().await?;
            Ok(format!("loaded {} ({})", s.url, s.title))
        }
        BrowserAction::Click { selector } => {
            let r = session.click(selector).await?;
            if r == "NOT_FOUND" {
                bail!("selector not found: {selector}");
            }
            Ok(r)
        }
        BrowserAction::Type { selector, text } => {
            let r = session.type_text(selector, text).await?;
            if r == "NOT_FOUND" {
                bail!("selector not found: {selector}");
            }
            Ok(r)
        }
        BrowserAction::Press { key } => {
            session.press_key(key).await?;
            Ok(format!("pressed {key}"))
        }
        BrowserAction::Eval { script } => {
            let value = session.eval(script).await?;
            let text = value.to_string();
            Ok(text.chars().take(2000).collect())
        }
        BrowserAction::Wait { ms } => {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
            Ok(format!("waited {ms}ms"))
        }
        BrowserAction::Scroll { dx, dy } => {
            session.scroll(*dx, *dy).await?;
            Ok(format!("scrolled dx={dx} dy={dy}"))
        }
        BrowserAction::WaitFor {
            selector,
            timeout_ms,
        } => {
            let found = session.wait_for_selector(selector, *timeout_ms).await?;
            Ok(if found {
                format!("found {selector}")
            } else {
                format!("timeout waiting for {selector}")
            })
        }
        BrowserAction::TabList => {
            let tabs = session.list_tabs().await?;
            let mut out = String::new();
            for (i, tab) in tabs.iter().enumerate() {
                out.push_str(&format!("{i}: {} — {}\n", tab.title, tab.url));
            }
            Ok(out.trim_end().to_string())
        }
        BrowserAction::TabNew { url } => {
            session.new_tab(url).await?;
            let s = session.page_summary().await?;
            Ok(format!("opened tab {} ({})", s.url, s.title))
        }
        BrowserAction::TabSwitch { index } => {
            session.switch_tab(*index).await?;
            let s = session.page_summary().await?;
            Ok(format!("tab {index}: {} ({})", s.url, s.title))
        }
        BrowserAction::TabClose { index } => {
            session.close_tab(*index).await?;
            Ok(format!(
                "closed tab; {} remaining",
                session.list_tabs().await?.len()
            ))
        }
        BrowserAction::Done { .. } | BrowserAction::Fail { .. } => Ok("terminal".into()),
    }
}

fn record(file: &mut std::fs::File, index: u32, action: &BrowserAction, note: &str) -> Result<()> {
    use std::io::Write;
    let line = json!({ "index": index, "action": action, "note": note });
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

async fn ask(
    http: &reqwest::Client,
    config: &OpenAiConfig,
    task: &str,
    page: &PageSummary,
    history: &[String],
    screenshot: &[u8],
) -> Result<BrowserAction> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(screenshot);

    let trail = if history.is_empty() {
        "(none)".to_string()
    } else {
        history
            .iter()
            .rev()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user_text = format!(
        "TASK: {task}\n\nURL: {}\nTITLE: {}\n\nACCESSIBILITY TREE (trimmed):\n{}\n\nPAGE TEXT (trimmed):\n{}\n\nRECENT STEPS:\n{}\n\nChoose one next action. JSON only.",
        page.url,
        page.title,
        page.ax.chars().take(2500).collect::<String>(),
        page.text.chars().take(2000).collect::<String>(),
        trail
    );

    let body = json!({
        "model": config.model,
        "max_tokens": 700,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system_prompt() },
            { "role": "user", "content": [
                { "type": "text", "text": user_text },
                { "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{b64}") } }
            ]}
        ]
    });

    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
    let mut last_err = String::new();
    for attempt in 0..3u32 {
        let result = http
            .post(&url)
            .bearer_auth(&config.api_key)
            .json(&body)
            .send()
            .await;
        match result {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;
                    let message = &value["choices"][0]["message"];
                    let content = message["content"].as_str().unwrap_or_default();
                    if content.trim().is_empty() {
                        if let Some(reasoning) = message["reasoning"].as_str() {
                            if reasoning.contains("\"action\"") {
                                return parse_browser_action(reasoning);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "empty model content: {}",
                            text.chars().take(300).collect::<String>()
                        ));
                    }
                    return parse_browser_action(content);
                }
                last_err = format!(
                    "HTTP {status}: {}",
                    text.chars().take(300).collect::<String>()
                );
                if !(status.is_server_error() || status.as_u16() == 429) || attempt == 2 {
                    bail!("{last_err}");
                }
            }
            Err(e) => {
                last_err = e.to_string();
                if attempt == 2 {
                    bail!("{last_err}");
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(700 * 2u64.pow(attempt))).await;
    }
    bail!("{last_err}")
}

fn system_prompt() -> &'static str {
    "You are a browser automation agent driving a Chromium page over CDP. \
Reply with ONE JSON object, no markdown, no prose.\n\
Actions:\n\
{\"action\":\"navigate\",\"url\":\"https://...\"}\n\
{\"action\":\"click\",\"selector\":\"css selector or text=Visible label\"}\n\
{\"action\":\"type\",\"selector\":\"css selector\",\"text\":\"...\"}\n\
{\"action\":\"press\",\"key\":\"Enter|Tab|Escape\"}\n\
{\"action\":\"eval\",\"script\":\"JS expression returning JSON-able data\"}\n\
{\"action\":\"wait\",\"ms\":1000}\n\
{\"action\":\"scroll\",\"dy\":600}                 positive dy scrolls down\n\
{\"action\":\"wait_for\",\"selector\":\"...\",\"timeout_ms\":5000}\n\
{\"action\":\"tab_list\"}\n\
{\"action\":\"tab_new\",\"url\":\"https://...\"}\n\
{\"action\":\"tab_switch\",\"index\":0}\n\
{\"action\":\"tab_close\",\"index\":0}            omit index to close current\n\
{\"action\":\"done\",\"summary\":\"one line result\"}\n\
{\"action\":\"fail\",\"reason\":\"...\"}\n\
Rules: prefer `text=` selectors when you cannot see exact CSS. Selectors also match inside same-origin iframes. \
Use the accessibility tree for roles/names; use eval for reading data. \
Do not submit forms, purchase, or log in unless the task explicitly asks. \
Never repeat a failing action more than twice. When the goal is reached, reply done."
}

/// Parse a browser action from a model reply, tolerating fences and prose.
pub fn parse_browser_action(content: &str) -> Result<BrowserAction> {
    let trimmed = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```");
    let value =
        extract_object(trimmed).ok_or_else(|| anyhow::anyhow!("no JSON action in model reply"))?;
    let action = value["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing action field"))?
        .to_ascii_lowercase();

    let s = |key: &str| -> Result<String> {
        value[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("missing string field `{key}`"))
    };

    Ok(match action.as_str() {
        "navigate" | "open" | "goto" => BrowserAction::Navigate { url: s("url")? },
        "click" => BrowserAction::Click {
            selector: s("selector")?,
        },
        "type" | "fill" => BrowserAction::Type {
            selector: s("selector")?,
            text: s("text")?,
        },
        "press" => BrowserAction::Press { key: s("key")? },
        "eval" | "extract" => BrowserAction::Eval {
            script: s("script")?,
        },
        "wait" => BrowserAction::Wait {
            ms: value["ms"].as_u64().unwrap_or(800),
        },
        "scroll" => BrowserAction::Scroll {
            dx: value["dx"].as_i64().unwrap_or(0) as i32,
            dy: value["dy"].as_i64().unwrap_or(default_scroll() as i64) as i32,
        },
        "wait_for" => BrowserAction::WaitFor {
            selector: s("selector")?,
            timeout_ms: value["timeout_ms"].as_u64().unwrap_or(default_wait_ms()),
        },
        "tab_list" => BrowserAction::TabList,
        "tab_new" => BrowserAction::TabNew { url: s("url")? },
        "tab_switch" => BrowserAction::TabSwitch {
            index: value["index"].as_u64().unwrap_or(0) as usize,
        },
        "tab_close" => BrowserAction::TabClose {
            index: value["index"].as_u64().map(|i| i as usize),
        },
        "done" => BrowserAction::Done {
            summary: value["summary"].as_str().unwrap_or("task complete").into(),
        },
        "fail" => BrowserAction::Fail {
            reason: value["reason"]
                .as_str()
                .unwrap_or("model reported failure")
                .into(),
        },
        other => bail!("unsupported browser action: {other}"),
    })
}

fn extract_object(s: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(s.trim()) {
        if v.get("action").is_some() {
            return Some(v);
        }
    }
    let bytes = s.as_bytes();
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
                        if let Ok(v) = serde_json::from_str::<Value>(&s[i..=j]) {
                            if v.get("action").is_some() {
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

fn default_run_dir() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(".runs").join(format!("browser-{stamp}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_navigate() {
        let a =
            parse_browser_action(r#"{"action":"navigate","url":"https://example.com"}"#).unwrap();
        assert!(matches!(a, BrowserAction::Navigate { url } if url == "https://example.com"));
    }

    #[test]
    fn parses_fenced_click() {
        let a =
            parse_browser_action("```json\n{\"action\":\"click\",\"selector\":\"text=More\"}\n```")
                .unwrap();
        assert!(matches!(a, BrowserAction::Click { selector } if selector == "text=More"));
    }

    #[test]
    fn parses_done_from_prose() {
        let a = parse_browser_action("Result: {\"action\":\"done\",\"summary\":\"ok\"}").unwrap();
        assert!(matches!(a, BrowserAction::Done { summary } if summary == "ok"));
    }

    #[test]
    fn parses_scroll_and_wait_for() {
        let a = parse_browser_action(r#"{"action":"scroll","dy":800}"#).unwrap();
        assert!(matches!(a, BrowserAction::Scroll { dy: 800, .. }));
        let b =
            parse_browser_action(r#"{"action":"wait_for","selector":"main","timeout_ms":3000}"#)
                .unwrap();
        assert!(matches!(
            b,
            BrowserAction::WaitFor {
                timeout_ms: 3000,
                ..
            }
        ));
    }

    #[test]
    fn parses_tab_actions() {
        let a =
            parse_browser_action(r#"{"action":"tab_new","url":"https://example.com"}"#).unwrap();
        assert!(matches!(a, BrowserAction::TabNew { .. }));
        let b = parse_browser_action(r#"{"action":"tab_close"}"#).unwrap();
        assert!(matches!(b, BrowserAction::TabClose { index: None }));
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(parse_browser_action(r#"{"action":"teleport"}"#).is_err());
    }
}
