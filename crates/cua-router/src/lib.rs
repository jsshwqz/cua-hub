//! Rule-based routing between engines.
//!
//! Phase 1 ships a deliberately simple router: it selects the engine that is
//! most likely to succeed and records why, so later phases can replace the
//! rules with a learned or model-driven policy without touching callers.

use cua_core::EngineKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub kind: EngineKind,
    /// Engines to try, in order, if the primary is unavailable.
    pub fallbacks: Vec<EngineKind>,
    pub reason: String,
}

pub trait Router: Send + Sync {
    fn route(&self, task: &str) -> RoutingDecision;
}

/// Keyword/pattern router.
#[derive(Debug, Default)]
pub struct RuleRouter {
    forced: Option<EngineKind>,
}

impl RuleRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn forced(kind: EngineKind) -> Self {
        Self { forced: Some(kind) }
    }
}

impl Router for RuleRouter {
    fn route(&self, task: &str) -> RoutingDecision {
        if let Some(kind) = self.forced {
            return RoutingDecision {
                kind,
                fallbacks: vec![],
                reason: "engine forced by caller".into(),
            };
        }

        let t = task.to_ascii_lowercase();

        // Deterministic shell work is cheapest through the code engine.
        let code_hints = [
            "shell",
            "bash",
            "powershell",
            "cmd.exe",
            "run command",
            "执行命令",
            "命令行",
            "脚本",
            "script",
        ];
        // Browser-only work benefits from DOM access.
        let browser_hints = [
            "http://",
            "https://",
            "浏览器",
            "browser",
            "网页",
            "website",
            "url",
        ];

        if code_hints.iter().any(|h| t.contains(h)) {
            return RoutingDecision {
                kind: EngineKind::Code,
                fallbacks: vec![EngineKind::Vision],
                reason: "task looks like deterministic shell work".into(),
            };
        }
        if browser_hints.iter().any(|h| t.contains(h)) {
            return RoutingDecision {
                kind: EngineKind::Browser,
                fallbacks: vec![EngineKind::Vision],
                reason: "task mentions a browser or URL".into(),
            };
        }

        RoutingDecision {
            kind: EngineKind::Vision,
            fallbacks: vec![],
            reason: "default: screenshot + vision model".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_task_routes_to_browser() {
        let d = RuleRouter::new().route("打开 https://example.com 截图");
        assert_eq!(d.kind, EngineKind::Browser);
    }

    #[test]
    fn shell_task_routes_to_code() {
        let d = RuleRouter::new().route("用 powershell 查看磁盘空间");
        assert_eq!(d.kind, EngineKind::Code);
    }

    #[test]
    fn generic_task_routes_to_vision() {
        let d = RuleRouter::new().route("打开记事本输入 hello");
        assert_eq!(d.kind, EngineKind::Vision);
    }

    #[test]
    fn forced_engine_wins() {
        let d = RuleRouter::forced(EngineKind::Vision).route("打开浏览器");
        assert_eq!(d.kind, EngineKind::Vision);
        assert!(d.fallbacks.is_empty());
    }
}
