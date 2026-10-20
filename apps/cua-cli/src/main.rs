//! cua-cli：Computer Use Agent 的命令行入口。
//!
//! 示例：
//!   cua-cli info
//!   cua-cli screenshot --out /tmp/shot.png
//!   cua-cli exec --file examples/actions.json --mode dry-run
//!   cua-cli exec --file examples/actions.json --mode autonomous

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cua_runtime::{AutoConfirmer, Confirmer, Policy, RunMode, Runtime, StdinConfirmer};
use cua_types::Action;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cua-cli", about = "Computer Use Agent — Rust 执行运行时")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 打印当前屏幕尺寸（连通性自检）
    Info,
    /// 截屏并保存 PNG
    Screenshot {
        #[arg(long, default_value = "/tmp/cua-shot.png")]
        out: PathBuf,
    },
    /// 执行一个 JSON 动作序列
    Exec {
        /// 动作文件：Vec<Action> 的 JSON 数组
        #[arg(long)]
        file: PathBuf,
        /// 运行模式（默认 dry-run：只记录不执行）
        #[arg(long, value_enum, default_value_t = Mode::DryRun)]
        mode: Mode,
        /// 审计日志路径
        #[arg(long, default_value = "audit.jsonl")]
        audit: PathBuf,
        /// 截图输出目录
        #[arg(long, default_value = "screenshots")]
        shots: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    DryRun,
    Supervised,
    Autonomous,
}

impl From<Mode> for RunMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::DryRun => RunMode::DryRun,
            Mode::Supervised => RunMode::Supervised,
            Mode::Autonomous => RunMode::Autonomous,
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Info => {
            let img = cua_runtime::capture::capture_root().context("capture")?;
            println!("screen: {}x{}", img.width, img.height);
        }
        Cmd::Screenshot { out } => {
            let (w, h) = cua_runtime::capture::capture_to_png(&out)?;
            println!("saved {} ({}x{})", out.display(), w, h);
        }
        Cmd::Exec {
            file,
            mode,
            audit,
            shots,
        } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let actions: Vec<Action> = serde_json::from_str(&text).context("parse actions JSON")?;
            println!("loaded {} actions, mode={mode:?}", actions.len());

            let policy = Policy { mode: mode.into() };
            let mut runtime = Runtime::new(policy, audit, shots)?;
            let mut auto = AutoConfirmer;
            let mut stdin_c = StdinConfirmer;
            let confirmer: &mut dyn Confirmer = match mode {
                Mode::Supervised => &mut stdin_c,
                _ => &mut auto,
            };

            let mut executed = 0usize;
            for (i, action) in actions.iter().enumerate() {
                let outcome = runtime.execute(action, confirmer)?;
                if outcome.executed {
                    executed += 1;
                }
                println!(
                    "[{}/{}] {} -> {}",
                    i + 1,
                    actions.len(),
                    action.describe(),
                    if outcome.executed {
                        "executed"
                    } else {
                        "skipped"
                    }
                );
            }
            println!("done: {executed}/{} executed", actions.len());
        }
    }
    Ok(())
}
