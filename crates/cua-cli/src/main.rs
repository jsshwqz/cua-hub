mod mcp;
mod runner;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cua_core::Action;
use runner::{EngineArg, ProviderArg, RunOptions};

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
        /// Skip confirmation for medium-risk actions.
        #[arg(long, env = "CUA_HUB_YES")]
        yes: bool,
        /// Permit high-risk (destructive) actions. Never implied by --yes.
        #[arg(long)]
        allow_high: bool,
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
        /// Show the browser window for browser-routed tasks.
        #[arg(long)]
        browser_headed: bool,
    },
    /// Drive a headless (or visible) Chromium through a natural-language task.
    Browser {
        /// What to do, in natural language.
        task: String,
        #[arg(long, default_value_t = 15)]
        max_steps: u32,
        /// Plan one step without acting.
        #[arg(long)]
        dry_run: bool,
        /// Show the browser window.
        #[arg(long)]
        headed: bool,
        /// Override the model name.
        #[arg(long, env = "CUA_HUB_MODEL")]
        model: Option<String>,
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Optional page to open before the loop starts.
        #[arg(long)]
        start_url: Option<String>,
    },
    /// Speak MCP over stdio so other agents can drive this machine.
    Serve,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cua_hub=info,warn".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
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
            allow_high,
            dry_run,
            engine,
            provider,
            model,
            max_edge,
            run_dir,
            settle_ms,
            ui_tree,
            browser_headed,
        } => {
            let runtime = tokio::runtime::Runtime::new()?;
            let report = runtime.block_on(runner::run_task(RunOptions {
                task,
                max_steps,
                yes,
                allow_high,
                dry_run,
                engine: engine.map(Into::into),
                provider,
                model,
                max_edge,
                run_dir,
                settle_ms,
                ui_tree,
                browser_headed,
            }))?;
            if !report.is_success() {
                bail!("{}", report.summary());
            }
            Ok(())
        }
        Command::Browser {
            task,
            max_steps,
            dry_run,
            headed,
            model,
            run_dir,
            start_url,
        } => {
            let runtime = tokio::runtime::Runtime::new()?;
            let report = runtime.block_on(cua_engine_browser::run_task(
                cua_engine_browser::BrowserRunOptions {
                    task,
                    max_steps,
                    dry_run,
                    model,
                    headed,
                    start_url,
                    run_dir,
                    screenshot_quality: 70,
                },
            ))?;
            if !report.outcome.is_success() {
                bail!("{}", report.outcome.summary(report.steps));
            }
            Ok(())
        }
        Command::Serve => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(mcp::serve())
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

    use cua_router::Router as _;
    let decision = cua_router::RuleRouter::new().route("打开记事本输入 hello");
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
