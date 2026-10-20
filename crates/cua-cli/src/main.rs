use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cua_core::{Action, Engine, EngineKind, Risk, StepRecord};
use cua_engine_code::CodeEngine;
use cua_engine_vision::{AnthropicConfig, OpenAiConfig, OpenAiVisionEngine, VisionEngine};
use cua_router::{Router, RuleRouter};
use tracing::{info, warn};

#[derive(Parser)]
#[command(
    name = "cua-hub",
    version,
    about = "Multi-engine computer use agent",
    long_about = "Drive a desktop through a vision model, with dry-run previews and a per-run audit log."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check display capture, input injection and model configuration.
    Doctor,
    /// Capture the primary display to a PNG file.
    Capture {
        #[arg(default_value = "capture.png")]
        output: PathBuf,
    },
    /// Click at physical screen coordinates (no model involved).
    Click {
        x: i32,
        y: i32,
        /// Right button instead of left.
        #[arg(long)]
        right: bool,
        /// Double click.
        #[arg(long)]
        double: bool,
    },
    /// Send a key chord, e.g. "ctrl+esc" or "Return".
    Key { combo: String },
    /// Find a control by accessible name and print its rectangle.
    Find {
        name: String,
        #[arg(long, default_value_t = 300)]
        max_nodes: usize,
        #[arg(long, default_value_t = 6)]
        depth: usize,
    },
    /// Find a control by name and click its center.
    Invoke {
        name: String,
        #[arg(long, default_value_t = 300)]
        max_nodes: usize,
        #[arg(long, default_value_t = 6)]
        depth: usize,
    },
    /// Print the Windows accessibility tree of the desktop.
    Tree {
        #[arg(long, default_value_t = 150)]
        max_nodes: usize,
        /// How deep to descend in the tree.
        #[arg(long, default_value_t = 5)]
        depth: usize,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Run a natural-language task on this machine.
    Run {
        /// What to do, in natural language.
        task: String,
        /// Hard limit on agent steps.
        #[arg(long, default_value_t = 30)]
        max_steps: u32,
        /// Skip confirmation for medium/high risk actions.
        #[arg(long, env = "CUA_HUB_YES")]
        yes: bool,
        /// Plan one step only; never touches the machine.
        #[arg(long)]
        dry_run: bool,
        /// Force an engine instead of auto-routing.
        #[arg(long, value_enum)]
        engine: Option<EngineArg>,
        /// Model provider for the vision engine.
        #[arg(long, value_enum, default_value_t = ProviderArg::Auto)]
        provider: ProviderArg,
        /// Override the vision model name.
        #[arg(long, env = "CUA_HUB_MODEL")]
        model: Option<String>,
        /// Longest screenshot edge sent to the model.
        #[arg(long, default_value_t = 1568)]
        max_edge: u32,
        /// Directory for screenshots and the JSONL step log.
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Milliseconds to wait after a mutating action before the next screenshot.
        #[arg(long, default_value_t = 600)]
        settle_ms: u64,
        /// Include a shallow accessibility tree alongside each screenshot.
        #[arg(long)]
        ui_tree: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum EngineArg {
    Vision,
    Access,
    Browser,
    Code,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ProviderArg {
    /// Anthropic if its key is set, otherwise any OpenAI-compatible gateway.
    Auto,
    /// Anthropic Computer Use (native tool protocol).
    Anthropic,
    /// OpenAI / OpenRouter / Ollama / vLLM chat completions.
    Openai,
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cua_hub=info,warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Doctor => doctor(),
        Command::Capture { output } => capture(&output),
        Command::Click {
            x,
            y,
            right,
            double,
        } => {
            let action = Action::Click {
                at: cua_core::Point::new(x, y),
                button: if right {
                    cua_core::MouseButton::Right
                } else {
                    cua_core::MouseButton::Left
                },
                double,
            };
            cua_io::apply(&action, 1.0 / cua_io::dpi_scale())?;
            println!("clicked ({x},{y})");
            Ok(())
        }
        Command::Key { combo } => {
            let action = Action::Key {
                combo: combo.clone(),
            };
            cua_io::apply(&action, 1.0)?;
            println!("sent key {combo}");
            Ok(())
        }
        Command::Find {
            name,
            max_nodes,
            depth,
        } => match cua_engine_access::find(&name, max_nodes, depth)? {
            Some(node) => {
                let (x, y, w, h) = node.rect;
                println!(
                    "{} \"{}\" at ({x},{y},{w},{h}) center=({},{})",
                    node.control_type,
                    node.name,
                    x + w / 2,
                    y + h / 2
                );
                Ok(())
            }
            None => bail!("no control matching {name:?}"),
        },
        Command::Invoke {
            name,
            max_nodes,
            depth,
        } => {
            let node = cua_engine_access::find(&name, max_nodes, depth)?
                .ok_or_else(|| anyhow::anyhow!("no control matching {name:?}"))?;
            let (x, y, w, h) = node.rect;
            let action = Action::Click {
                at: cua_core::Point::new(x + w / 2, y + h / 2),
                button: cua_core::MouseButton::Left,
                double: false,
            };
            // UIA reports DPI-virtualized (logical) rectangles, which is the
            // same space enigo accepts on Windows: no scaling needed.
            cua_io::apply(&action, 1.0)?;
            println!(
                "clicked {} \"{}\" at ({},{})",
                node.control_type,
                node.name,
                x + w / 2,
                y + h / 2
            );
            Ok(())
        }
        Command::Tree {
            max_nodes,
            depth,
            json,
        } => {
            let snapshot = cua_engine_access::snapshot(max_nodes, depth)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print!("{}", snapshot.to_text());
            }
            Ok(())
        }
        Command::Run {
            task,
            max_steps,
            yes,
            dry_run,
            engine,
            provider,
            model,
            max_edge,
            run_dir,
            settle_ms,
            ui_tree,
        } => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(run_task(RunOptions {
                task,
                max_steps,
                yes,
                dry_run,
                engine: engine.map(Into::into),
                provider,
                model,
                max_edge,
                run_dir,
                settle_ms,
                ui_tree,
            }))
        }
    }
}

fn doctor() -> Result<()> {
    println!("cua-hub {}", env!("CARGO_PKG_VERSION"));
    println!("os: {} {}", std::env::consts::OS, std::env::consts::ARCH);

    match cua_io::capture_primary() {
        Ok(c) => println!(
            "display: {}x{} @ ({},{}) scale {:.2} -- OK",
            c.width, c.height, c.origin.0, c.origin.1, c.scale_factor
        ),
        Err(e) => println!("display: FAILED ({e})"),
    }

    match cua_io::cursor_position() {
        Ok((x, y)) => println!("input: pointer at ({x},{y}) -- OK"),
        Err(e) => println!("input: FAILED ({e})"),
    }

    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => {
            let masked = if k.len() > 8 {
                format!("{}...{}", &k[..4], &k[k.len() - 4..])
            } else {
                "****".into()
            };
            println!("api: ANTHROPIC_API_KEY set ({masked})");
        }
        _ => println!("api: ANTHROPIC_API_KEY missing -- vision engine unavailable"),
    }

    match std::env::var("OPENROUTER_API_KEY").or_else(|_| std::env::var("OPENAI_API_KEY")) {
        Ok(k) if !k.is_empty() => {
            let base = std::env::var("CUA_HUB_OPENAI_BASE_URL")
                .or_else(|_| std::env::var("OPENROUTER_BASE_URL"))
                .unwrap_or_else(|_| "https://api.openai.com/v1".into());
            let model = std::env::var("CUA_HUB_MODEL")
                .or_else(|_| std::env::var("OPENROUTER_MODEL"))
                .unwrap_or_else(|_| "gpt-4o-mini".into());
            println!("api: openai-compatible key set, base={base}, model={model}");
        }
        _ => println!("api: no OPENAI_API_KEY / OPENROUTER_API_KEY"),
    }

    let model = std::env::var("CUA_HUB_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".into());
    println!("model: {model}");

    let decision = RuleRouter::new().route("打开记事本输入 hello");
    println!(
        "router: sample task -> {} ({})",
        decision.kind.as_str(),
        decision.reason
    );
    Ok(())
}

fn capture(output: &Path) -> Result<()> {
    let c = cua_io::capture_primary().context("capture failed")?;
    let png = cua_io::encode_png(&c.image)?;
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(output, png)?;
    println!("saved {} ({}x{})", output.display(), c.width, c.height);
    Ok(())
}

struct RunOptions {
    task: String,
    max_steps: u32,
    yes: bool,
    dry_run: bool,
    engine: Option<EngineKind>,
    provider: ProviderArg,
    model: Option<String>,
    max_edge: u32,
    run_dir: Option<PathBuf>,
    settle_ms: u64,
    ui_tree: bool,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Completed(String),
    Failed(String),
    StepLimit,
    DryRun(Action),
}

async fn run_task(opts: RunOptions) -> Result<()> {
    let decision = match opts.engine {
        Some(kind) => RuleRouter::forced(kind).route(&opts.task),
        None => RuleRouter::new().route(&opts.task),
    };
    info!(
        engine = decision.kind.as_str(),
        reason = %decision.reason,
        "routing task"
    );
    if matches!(decision.kind, EngineKind::Access | EngineKind::Browser) {
        warn!(
            requested = decision.kind.as_str(),
            "engine not implemented yet; falling back to vision"
        );
    }

    let provider = match opts.provider {
        ProviderArg::Auto => {
            if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                ProviderArg::Anthropic
            } else if std::env::var("OPENAI_API_KEY").is_ok()
                || std::env::var("OPENROUTER_API_KEY").is_ok()
            {
                ProviderArg::Openai
            } else {
                bail!(
                    "no model credentials: set ANTHROPIC_API_KEY, OPENAI_API_KEY or OPENROUTER_API_KEY"
                );
            }
        }
        p => p,
    };

    let mut engine: Box<dyn Engine> = if decision.kind == EngineKind::Code {
        let config = OpenAiConfig::from_env()?.with_model(opts.model.clone());
        println!("provider: llm-code model={}", config.model);
        Box::new(CodeEngine::new(config))
    } else {
        match provider {
            ProviderArg::Anthropic => {
                let mut config = AnthropicConfig::from_env()?;
                if let Some(m) = opts.model.clone() {
                    config.model = m;
                }
                println!("provider: anthropic model={}", config.model);
                Box::new(VisionEngine::new(config))
            }
            ProviderArg::Openai | ProviderArg::Auto => {
                let config = OpenAiConfig::from_env()?.with_model(opts.model.clone());
                println!("provider: openai-compatible model={}", config.model);
                Box::new(OpenAiVisionEngine::new(config))
            }
        }
    };
    let vision_engine = engine.kind() == EngineKind::Vision;

    if opts.dry_run {
        println!("[dry-run] max_steps={}", opts.max_steps);
    }

    let run_dir = opts.run_dir.unwrap_or_else(default_run_dir);
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
        println!(
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
            outcome = Outcome::DryRun(action);
            break;
        }

        if !confirm(&action, opts.yes)? {
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

        match result {
            Ok(()) => record.note = Some("executed".into()),
            Err(e) => record.note = Some(format!("error: {e}")),
        }
        write_record(&mut log_file, &record)?;
        engine.on_action_result(&record).await;
        history.push(record);

        tokio::time::sleep(Duration::from_millis(opts.settle_ms)).await;
    }

    let summary = match &outcome {
        Outcome::Completed(s) => format!("completed: {s}"),
        Outcome::Failed(s) => format!("failed: {s}"),
        Outcome::StepLimit => format!("stopped after {} steps", opts.max_steps),
        Outcome::DryRun(a) => format!("dry-run planned: {}", describe(a)),
    };
    std::fs::write(
        run_dir.join("outcome.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "task": opts.task,
            "outcome": summary,
            "steps": history.len(),
        }))?,
    )?;
    println!("\n{summary}");
    println!("artifacts: {}", run_dir.display());

    match outcome {
        Outcome::Completed(_) | Outcome::DryRun(_) => Ok(()),
        Outcome::Failed(reason) => bail!("{reason}"),
        Outcome::StepLimit => bail!("step limit reached"),
    }
}

fn confirm(action: &Action, yes: bool) -> Result<bool> {
    if action.risk() < Risk::Medium || yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "{} risk action requires --yes in non-interactive mode",
            action.risk().as_str()
        );
    }
    print!(
        "about to run a {} risk action: {}\nconfirm? [y/N] ",
        action.risk().as_str(),
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

fn describe(action: &Action) -> String {
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
            format!("type {:?}", preview)
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

fn default_run_dir() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(".runs").join(stamp.to_string())
}
