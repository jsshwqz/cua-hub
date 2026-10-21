//! Clipboard access with a persistent instance.
//!
//! On X11 the clipboard contents are owned by the process that set them; if the
//! instance is dropped the data disappears (no clipboard manager in a bare X
//! session). Keep one process-wide instance, lazily created.

use std::sync::{Mutex, OnceLock};

use cua_core::{Error, Result};

fn clipboard() -> Result<&'static Mutex<arboard::Clipboard>> {
    static CB: OnceLock<Mutex<arboard::Clipboard>> = OnceLock::new();
    if let Some(cb) = CB.get() {
        return Ok(cb);
    }
    let cb = arboard::Clipboard::new().map_err(|e| Error::Input(format!("clipboard init: {e}")))?;
    let _ = CB.set(Mutex::new(cb));
    Ok(CB.get().expect("just set"))
}

pub fn get_text() -> Result<String> {
    let cb = clipboard()?;
    let mut guard = cb
        .lock()
        .map_err(|e| Error::Input(format!("clipboard lock: {e}")))?;
    guard
        .get_text()
        .map_err(|e| Error::Input(format!("read clipboard text: {e}")))
}

pub fn set_text(text: &str) -> Result<()> {
    let cb = clipboard()?;
    let mut guard = cb
        .lock()
        .map_err(|e| Error::Input(format!("clipboard lock: {e}")))?;
    guard
        .set_text(text.to_string())
        .map_err(|e| Error::Input(format!("write clipboard text: {e}")))
}
