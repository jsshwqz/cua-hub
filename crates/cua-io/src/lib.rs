//! Screen capture and input injection for cua-hub.

pub mod capture;
pub mod clipboard;
pub mod exec;
pub mod input;

pub use capture::{
    capture_primary, capture_window, dpi_scale, encode_png, fit_to, image_hash, list_windows,
    make_dpi_aware, observe, related_windows, screen_hash, CaptureSource, Captured, Frame,
    WindowCapture, WindowInfo,
};
pub use clipboard::{get_text as clipboard_get, set_text as clipboard_set};
pub use exec::{run_command, CommandOutput};
pub use input::{apply, cursor_position, key_state, mouse_button};
