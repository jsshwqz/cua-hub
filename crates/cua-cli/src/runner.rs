//! Shared task runner used by the CLI and the MCP server.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use cua_core::{Action, Engine, EngineKind, Risk, StepRecord};
use cua_engine_code::CodeEngine;
use cua_engine_vision::{AnthropicConfig, OpenAiConfig, OpenAiVisionEngine, VisionEngine};
use cua_router::{Router, RuleRouter};
use tracing::{info, warn};

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum EngineArg {
    Vision,
    Access,
    Browser,
    Code,
}

impl From<EngineArg> for EngineKind {
    fn from(v: EngineArg) -> Self {
        match v {
            EngineArg::Vision => EngineKind::Vision,
            EngineArg::Access => EngineKind::Access,
            EngineArg::Browser => EngineKind::Browser,
            EngineArg::Code => EngineKind::Code,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    /// Anthropic if its key is set, otherwise any OpenAI-compatible gateway.
    Auto,
    /// Anthropic Computer Use (native tool protocol).
    Anthropic,
    /// OpenAI / OpenRouter / Ollama / vLLM chat completions.
    Openai,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub task: String,
    pub max_steps: u32,
    /// Skip confirmation for medium-risk actions.
    pub yes: bool,
    /// Permit high-risk actions (destructive commands); never implied by `yes`.
    pub allow_high: bool,
    pub dry_run: bool,
    pub engine: Option<EngineKind>,
    pub provider: ProviderArg,
    pub model: Option<String>,
    pub max_edge: u32,
    pub run_dir: Option<PathBuf>,
    pub settle_ms: u64,
    pub ui_tree: bool,
    /// Show the browser window instead of headless mode.
    pub browser_headed: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            task: String::new(),
            max_steps: 20,
            yes: false,
            allow_high: false,
            dry_run: false,
            engine: None,
            provider: ProviderArg::Auto,
            model: None,
            max_edge: 1568,
            run_dir: None,
            settle_ms: 600,
            ui_tree: false,
            browser_headed: false,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Completed(String),
    Failed(String),
    StepLimit,
    /// Planned action already described as text.
    DryRun(String),
}

#[derive(Debug)]
pub struct RunReport {
    pub outcome: Outcome,
    pub run_dir: PathBuf,
    pub steps: usize,
}

impl RunReport {
    pub fn summary(&self) -> String {
        match &self.outcome {
            Outcome::Completed(s) => format!("completed: {s}"),
            Outcome::Failed(s) => format!("failed: {s}"),
            Outcome::StepLimit => format!("stopped after {} steps", self.steps),
            Outcome::DryRun(planned) => format!("dry-run planned: {planned}"),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self.outcome, Outcome::Completed(_) | Outcome::DryRun(_))
    }
}

pub async fn run_task(opts: RunOptions) -> Result<RunReport> {
    let decision = match opts.engine {
        Some(kind) => RuleRouter::forced(kind).route(&opts.task),
        None => RuleRouter::new().route(&opts.task),
    };
    info!(
        engine = decision.kind.as_str(),
        reason = %decision.reason,
        "routing task"
    );
    if decision.kind == EngineKind::Access {
        warn!(
            requested = decision.kind.as_str(),
            "engine not implemented yet; falling back to vision"
        );
    }

    // Browser tasks run in their own CDP session and action space.
    if decision.kind == EngineKind::Browser {
        let report = cua_engine_browser::run_task(cua_engine_browser::BrowserRunOptions {
            task: opts.task.clone(),
            max_steps: opts.max_steps,
            dry_run: opts.dry_run,
            model: opts.model.clone(),
            headed: opts.browser_headed,
            start_url: None,
            run_dir: opts.run_dir.clone(),
            screenshot_quality: 70,
        })
        .await?;
        let outcome = match report.outcome {
            cua_engine_browser::BrowserOutcome::Completed(s) => Outcome::Completed(s),
            cua_engine_browser::BrowserOutcome::Failed(s) => Outcome::Failed(s),
            cua_engine_browser::BrowserOutcome::StepLimit => Outcome::StepLimit,
            cua_engine_browser::BrowserOutcome::DryRun(s) => Outcome::DryRun(s),
        };
        return Ok(RunReport {
            outcome,
            run_dir: report.run_dir,
            steps: report.steps,
        });
    }

    let provider = resolve_provider(opts.provider)?;
    let mut engine = build_engine(decision.kind, provider, opts.model.clone())?;
    let vision_engine = engine.kind() == EngineKind::Vision;

    if opts.dry_run {
        eprintln!("[dry-run] max_steps={}", opts.max_steps);
    }

    let run_dir = opts.run_dir.clone().unwrap_or_else(default_run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let log_path = run_dir.join("steps.jsonl");
    let mut log_file = std::fs::File::create(&log_path)?;
    let mut history: Vec<StepRecord> = Vec::new();
    let mut outcome = Outcome::StepLimit;

    for index in 0..opts.max_steps {
        let frame = if vision_engine {
            let frame = cua_io::observe(opts.max_edge, 80)?;
            let shot_path = run_dir.join(format!("step-{index:02}.png"));
            std::fs::write(&shot_path, &frame.png)?;
            Some(frame)
        } else {
            None
        };
        let mut observation = frame
            .as_ref()
            .map(|f| f.observation.clone())
            .unwrap_or_else(cua_core::Observation::empty);
        if opts.ui_tree && vision_engine {
            // Depth 1 keeps the per-step COM cost under a second; deeper trees
            // are available through `cua-hub tree` for debugging.
            match cua_engine_access::snapshot(60, 1) {
                Ok(snap) => observation.ui_tree = Some(snap.to_text()),
                Err(e) => tracing::warn!(error = %e, "ui tree snapshot failed"),
            }
        }

        let action = engine
            .next_action(&opts.task, &observation, &history)
            .await
            .with_context(|| format!("engine failed at step {index}"))?;

        let mut record = StepRecord::new(index, action.clone());
        eprintln!(
            "[{index:02}] {} {}",
            action.risk().as_str(),
            describe(&action)
        );

        match action {
            Action::Done { summary } => {
                record.note = Some(format!("completed: {summary}"));
                write_record(&mut log_file, &record)?;
                outcome = Outcome::Completed(summary);
                break;
            }
            Action::Fail { reason } => {
                record.note = Some(format!("failed: {reason}"));
                write_record(&mut log_file, &record)?;
                outcome = Outcome::Failed(reason);
                break;
            }
            Action::Wait { ms } => {
                write_record(&mut log_file, &record)?;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                continue;
            }
            Action::Screenshot => {
                write_record(&mut log_file, &record)?;
                continue;
            }
            _ => {}
        }

        if opts.dry_run {
            write_record(&mut log_file, &record)?;
            outcome = Outcome::DryRun(describe(&action));
            break;
        }

        if !confirm(&action, opts.yes, opts.allow_high)? {
            record.note = Some("skipped: user declined".into());
            write_record(&mut log_file, &record)?;
            outcome = Outcome::Failed("action declined by user".into());
            break;
        }

        if let Action::RunCommand { command, shell } = &action {
            match cua_io::run_command(command, *shell).await {
                Ok(out) => record.note = Some(out.to_note()),
                Err(e) => record.note = Some(format!("error: {e}")),
            }
            record.input_scale = frame.as_ref().map(|f| f.input_scale);
            write_record(&mut log_file, &record)?;
            engine.on_action_result(&record).await;
            history.push(record);
            continue;
        }

        let result = tokio::task::spawn_blocking({
            let action = action.clone();
            let scale = frame.as_ref().map(|f| f.input_scale).unwrap_or(1.0);
            move || cua_io::apply(&action, scale)
        })
        .await
        .context("input task panicked")?;

        record.input_scale = frame.as_ref().map(|f| f.input_scale);
        match result {
            Ok(()) => record.note = Some("executed".into()),
            Err(e) => record.note = Some(format!("error: {e}")),
        }
        write_record(&mut log_file, &record)?;
        engine.on_action_result(&record).await;
        history.push(record);

        tokio::time::sleep(Duration::from_millis(opts.settle_ms)).await;
    }

    let report = RunReport {
        outcome,
        run_dir: run_dir.clone(),
        steps: history.len(),
    };
    std::fs::write(
        run_dir.join("outcome.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "task": opts.task,
            "outcome": report.summary(),
            "steps": report.steps,
        }))?,
    )?;
    eprintln!("\n{}", report.summary());
    eprintln!("artifacts: {}", run_dir.display());
    Ok(report)
}

fn resolve_provider(requested: ProviderArg) -> Result<ProviderArg> {
    if requested != ProviderArg::Auto {
        return Ok(requested);
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return Ok(ProviderArg::Anthropic);
    }
    if std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("OPENROUTER_API_KEY").is_ok() {
        return Ok(ProviderArg::Openai);
    }
    bail!("no model credentials: set ANTHROPIC_API_KEY, OPENAI_API_KEY or OPENROUTER_API_KEY")
}

fn build_engine(
    kind: EngineKind,
    provider: ProviderArg,
    model: Option<String>,
) -> Result<Box<dyn Engine>> {
    if kind == EngineKind::Code {
        let config = OpenAiConfig::from_env()?.with_model(model);
        eprintln!("provider: llm-code model={}", config.model);
        return Ok(Box::new(CodeEngine::new(config)));
    }

    match provider {
        ProviderArg::Anthropic => {
            let mut config = AnthropicConfig::from_env()?;
            if let Some(m) = model {
                config.model = m;
            }
            eprintln!("provider: anthropic model={}", config.model);
            Ok(Box::new(VisionEngine::new(config)))
        }
        ProviderArg::Openai | ProviderArg::Auto => {
            let config = OpenAiConfig::from_env()?.with_model(model);
            eprintln!("provider: openai-compatible model={}", config.model);
            Ok(Box::new(OpenAiVisionEngine::new(config)))
        }
    }
}

pub fn confirm(action: &Action, yes: bool, allow_high: bool) -> Result<bool> {
    let risk = action.risk();
    if risk == Risk::High && !allow_high {
        bail!(
            "high-risk action blocked: {}. Re-run with --allow-high / allow_high=true if intended",
            describe(action)
        );
    }
    if risk < Risk::Medium || yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "{} risk action requires --yes in non-interactive mode",
            risk.as_str()
        );
    }
    print!(
        "about to run a {} risk action: {}\nconfirm? [y/N] ",
        risk.as_str(),
        describe(action)
    );
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn write_record(file: &mut std::fs::File, record: &StepRecord) -> Result<()> {
    let line = serde_json::to_string(record)?;
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

pub fn describe(action: &Action) -> String {
    match action {
        Action::Screenshot => "screenshot".into(),
        Action::Click { at, button, double } => format!(
            "{}{}click at ({}, {})",
            if *double { "double " } else { "" },
            button_name(*button),
            at.x,
            at.y
        ),
        Action::Move { to } => format!("move to ({}, {})", to.x, to.y),
        Action::Drag { from, to } => {
            format!("drag ({}, {}) -> ({}, {})", from.x, from.y, to.x, to.y)
        }
        Action::Type { text, .. } => {
            let preview: String = text.chars().take(60).collect();
            format!("type {preview:?}")
        }
        Action::Key { combo } => format!("key {combo}"),
        Action::Scroll { at, dx, dy } => {
            format!("scroll ({dx}, {dy}) at ({}, {})", at.x, at.y)
        }
        Action::Wait { ms } => format!("wait {ms}ms"),
        Action::RunCommand { command, shell } => {
            let preview: String = command.chars().take(120).collect();
            format!("run [{shell:?}] {preview}")
        }
        Action::Done { summary } => format!("done: {summary}"),
        Action::Fail { reason } => format!("fail: {reason}"),
    }
}

fn button_name(b: cua_core::MouseButton) -> &'static str {
    match b {
        cua_core::MouseButton::Left => "left ",
        cua_core::MouseButton::Right => "right ",
        cua_core::MouseButton::Middle => "middle ",
    }
}

pub fn default_run_dir() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(".runs").join(stamp.to_string())
}
