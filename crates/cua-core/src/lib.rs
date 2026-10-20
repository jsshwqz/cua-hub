//! Core abstractions shared by every cua-hub crate.
//!
//! The platform is built around three ideas:
//!
//! * [`Action`] — a normalized, engine-agnostic description of something to do
//!   with mouse / keyboard / screen.
//! * [`Observation`] — what the agent can "see" at a given step.
//! * [`Engine`] — a strategy that turns (task, observation) into an [`Action`].

pub mod action;
pub mod error;
pub mod observation;

pub use action::{Action, MouseButton, Point, Risk};
pub use error::{Error, Result};
pub use observation::Observation;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Which engine produced (or should handle) a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// Screenshot -> vision-language model -> action. Universal fallback.
    Vision,
    /// OS accessibility tree (Windows UIA / Linux AT-SPI2). Fast and precise.
    Access,
    /// Browser automation over CDP.
    Browser,
    /// Generated shell/PowerShell scripts for deterministic work.
    Code,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Vision => "vision",
            EngineKind::Access => "access",
            EngineKind::Browser => "browser",
            EngineKind::Code => "code",
        }
    }
}

/// One step of an agent run, kept for logging and for model context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: u32,
    pub action: Action,
    /// Free-form note: engine reasoning, execution outcome, error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub at: chrono::DateTime<chrono::Utc>,
}

impl StepRecord {
    pub fn new(index: u32, action: Action) -> Self {
        Self {
            index,
            action,
            note: None,
            at: chrono::Utc::now(),
        }
    }
}

/// A strategy that decides the next action given the task and current screen.
#[async_trait]
pub trait Engine: Send + Sync {
    fn name(&self) -> &'static str;
    fn kind(&self) -> EngineKind;

    /// Decide and return the next action.
    ///
    /// Implementations may keep internal conversation state; `history` is
    /// provided for stateless engines and for logging context.
    async fn next_action(
        &mut self,
        task: &str,
        observation: &Observation,
        history: &[StepRecord],
    ) -> Result<Action>;

    /// Optional hook called after the action was executed (or attempted).
    async fn on_action_result(&mut self, _record: &StepRecord) {}
}
