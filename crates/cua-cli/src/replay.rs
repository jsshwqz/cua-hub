//! Run history and deterministic replay of a recorded `steps.jsonl`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use cua_core::{Action, StepRecord};
use serde::Serialize;

use crate::runner;

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub dir: PathBuf,
    pub task: String,
    pub outcome: String,
    pub steps: u64,
    pub modified: String,
}

/// List recent runs under `root` (usually `.runs`), newest first.
pub fn list_runs(root: &Path, limit: usize) -> Result<Vec<RunSummary>> {
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let has_log = path.join("steps.jsonl").exists() || path.join("outcome.json").exists();
            if !has_log {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            dirs.push((modified, path));
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));

    let mut out = Vec::new();
    for (modified, path) in dirs.into_iter().take(limit) {
        let meta = read_outcome(&path);
        out.push(RunSummary {
            dir: path,
            task: meta.0,
            outcome: meta.1,
            steps: meta.2,
            modified: chrono::DateTime::<chrono::Local>::from(modified)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        });
    }
    Ok(out)
}

fn read_outcome(dir: &Path) -> (String, String, u64) {
    let value = std::fs::read_to_string(dir.join("outcome.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    match value {
        Some(v) => (
            v["task"].as_str().unwrap_or_default().to_string(),
            v["outcome"].as_str().unwrap_or_default().to_string(),
            v["steps"].as_u64().unwrap_or(0),
        ),
        None => (String::new(), "(no outcome.json)".into(), 0),
    }
}

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub dir: PathBuf,
    pub dry_run: bool,
    pub yes: bool,
    pub allow_high: bool,
    pub max_steps: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub dir: PathBuf,
    pub executed: usize,
    pub skipped: usize,
    pub dry_run: bool,
}

impl ReplayReport {
    pub fn summary(&self) -> String {
        format!(
            "{} replay: {} executed, {} skipped",
            if self.dry_run { "dry-run" } else { "done" },
            self.executed,
            self.skipped
        )
    }
}

pub async fn replay(opts: ReplayOptions) -> Result<ReplayReport> {
    let path = opts.dir.join("steps.jsonl");
    let file = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut records: Vec<StepRecord> = Vec::new();
    for (lineno, line) in std::io::BufRead::lines(reader).enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: StepRecord = serde_json::from_str(&line)
            .with_context(|| format!("parse {} line {}", path.display(), lineno + 1))?;
        records.push(record);
    }
    if records.is_empty() {
        anyhow::bail!("no steps recorded in {}", path.display());
    }

    let mut executed = 0usize;
    let mut skipped = 0usize;

    for record in &records {
        if let Some(max) = opts.max_steps {
            if record.index >= max {
                skipped += 1;
                continue;
            }
        }

        let action = &record.action;
        match action {
            Action::Done { .. } | Action::Fail { .. } | Action::Screenshot => {
                skipped += 1;
                continue;
            }
            Action::Wait { ms } => {
                eprintln!("[{:02}] wait {ms}ms", record.index);
                if !opts.dry_run {
                    tokio::time::sleep(Duration::from_millis(*ms)).await;
                }
                executed += 1;
                continue;
            }
            _ => {}
        }

        eprintln!(
            "[{:02}] {} {} (scale {})",
            record.index,
            action.risk().as_str(),
            runner::describe(action),
            record.input_scale.unwrap_or(1.0)
        );

        if opts.dry_run {
            executed += 1;
            continue;
        }

        if !runner::confirm(action, opts.yes, opts.allow_high)? {
            skipped += 1;
            continue;
        }

        if let Action::RunCommand { command, shell } = action {
            let out = cua_io::run_command(command, *shell).await?;
            eprintln!("      -> {}", out.to_note().replace('\n', " | "));
            executed += 1;
            continue;
        }

        let action = action.clone();
        let scale = record.input_scale.unwrap_or(1.0);
        tokio::task::spawn_blocking(move || cua_io::apply(&action, scale))
            .await
            .context("input task panicked")??;
        executed += 1;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(ReplayReport {
        dir: opts.dir,
        executed,
        skipped,
        dry_run: opts.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_summary_from_outcome() {
        let dir = std::env::temp_dir().join(format!("cua-replay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("outcome.json"),
            r#"{"task":"demo","outcome":"completed: ok","steps":3}"#,
        )
        .unwrap();
        let runs = list_runs(dir.parent().unwrap(), 10).unwrap();
        assert!(runs.iter().any(|r| r.task == "demo" && r.steps == 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_summary_mentions_counts() {
        let r = ReplayReport {
            dir: PathBuf::from("x"),
            executed: 2,
            skipped: 1,
            dry_run: true,
        };
        assert!(r.summary().contains("2 executed"));
    }
}
