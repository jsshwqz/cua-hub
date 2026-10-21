//! Screen capture, image encoding and model-space scaling.

use cua_core::{Error, Observation, Result};
use image::{codecs::jpeg::JpegEncoder, RgbaImage};

/// A captured primary display, in physical pixels.
#[derive(Debug, Clone)]
pub struct Captured {
    pub image: RgbaImage,
    pub width: u32,
    pub height: u32,
    pub origin: (i32, i32),
    pub scale_factor: f32,
}

/// One observation ready to be sent to an engine, plus the data needed to
/// translate model coordinates back to physical pixels and to archive the frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub observation: Observation,
    /// physical = model * scale
    pub scale: f32,
    /// input = physical / dpi, i.e. the multiplier from model space to the
    /// coordinate space used by enigo (logical pixels on Windows/macOS).
    pub input_scale: f32,
    pub dpi: f32,
    pub png: Vec<u8>,
    pub physical: (u32, u32),
}

/// DPI scale of the primary monitor (1.0 at 100% scaling).
pub fn dpi_scale() -> f32 {
    xcap::Monitor::all()
        .ok()
        .and_then(|m| {
            m.iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .or_else(|| m.first())
                .and_then(|m| m.scale_factor().ok())
        })
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0)
}

/// Capture the primary display.
pub fn capture_primary() -> Result<Captured> {
    let monitors =
        xcap::Monitor::all().map_err(|e| Error::Capture(format!("enumerate monitors: {e}")))?;
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| Error::Capture("no monitor found".into()))?;

    let image = monitor
        .capture_image()
        .map_err(|e| Error::Capture(format!("capture image: {e}")))?;

    Ok(Captured {
        width: image.width(),
        height: image.height(),
        origin: (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0)),
        scale_factor: monitor.scale_factor().unwrap_or(1.0),
        image,
    })
}

/// A top-level window that can be captured independently of the desktop.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: u32,
    pub app: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
    pub focused: bool,
}

/// A window capture plus its on-screen origin, so window-relative model
/// coordinates can be translated back to absolute input coordinates.
#[derive(Debug, Clone)]
pub struct WindowCapture {
    pub info: WindowInfo,
    pub frame: Frame,
}

/// List capturable top-level windows, optionally filtered by a case-insensitive
/// substring of the title or application name.
pub fn list_windows(query: Option<&str>, include_minimized: bool) -> Result<Vec<WindowInfo>> {
    let windows =
        xcap::Window::all().map_err(|e| Error::Capture(format!("enumerate windows: {e}")))?;
    let needle = query.map(str::to_lowercase);
    let mut out = Vec::new();
    for window in windows {
        let title = window.title().unwrap_or_default();
        let app = window.app_name().unwrap_or_default();
        if title.is_empty() && app.is_empty() {
            continue;
        }
        let minimized = window.is_minimized().unwrap_or(false);
        if minimized && !include_minimized {
            continue;
        }
        if let Some(needle) = &needle {
            if !title.to_lowercase().contains(needle) && !app.to_lowercase().contains(needle) {
                continue;
            }
        }
        out.push(WindowInfo {
            id: window.id().unwrap_or(0),
            pid: window.pid().unwrap_or(0),
            app,
            title,
            x: window.x().unwrap_or(0),
            y: window.y().unwrap_or(0),
            width: window.width().unwrap_or(0),
            height: window.height().unwrap_or(0),
            minimized,
            focused: window.is_focused().unwrap_or(false),
        });
    }
    out.sort_by(|a, b| {
        b.focused
            .cmp(&a.focused)
            .then_with(|| a.app.to_lowercase().cmp(&b.app.to_lowercase()))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    Ok(out)
}

/// Capture one window by numeric id or by a case-insensitive title/app selector.
/// The capture is window-scoped, so it still works when another window covers it
/// (depending on the platform capture backend).
pub fn capture_window(selector: &str, max_edge: u32, jpeg_quality: u8) -> Result<WindowCapture> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(Error::Capture("window selector is empty".into()));
    }
    let wanted_id = selector.parse::<u32>().ok();
    let windows =
        xcap::Window::all().map_err(|e| Error::Capture(format!("enumerate windows: {e}")))?;
    let mut matches: Vec<_> = windows
        .into_iter()
        .filter(|window| {
            let id_match = wanted_id.is_some_and(|id| window.id().ok() == Some(id));
            let title = window.title().unwrap_or_default();
            let app = window.app_name().unwrap_or_default();
            id_match
                || title.eq_ignore_ascii_case(selector)
                || app.eq_ignore_ascii_case(selector)
                || title.to_lowercase().contains(&selector.to_lowercase())
                || app.to_lowercase().contains(&selector.to_lowercase())
        })
        .collect();
    if matches.is_empty() {
        return Err(Error::Capture(format!("no window matches `{selector}`")));
    }
    // Prefer an exact title, then the focused window, then the first match.
    matches.sort_by_key(|window| {
        let exact = window
            .title()
            .map(|t| !t.eq_ignore_ascii_case(selector))
            .unwrap_or(true);
        let focused = !window.is_focused().unwrap_or(false);
        (exact, focused)
    });
    let window = matches.remove(0);
    let info = WindowInfo {
        id: window.id().unwrap_or(0),
        pid: window.pid().unwrap_or(0),
        app: window.app_name().unwrap_or_default(),
        title: window.title().unwrap_or_default(),
        x: window.x().unwrap_or(0),
        y: window.y().unwrap_or(0),
        width: window.width().unwrap_or(0),
        height: window.height().unwrap_or(0),
        minimized: window.is_minimized().unwrap_or(false),
        focused: window.is_focused().unwrap_or(false),
    };
    if info.minimized {
        return Err(Error::Capture(format!(
            "window `{}` is minimized; restore it first",
            info.title
        )));
    }
    let image = window
        .capture_image()
        .map_err(|e| Error::Capture(format!("capture window `{}`: {e}", info.title)))?;
    if image.width() == 0 || image.height() == 0 {
        return Err(Error::Capture(format!(
            "window `{}` returned an empty image",
            info.title
        )));
    }
    let frame = frame_from_image(image, max_edge, jpeg_quality, dpi_scale())?;
    Ok(WindowCapture { info, frame })
}

/// Capture, downscale to `max_edge`, and encode both JPEG (for the model) and
/// PNG (for the local run log).
pub fn observe(max_edge: u32, jpeg_quality: u8) -> Result<Frame> {
    let captured = capture_primary()?;
    let dpi = if captured.scale_factor > 0.0 {
        captured.scale_factor
    } else {
        1.0
    };
    frame_from_image(captured.image, max_edge, jpeg_quality, dpi)
}

fn frame_from_image(image: RgbaImage, max_edge: u32, jpeg_quality: u8, dpi: f32) -> Result<Frame> {
    let physical = image.dimensions();
    let dpi = if dpi > 0.0 { dpi } else { 1.0 };
    let png = encode_png(&image)?;
    let (resized, scale) = fit_to(image, max_edge);
    let (w, h) = resized.dimensions();

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, jpeg_quality)
        .encode_image(&resized)
        .map_err(|e| Error::Capture(format!("jpeg encode: {e}")))?;

    Ok(Frame {
        observation: Observation::new(jpeg, "image/jpeg", w, h),
        scale,
        input_scale: scale / dpi,
        dpi,
        png,
        physical,
    })
}

/// Resize so the longest edge is at most `max_edge`. Returns the image and the
/// model->physical scale factor (physical = model * factor).
pub fn fit_to(image: RgbaImage, max_edge: u32) -> (RgbaImage, f32) {
    let (w, h) = image.dimensions();
    let longest = w.max(h);
    if longest <= max_edge || max_edge == 0 {
        return (image, 1.0);
    }
    let factor = longest as f32 / max_edge as f32;
    let nw = ((w as f32 / factor).round() as u32).max(1);
    let nh = ((h as f32 / factor).round() as u32).max(1);
    let resized = image::imageops::resize(&image, nw, nh, image::imageops::FilterType::Triangle);
    (resized, factor)
}

/// Cheap 8x8 average hash of the screen, used to tell whether an action
/// changed anything. Coarse on purpose: identical hashes mean a static screen.
pub fn screen_hash() -> Result<u64> {
    let captured = capture_primary()?;
    Ok(average_hash(&captured.image))
}

fn average_hash(image: &RgbaImage) -> u64 {
    let small = image::imageops::resize(image, 8, 8, image::imageops::FilterType::Triangle);
    let luma: Vec<u32> = small
        .pixels()
        .map(|p| (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000)
        .collect();
    let avg = luma.iter().sum::<u32>() / luma.len().max(1) as u32;
    let mut hash = 0u64;
    for (i, value) in luma.iter().enumerate().take(64) {
        if *value >= avg {
            hash |= 1 << i;
        }
    }
    hash
}

/// Encode a screenshot as PNG (lossless, for local logs).
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|e| Error::Capture(format!("png encode: {e}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn identical_images_hash_equal() {
        let a = RgbaImage::from_pixel(16, 16, Rgba([10, 20, 30, 255]));
        let b = a.clone();
        assert_eq!(average_hash(&a), average_hash(&b));
    }

    #[test]
    fn different_images_hash_differ() {
        let a = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255]));
        let mut b = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255]));
        for x in 0..8 {
            for y in 0..8 {
                b.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        assert_ne!(average_hash(&a), average_hash(&b));
    }
}
