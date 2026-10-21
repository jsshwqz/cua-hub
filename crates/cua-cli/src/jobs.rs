//! In-process background job registry shared by the MCP server and the console.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::runner::{self, RunOptions};

struct Job {
    task: String,
    status: String,
    run_dir: std::path::PathBuf,
    summary: Option<String>,
    success: Option<bool>,
    steps: usize,
    handle: tokio::task::AbortHandle,
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}",
        chrono::Utc::now().timestamp_millis(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn count_steps(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("steps.jsonl"))
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

fn job_json(id: &str, job: &Job) -> Value {
    json!({
        "job_id": id,
        "task": job.task,
        "status": job.status,
        "steps": job.steps,
        "summary": job.summary,
        "success": job.success,
        "artifacts": job.run_dir.to_string_lossy(),
    })
}

/// Spawn `run_task` in the background; returns the new job id.
pub fn start(mut opts: RunOptions) -> String {
    let id = next_id();
    let run_dir = runner::default_run_dir().join(format!("job-{id}"));
    opts.run_dir = Some(run_dir.clone());
    let task = opts.task.clone();

    let job_id = id.clone();
    let handle = tokio::spawn(async move {
        let result = runner::run_task(opts).await;
        let mut guard = jobs().lock().expect("jobs lock");
        if let Some(job) = guard.get_mut(&job_id) {
            match result {
                Ok(report) => {
                    job.status = if report.is_success() {
                        "completed".into()
                    } else {
                        "failed".into()
                    };
                    job.summary = Some(report.summary());
                    job.success = Some(report.is_success());
                    job.steps = report.steps;
                }
                Err(e) => {
                    job.status = "failed".into();
                    job.summary = Some(format!("error: {e}"));
                    job.success = Some(false);
                }
            }
        }
    });

    jobs().lock().expect("jobs lock").insert(
        id.clone(),
        Job {
            task,
            status: "running".into(),
            run_dir,
            summary: None,
            success: None,
            steps: 0,
            handle: handle.abort_handle(),
        },
    );
    id
}

pub fn snapshot() -> Vec<Value> {
    let mut guard = jobs().lock().expect("jobs lock");
    let mut out: Vec<Value> = guard
        .iter_mut()
        .map(|(id, job)| {
            if job.status == "running" {
                job.steps = count_steps(&job.run_dir);
            }
            job_json(id, job)
        })
        .collect();
    out.sort_by(|a, b| {
        b["job_id"]
            .as_str()
            .unwrap_or_default()
            .cmp(a["job_id"].as_str().unwrap_or_default())
    });
    out
}

pub fn status(id: &str) -> Option<Value> {
    let mut guard = jobs().lock().expect("jobs lock");
    let job = guard.get_mut(id)?;
    if job.status == "running" {
        job.steps = count_steps(&job.run_dir);
    }
    Some(job_json(id, job))
}

pub fn cancel(id: &str) -> Option<String> {
    let mut guard = jobs().lock().expect("jobs lock");
    let job = guard.get_mut(id)?;
    if job.status == "running" {
        job.handle.abort();
        job.status = "cancelled".into();
        job.summary = Some("cancelled".into());
        job.success = Some(false);
    }
    Some(job.status.clone())
}
