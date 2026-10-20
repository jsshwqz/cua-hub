//! Cross-engine orchestration: plan a task into ordered browser/code/vision
//! steps, pass outputs forward with `{{name}}`, and report per-step results.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use cua_core::EngineKind;
use cua_engine_vision::OpenAiConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::runner::{self, RunOptions};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub engine: EngineKind,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_as: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepExecResult {
    pub engine: EngineKind,
    pub goal: String,
    pub summary: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct OrchestrateOptions {
    pub task: String,
    pub dry_run: bool,
    pub max_plan_steps: usize,
    pub max_steps_per_engine: u32,
    pub model: Option<String>,
    pub headed: bool,
    pub yes: bool,
    pub allow_high: bool,
    pub run_dir: Option<PathBuf>,
}

impl Default for OrchestrateOptions {
    fn default() -> Self {
        Self {
            task: String::new(),
            dry_run: false,
            max_plan_steps: 6,
            max_steps_per_engine: 10,
            model: None,
            headed: false,
            yes: true,
            allow_high: false,
            run_dir: None,
        }
    }
}

#[derive(Debug)]
pub struct OrchestrateReport {
    pub plan: Vec<PlanStep>,
    pub results: Vec<StepExecResult>,
    pub success: bool,
    pub run_dir: PathBuf,
    pub dry_run: bool,
}

impl OrchestrateReport {
    pub fn summary(&self) -> String {
        if self.dry_run {
            let lines: Vec<String> = self
                .plan
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    format!(
                        "{}. [{}] {}{}",
                        i + 1,
                        s.engine.as_str(),
                        s.goal,
                        s.save_as
                            .as_ref()
                            .map(|v| format!(" (save as {v})"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            return format!("dry-run plan:\n{}", lines.join("\n"));
        }
        let mut out = String::new();
        for (i, r) in self.results.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {} -> {}\n",
                i + 1,
                r.engine.as_str(),
                r.goal,
                r.summary
            ));
        }
        if let Some(last) = self.results.last() {
            out.push_str(&format!("final: {}", last.summary));
        }
        out
    }
}

pub async fn run(opts: OrchestrateOptions) -> Result<OrchestrateReport> {
    let config = OpenAiConfig::from_env()?.with_model(opts.model.clone());
    eprintln!("provider: orchestrator model={}", config.model);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default();

    let plan = plan(&http, &config, &opts.task, opts.max_plan_steps).await?;
    let run_dir = opts.run_dir.clone().unwrap_or_else(default_run_dir);
    std::fs::create_dir_all(&run_dir)?;
    std::fs::write(run_dir.join("plan.json"), serde_json::to_vec_pretty(&plan)?)?;

    eprintln!("plan ({} steps):", plan.len());
    for (i, s) in plan.iter().enumerate() {
        eprintln!("  {}. [{}] {}", i + 1, s.engine.as_str(), s.goal);
    }

    if opts.dry_run {
        return Ok(OrchestrateReport {
            plan,
            results: Vec::new(),
            success: true,
            run_dir,
            dry_run: true,
        });
    }

    let mut vars: HashMap<String, String> = HashMap::new();
    let mut results = Vec::new();
    let mut success = true;

    for (index, step) in plan.iter().enumerate() {
        let goal = apply_vars(&step.goal, &vars);
        eprintln!(
            "\n=== step {}/{} [{}] {}",
            index + 1,
            plan.len(),
            step.engine.as_str(),
            goal
        );
        let step_dir = run_dir.join(format!("step-{}-{}", index + 1, step.engine.as_str()));

        let (summary, ok, artifacts) = match step.engine {
            EngineKind::Browser => {
                let report = cua_engine_browser::run_task(cua_engine_browser::BrowserRunOptions {
                    task: goal.clone(),
                    max_steps: opts.max_steps_per_engine,
                    dry_run: false,
                    model: opts.model.clone(),
                    headed: opts.headed,
                    start_url: None,
                    run_dir: Some(step_dir.clone()),
                    screenshot_quality: 70,
                })
                .await?;
                let ok = report.outcome.is_success();
                (
                    report.outcome.summary(report.steps),
                    ok,
                    Some(report.run_dir),
                )
            }
            EngineKind::Vision | EngineKind::Code | EngineKind::Access => {
                let kind = if step.engine == EngineKind::Access {
                    EngineKind::Vision
                } else {
                    step.engine
                };
                let report = runner::run_task(RunOptions {
                    task: goal.clone(),
                    max_steps: opts.max_steps_per_engine,
                    yes: opts.yes,
                    allow_high: opts.allow_high,
                    dry_run: false,
                    engine: Some(kind),
                    provider: runner::ProviderArg::Auto,
                    model: opts.model.clone(),
                    max_edge: 1568,
                    run_dir: Some(step_dir.clone()),
                    settle_ms: 600,
                    ui_tree: false,
                    browser_headed: opts.headed,
                })
                .await?;
                let ok = report.is_success();
                (report.summary(), ok, Some(report.run_dir.clone()))
            }
        };

        let summary = clean_summary(&summary);
        if let Some(name) = &step.save_as {
            vars.insert(name.clone(), summary.clone());
        }
        results.push(StepExecResult {
            engine: step.engine,
            goal,
            summary: summary.clone(),
            success: ok,
            artifacts,
        });

        if !ok {
            success = false;
            eprintln!("step failed, stopping");
            break;
        }
    }

    let report = OrchestrateReport {
        success,
        plan,
        results,
        run_dir: run_dir.clone(),
        dry_run: false,
    };
    std::fs::write(
        run_dir.join("results.json"),
        serde_json::to_vec_pretty(&json!({
            "success": report.success,
            "steps": report.results,
        }))?,
    )?;
    eprintln!("\nartifacts: {}", run_dir.display());
    Ok(report)
}

/// Replace `{{name}}` placeholders with step outputs.
pub fn apply_vars(goal: &str, vars: &HashMap<String, String>) -> String {
    let mut out = goal.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

/// Strip outcome prefixes so saved variables hold plain content.
fn clean_summary(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("completed: ")
        .or_else(|| s.strip_prefix("dry-run planned: "))
        .unwrap_or(s);
    s.trim().to_string()
}

async fn plan(
    http: &reqwest::Client,
    config: &OpenAiConfig,
    task: &str,
    max_plan_steps: usize,
) -> Result<Vec<PlanStep>> {
    let body = json!({
        "model": config.model,
        "max_tokens": 900,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": planner_prompt(max_plan_steps) },
            { "role": "user", "content": format!("TASK: {task}\nOS: {}", std::env::consts::OS) }
        ]
    });
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));

    let mut last_err = String::new();
    for attempt in 0..3u32 {
        match http
            .post(&url)
            .bearer_auth(&config.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let value: Value = serde_json::from_str(&text)?;
                    let message = &value["choices"][0]["message"];
                    let content = message["content"].as_str().unwrap_or_default();
                    if content.trim().is_empty() {
                        if let Some(reasoning) = message["reasoning"].as_str() {
                            if reasoning.contains("\"steps\"") {
                                return parse_plan(reasoning, max_plan_steps);
                            }
                        }
                        bail!("planner returned empty content");
                    }
                    return parse_plan(content, max_plan_steps);
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

fn planner_prompt(max_steps: usize) -> String {
    format!(
        "You decompose a desktop automation goal into at most {max_steps} ordered steps.\n\
Engines:\n\
- browser: web pages and URLs (headless Chromium: navigate/click/type/eval)\n\
- code: shell commands on this OS (PowerShell on Windows); deterministic data, files, system info\n\
- vision: GUI applications via screenshots, mouse and keyboard\n\
Rules:\n\
- Use browser when the goal involves a URL or web content; code for data/files; vision for GUI apps.\n\
- Each goal must be self-contained, imperative and specific.\n\
- Later goals may embed earlier results as {{{{name}}}} after setting save_as on the earlier step.\n\
- The last step must produce the final answer.\n\
- Never include destructive commands (delete, format, shutdown).\n\
Reply with JSON only: {{\"steps\":[{{\"engine\":\"browser\",\"goal\":\"...\",\"save_as\":\"name\"}}]}}\n\
save_as is optional; omit it when the output is not needed later."
    )
}

/// Parse and validate a planner reply.
pub fn parse_plan(content: &str, max_steps: usize) -> Result<Vec<PlanStep>> {
    let value = extract_object(content).ok_or_else(|| anyhow::anyhow!("no JSON plan in reply"))?;
    let steps = value["steps"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("plan has no steps array"))?;
    if steps.is_empty() {
        bail!("plan is empty");
    }

    let mut out = Vec::new();
    for (i, s) in steps.iter().enumerate().take(max_steps) {
        let engine = match s["engine"].as_str().unwrap_or_default() {
            "browser" => EngineKind::Browser,
            "code" => EngineKind::Code,
            "vision" => EngineKind::Vision,
            "access" => EngineKind::Access,
            other => bail!("step {i} has unknown engine: {other}"),
        };
        let goal = s["goal"]
            .as_str()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .ok_or_else(|| anyhow::anyhow!("step {i} has no goal"))?;
        let save_as = s["save_as"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        out.push(PlanStep {
            engine,
            goal: goal.to_string(),
            save_as,
        });
    }
    if out.is_empty() {
        bail!("plan is empty after validation");
    }
    Ok(out)
}

fn extract_object(s: &str) -> Option<Value> {
    let trimmed = s
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```");
    if let Ok(v) = serde_json::from_str::<Value>(trimmed.trim()) {
        if v.get("steps").is_some() {
            return Some(v);
        }
    }
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
                            if v.get("steps").is_some() {
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
    PathBuf::from(".runs").join(format!("orchestrate-{stamp}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_with_engines() {
        let plan = parse_plan(
            r#"{"steps":[
                {"engine":"browser","goal":"open example.com and read the title","save_as":"title"},
                {"engine":"code","goal":"print the title: {{title}}"}
            ]}"#,
            6,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].engine, EngineKind::Browser);
        assert_eq!(plan[0].save_as.as_deref(), Some("title"));
        assert_eq!(plan[1].engine, EngineKind::Code);
    }

    #[test]
    fn caps_plan_length() {
        let plan = parse_plan(
            r#"{"steps":[
                {"engine":"code","goal":"a"},
                {"engine":"code","goal":"b"},
                {"engine":"code","goal":"c"}
            ]}"#,
            2,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn substitutes_variables() {
        let mut vars = HashMap::new();
        vars.insert("title".to_string(), "Example Domain".to_string());
        assert_eq!(
            apply_vars("print: {{title}}!", &vars),
            "print: Example Domain!"
        );
    }

    #[test]
    fn rejects_unknown_engine() {
        assert!(parse_plan(r#"{"steps":[{"engine":"telepathy","goal":"x"}]}"#, 6).is_err());
    }

    #[test]
    fn clean_summary_strips_prefix() {
        assert_eq!(clean_summary("completed: 42 GB free"), "42 GB free");
    }
}
