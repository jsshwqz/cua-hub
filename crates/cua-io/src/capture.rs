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

/// Capture, downscale to `max_edge`, and encode both JPEG (for the model) and
/// PNG (for the local run log).
pub fn observe(max_edge: u32, jpeg_quality: u8) -> Result<Frame> {
    let captured = capture_primary()?;
    let physical = (captured.width, captured.height);
    let dpi = if captured.scale_factor > 0.0 {
        captured.scale_factor
    } else {
        1.0
    };
    let png = encode_png(&captured.image)?;
    let (resized, scale) = fit_to(captured.image, max_edge);
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
