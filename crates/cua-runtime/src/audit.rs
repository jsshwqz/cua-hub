//! 审计日志：每个动作的判定与结果追加写一行 JSON（JSONL），事后可回放。

use anyhow::{Context, Result};
use cua_types::Action;
use std::io::Write;
use std::path::PathBuf;

pub struct AuditLog {
    path: PathBuf,
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn record(
        &self,
        action: &Action,
        decision: &str,
        result: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let line = serde_json::json!({
            "ts_ms": now_ms(),
            "action": action.describe(),
            "action_json": serde_json::to_value(action).unwrap_or(serde_json::Value::Null),
            "safety": format!("{:?}", action.safety()).to_lowercase(),
            "decision": decision,
            "result": result,
            "detail": detail,
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open audit log {}", self.path.display()))?;
        writeln!(f, "{line}").context("write audit line")?;
        Ok(())
    }
}
