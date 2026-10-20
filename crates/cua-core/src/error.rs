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

    #[error("configuration error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
