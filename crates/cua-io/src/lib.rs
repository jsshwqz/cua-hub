//! Screen capture and input injection for cua-hub.

pub mod capture;
pub mod input;

pub use capture::{capture_primary, dpi_scale, encode_png, fit_to, observe, Captured, Frame};
pub use input::{apply, cursor_position};
