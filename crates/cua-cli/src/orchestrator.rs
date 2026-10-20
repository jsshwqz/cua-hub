//! Cross-engine orchestration: plan a task into ordered browser/code/vision
//! steps, pass outputs forward with `{{name}}`, and recover from failures with
//! a bounded retry and re-plan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct OrchestrateOptions {
    pub task: String,
    pub dry_run: bool,
    pub max_plan_steps: usize,
    pub max_steps_per_engine: u32,
    /// Extra attempts per step with the failure fed back.
    pub retries_per_step: u32,
    /// How many times a failed step may trigger a new plan.
    pub max_replans: usize,
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
            retries_per_step: 1,
            max_replans: 1,
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
    pub replans_used: usize,
}

impl OrchestrateReport {
    pub fn summary(&self) -> String {
        if self.dry_run {
            return format!("dry-run plan:\n{}", render_plan(&self.plan));
        }
        let mut out = String::new();
        for (i, r) in self.results.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {} -> {}{}\n",
                i + 1,
                r.engine.as_str(),
                r.goal,
                r.summary,
                if r.attempts > 1 {
                    format!(" ({} attempts)", r.attempts)
                } else {
                    String::new()
                }
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

    let mut plan = plan(&http, &config, &opts.task, opts.max_plan_steps).await?;
    let run_dir = opts.run_dir.clone().unwrap_or_else(default_run_dir);
    std::fs::create_dir_all(&run_dir)?;
    std::fs::write(run_dir.join("plan.json"), serde_json::to_vec_pretty(&plan)?)?;

    eprintln!("plan ({} steps):", plan.len());
    eprintln!("{}", render_plan(&plan));

    if opts.dry_run {
        return Ok(OrchestrateReport {
            plan,
            results: Vec::new(),
            success: true,
            run_dir,
            dry_run: true,
            replans_used: 0,
        });
    }

    let mut vars: HashMap<String, String> = HashMap::new();
    let mut results: Vec<StepExecResult> = Vec::new();
    let mut replans_used = 0usize;
    let mut terminal_failure = false;
    let mut step_counter = 0usize;
    let mut index = 0usize;
    let mut iterations = 0usize;

    while index < plan.len() {
        iterations += 1;
        if iterations > 40 {
            terminal_failure = true;
            eprintln!("orchestrator iteration cap reached");
            break;
        }

        let step = plan[index].clone();
        let goal = apply_vars(&step.goal, &vars);
        eprintln!(
            "\n=== step {}/{} [{}] {}",
            index + 1,
            plan.len(),
            step.engine.as_str(),
            goal
        );

        let mut attempt = 0u32;
        let mut last_error = String::new();
        let mut success = false;
        let mut summary = String::new();
        let mut artifacts = None;

        while attempt <= opts.retries_per_step {
            attempt += 1;
            step_counter += 1;
            let attempt_goal = if attempt == 1 {
                goal.clone()
            } else {
                format!(
                    "{goal}\n\nPrevious attempt failed with: {last_error}\nUse a different approach and verify the result."
                )
            };
            let step_dir = run_dir.join(format!(
                "step-{step_counter:02}-{}-a{attempt}",
                step.engine.as_str()
            ));
            // Retries and re-planned steps get a little headroom: a failed
            // attempt usually ran into the budget before it could report done.
            let budget = opts.max_steps_per_engine
                + (attempt.saturating_sub(1)) * 3
                + (replans_used as u32) * 2;
            // A provider/engine error is a failed attempt, not an aborted run:
            // feed it into the retry/re-plan path instead of bubbling up.
            let (s, ok, dir) =
                match execute_step(step.engine, &attempt_goal, budget, &opts, &step_dir).await {
                    Ok(v) => v,
                    Err(e) => (format!("error: {e}"), false, Some(step_dir.clone())),
                };
            summary = clean_summary(&s);
            artifacts = dir;
            success = ok;
            if ok {
                break;
            }
            last_error = summary.clone();
            if attempt <= opts.retries_per_step {
                eprintln!("attempt {attempt} failed: {last_error}; retrying");
            }
        }

        results.push(StepExecResult {
            engine: step.engine,
            goal: goal.clone(),
            summary: summary.clone(),
            success,
            attempts: attempt,
            note: if success {
                None
            } else {
                Some(last_error.clone())
            },
            artifacts,
        });

        if success {
            if let Some(name) = &step.save_as {
                vars.insert(name.clone(), summary);
            }
            index += 1;
            continue;
        }

        if replans_used >= opts.max_replans {
            terminal_failure = true;
            eprintln!("step failed and no re-plans left");
            break;
        }

        replans_used += 1;
        let new_plan = replan(
            &http,
            &config,
            &opts.task,
            &plan,
            &results,
            &vars,
            opts.max_plan_steps,
        )
        .await?;
        eprintln!("\n--- re-plan {replans_used}: {} steps", new_plan.len());
        eprintln!("{}", render_plan(&new_plan));
        std::fs::write(
            run_dir.join(format!("plan-rev{replans_used}.json")),
            serde_json::to_vec_pretty(&new_plan)?,
        )?;
        plan = new_plan;
        index = 0;
    }

    let report = OrchestrateReport {
        success: !terminal_failure,
        plan,
        results,
        run_dir: run_dir.clone(),
        dry_run: false,
        replans_used,
    };
    std::fs::write(
        run_dir.join("results.json"),
        serde_json::to_vec_pretty(&json!({
            "success": report.success,
            "replans_used": report.replans_used,
            "steps": report.results,
        }))?,
    )?;
    eprintln!("\nartifacts: {}", run_dir.display());
    Ok(report)
}

async fn execute_step(
    kind: EngineKind,
    goal: &str,
    budget: u32,
    opts: &OrchestrateOptions,
    step_dir: &Path,
) -> Result<(String, bool, Option<PathBuf>)> {
    match kind {
        EngineKind::Browser => {
            let report = cua_engine_browser::run_task(cua_engine_browser::BrowserRunOptions {
                task: goal.to_string(),
                max_steps: budget,
                dry_run: false,
                model: opts.model.clone(),
                headed: opts.headed,
                start_url: None,
                run_dir: Some(step_dir.to_path_buf()),
                screenshot_quality: 70,
            })
            .await?;
            let ok = report.outcome.is_success();
            Ok((
                report.outcome.summary(report.steps),
                ok,
                Some(report.run_dir),
            ))
        }
        EngineKind::Vision | EngineKind::Code | EngineKind::Access => {
            let kind = if kind == EngineKind::Access {
                EngineKind::Vision
            } else {
                kind
            };
            let report = runner::run_task(RunOptions {
                task: goal.to_string(),
                max_steps: budget,
                yes: opts.yes,
                allow_high: opts.allow_high,
                dry_run: false,
                engine: Some(kind),
                provider: runner::ProviderArg::Auto,
                model: opts.model.clone(),
                max_edge: 1568,
                run_dir: Some(step_dir.to_path_buf()),
                settle_ms: 600,
                ui_tree: false,
                browser_headed: opts.headed,
            })
            .await?;
            let ok = report.is_success();
            Ok((report.summary(), ok, Some(report.run_dir.clone())))
        }
    }
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

fn render_plan(plan: &[PlanStep]) -> String {
    plan.iter()
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
        .collect::<Vec<_>>()
        .join("\n")
}

async fn chat(
    http: &reqwest::Client,
    config: &OpenAiConfig,
    system: &str,
    user: &str,
) -> Result<String> {
    let body = json!({
        "model": config.model,
        "max_tokens": 900,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
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
                    if !content.trim().is_empty() {
                        return Ok(content.to_string());
                    }
                    if let Some(reasoning) = message["reasoning"].as_str() {
                        if reasoning.contains("\"steps\"") {
                            return Ok(reasoning.to_string());
                        }
                    }
                    bail!("planner returned empty content");
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

async fn plan(
    http: &reqwest::Client,
    config: &OpenAiConfig,
    task: &str,
    max_plan_steps: usize,
) -> Result<Vec<PlanStep>> {
    let user = format!("TASK: {task}\nOS: {}", std::env::consts::OS);
    let content = chat(http, config, &planner_prompt(max_plan_steps), &user).await?;
    parse_plan(&content, max_plan_steps)
}

async fn replan(
    http: &reqwest::Client,
    config: &OpenAiConfig,
    task: &str,
    previous: &[PlanStep],
    results: &[StepExecResult],
    vars: &HashMap<String, String>,
    max_plan_steps: usize,
) -> Result<Vec<PlanStep>> {
    let mut completed = String::new();
    let mut failed = String::new();
    for r in results {
        let line = format!("[{}] {} -> {}\n", r.engine.as_str(), r.goal, r.summary);
        if r.success {
            completed.push_str(&line);
        } else {
            failed.push_str(&line);
        }
    }
    let vars_text = if vars.is_empty() {
        "(none)".to_string()
    } else {
        vars.iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = format!(
        "ORIGINAL TASK: {task}\n\nPREVIOUS PLAN:\n{}\n\nCOMPLETED STEPS:\n{}\nFAILED STEPS:\n{}\nAVAILABLE VARIABLES:\n{}\n\n\
         Produce a NEW plan for the REMAINING work only. Do not repeat completed steps. \
         Use a different approach or engine for the failed part. \
         Keep results needed later via save_as and {{name}} references. JSON only.",
        render_plan(previous),
        if completed.is_empty() { "(none)\n" } else { &completed },
        if failed.is_empty() { "(none)\n" } else { &failed },
        vars_text
    );
    let content = chat(http, config, &planner_prompt(max_plan_steps), &user).await?;
    parse_plan(&content, max_plan_steps)
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

    #[test]
    fn renders_plan_with_save_as() {
        let plan = vec![PlanStep {
            engine: EngineKind::Browser,
            goal: "read title".into(),
            save_as: Some("title".into()),
        }];
        assert_eq!(
            render_plan(&plan),
            "1. [browser] read title (save as title)"
        );
    }
}
