use serde::{Deserialize, Serialize};

/// What an engine sees at one step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Encoded image bytes (PNG or JPEG depending on `media_type`).
    #[serde(skip)]
    pub image: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    /// Current pointer position, when cheaply available.
    pub cursor: Option<(i32, i32)>,
    /// Optional accessibility summary (filled in by the access engine later).
    pub ui_tree: Option<String>,
}

impl Observation {
    pub fn new(image: Vec<u8>, media_type: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            image,
            media_type: media_type.into(),
            width,
            height,
            cursor: None,
            ui_tree: None,
        }
    }

    pub fn base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&self.image)
    }
}
