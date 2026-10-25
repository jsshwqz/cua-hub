use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("capture failed: {0}")]
    Capture(String),

    #[error("input injection failed: {0}")]
    Input(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("provider returned no actionable content")]
    NoActionableContent,

    #[error("invalid action from engine: {0}")]
    InvalidAction(String),

    /// The backend for this platform has not implemented the operation at all.
    /// Distinct from "the control does not have that property", which callers
    /// must be able to tell apart before they change strategy.
    #[error("not supported on this platform: {0}")]
    Unsupported(String),

    /// A control could not be located, reached, or answered about -- the failure
    /// is in the lookup, not in the control and not in the capture. Its own
    /// variant because the alternative was reporting these through
    /// `capture failed:`, which sends the reader to a screenshot problem.
    #[error("accessibility lookup failed: {0}")]
    Lookup(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
