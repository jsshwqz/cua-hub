//! Screen capture, image encoding and model-space scaling.

use cua_core::{Error, Observation, Result};
use image::{codecs::jpeg::JpegEncoder, RgbaImage};

#[cfg(windows)]
use crate::wgc;

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
    /// The multiplier from model space to the coordinate space the input layer
    /// expects. The process is per-monitor DPI aware (see `make_dpi_aware`), so
    /// that space is physical pixels and this equals `scale`.
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

/// Put the whole process into one physical-pixel coordinate space.
///
/// A DPI-unaware process is shown virtualised geometry: `GetWindowRect` and UIA
/// bounding boxes come back in logical pixels, `GetSystemMetrics` reports a
/// smaller screen (which is what the input layer normalises against), while the
/// capture backend reports real pixels. Every coordinate then crosses a 1.25x
/// boundary somewhere and clicks land near but not on the target. Declaring
/// per-monitor awareness makes window rects, accessibility rects, screenshots
/// and injected pointer positions agree, so callers can treat all of them as
/// physical pixels. Must run before any window or GDI work; a failure just
/// means the process already declared an awareness mode.
pub fn make_dpi_aware() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }
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

/// How a window image was produced. Callers use this to judge whether the
/// image is the window's own surface or a screen region that may contain
/// whatever was drawn on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// The window's own surface (PrintWindow with PW_RENDERFULLCONTENT).
    /// Occluding windows are not included.
    WindowSurface,
    /// The compositor's output for the window (Windows.Graphics.Capture). Used
    /// when GDI cannot see the surface: hardware-composited video, DRM content,
    /// GPU-only games. Also occlusion-proof.
    GraphicsCapture,
    /// A rectangle grabbed from the screen: correct coordinates, but occluded
    /// pixels show the covering window.
    ScreenRegion,
    /// Platform capture backend (Linux/macOS).
    Platform,
}

/// A window capture plus the on-screen rectangle the image maps to, so
/// window-relative model coordinates can be translated back to absolute input
/// coordinates without the caller doing DPI math.
#[derive(Debug, Clone)]
pub struct WindowCapture {
    pub info: WindowInfo,
    pub frame: Frame,
    pub source: CaptureSource,
    /// Average hash of the unscaled capture, for change detection.
    pub hash: u64,
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
        // Popups such as menus are real windows but are not in the capture
        // library's list, so fall back to the raw HWND before giving up.
        #[cfg(windows)]
        if let Some(id) = wanted_id {
            if let Some(info) = win::info_from_hwnd(id) {
                return capture_hwnd(info, max_edge, jpeg_quality);
            }
        }
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
    let mut info = WindowInfo {
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
    let (image, source) = window_surface(&window, &mut info)?;
    if image.width() == 0 || image.height() == 0 {
        return Err(Error::Capture(format!(
            "window `{}` returned an empty image",
            info.title
        )));
    }
    let hash = average_hash(&image);
    let frame = frame_from_image(image, max_edge, jpeg_quality, dpi_scale())?;
    Ok(WindowCapture {
        info,
        frame,
        source,
        hash,
    })
}

/// Grab the window's own pixels. On Windows this prefers the window surface, so
/// a window hidden behind another one still comes back complete; the returned
/// rectangle in `info` is updated to match the image exactly.
/// Pick the best window-surface backend, in order of trustworthiness.
///
/// PrintWindow is first because it is cheap and exact; a blank result means the
/// window composites on the GPU, where Windows.Graphics.Capture sees what GDI
/// cannot. A screen region is the last resort: correct pixels, but whatever
/// covers the window shows through. `CUA_HUB_CAPTURE_BACKEND=wgc` forces the
/// compositor path first, for diagnostics.
#[cfg(windows)]
fn surface_backends(
    hwnd: windows::Win32::Foundation::HWND,
    info: &WindowInfo,
) -> Option<(RgbaImage, CaptureSource)> {
    let forced = std::env::var("CUA_HUB_CAPTURE_BACKEND")
        .map(|v| v.eq_ignore_ascii_case("wgc"))
        .unwrap_or(false);
    if forced {
        if let Some(image) = wgc::capture(info.id, info.width, info.height) {
            return Some((image, CaptureSource::GraphicsCapture));
        }
    }
    if let Some(image) = win::print_window(hwnd, info.width, info.height) {
        if !win::is_blank(&image) {
            return Some((image, CaptureSource::WindowSurface));
        }
    }
    if let Some(image) = wgc::capture(info.id, info.width, info.height) {
        return Some((image, CaptureSource::GraphicsCapture));
    }
    win::screen_region(info.x, info.y, info.width as i32, info.height as i32)
        .map(|image| (image, CaptureSource::ScreenRegion))
}

#[cfg(windows)]
fn window_surface(
    window: &xcap::Window,
    info: &mut WindowInfo,
) -> Result<(RgbaImage, CaptureSource)> {
    // xcap reports the window rectangle in physical pixels, which is the space
    // every other tool in this server speaks. GetWindowRect from a DPI-unaware
    // process would come back in logical pixels instead, so the rectangle from
    // `info` is what the image is sized and positioned against.
    let hwnd = windows::Win32::Foundation::HWND(info.id as *mut _);
    if let Some(pair) = surface_backends(hwnd, info) {
        return Ok(pair);
    }
    let image = window
        .capture_image()
        .map_err(|e| Error::Capture(format!("capture window `{}`: {e}", info.title)))?;
    Ok((image, CaptureSource::Platform))
}

#[cfg(not(windows))]
fn window_surface(
    window: &xcap::Window,
    _info: &mut WindowInfo,
) -> Result<(RgbaImage, CaptureSource)> {
    let image = window
        .capture_image()
        .map_err(|e| Error::Capture(format!("capture window `{}`: {e}", _info.title)))?;
    Ok((image, CaptureSource::Platform))
}

#[cfg(windows)]
fn capture_hwnd(info: WindowInfo, max_edge: u32, jpeg_quality: u8) -> Result<WindowCapture> {
    let hwnd = windows::Win32::Foundation::HWND(info.id as *mut _);
    let Some((image, source)) = surface_backends(hwnd, &info) else {
        return Err(Error::Capture(format!(
            "cannot capture window {} (`{}`)",
            info.id, info.title
        )));
    };
    let hash = average_hash(&image);
    let frame = frame_from_image(image, max_edge, jpeg_quality, dpi_scale())?;
    Ok(WindowCapture {
        info,
        frame,
        source,
        hash,
    })
}

/// Average hash of an arbitrary image, used to detect "did this action change
/// anything" without shipping pixels back to the caller.
pub fn image_hash(image: &RgbaImage) -> u64 {
    average_hash(image)
}

#[cfg(windows)]
mod win {
    use crate::WindowInfo;
    use image::RgbaImage;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HGDIOBJ, SRCCOPY,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, PW_RENDERFULLCONTENT,
    };

    struct Collect {
        pid: u32,
        exclude: u32,
        limit: usize,
        out: Vec<WindowInfo>,
    }

    /// Visible top-level windows owned by `pid`, including popups that the
    /// capture library does not list (menus are #32768 windows).
    pub fn top_level_for_pid(pid: u32, exclude: u32, limit: usize) -> Vec<WindowInfo> {
        let mut ctx = Collect {
            pid,
            exclude,
            limit,
            out: Vec::new(),
        };
        unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
            let ctx = unsafe { &mut *(lparam.0 as *mut Collect) };
            let id = hwnd.0 as usize as u32;
            let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
            if id != ctx.exclude && ctx.out.len() < ctx.limit && visible {
                let mut owner = 0u32;
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
                if owner == ctx.pid {
                    if let Some(info) = info_from(hwnd, id, owner) {
                        ctx.out.push(info);
                    }
                }
            }
            windows::core::BOOL(1)
        }
        unsafe {
            let _ = EnumWindows(Some(each), LPARAM(&mut ctx as *mut _ as isize));
        }
        ctx.out
    }

    /// Describe a window by HWND, for ids the capture library never enumerated.
    pub fn info_from_hwnd(id: u32) -> Option<WindowInfo> {
        let hwnd = HWND(id as *mut _);
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        info_from(hwnd, id, pid)
    }

    fn info_from(hwnd: HWND, id: u32, pid: u32) -> Option<WindowInfo> {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 1 || height <= 1 {
            return None;
        }
        let mut title = [0u16; 256];
        let title_len = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
        let mut class = [0u16; 256];
        let class_len = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
        Some(WindowInfo {
            id,
            pid,
            app: String::from_utf16_lossy(&class[..class_len]),
            title: String::from_utf16_lossy(&title[..title_len]),
            x: rect.left,
            y: rect.top,
            width: width as u32,
            height: height as u32,
            minimized: false,
            focused: false,
        })
    }

    /// Render the window into a 32-bit top-down DIB and read it back.
    pub fn print_window(hwnd: HWND, width: u32, height: u32) -> Option<RgbaImage> {
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return None;
        }
        draw_into(width as i32, height as i32, |dc| {
            let rendered =
                unsafe { PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)) }
                    .as_bool();
            rendered || unsafe { PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(0)) }.as_bool()
        })
    }

    /// Copy a screen rectangle. Correct coordinates, but occluding windows show
    /// through; used only when the window surface cannot be rendered.
    pub fn screen_region(x: i32, y: i32, width: i32, height: i32) -> Option<RgbaImage> {
        if width <= 0 || height <= 0 || width > 8192 || height > 8192 {
            return None;
        }
        draw_into(width, height, |dc| {
            let screen = unsafe { GetDC(None) };
            let blit = unsafe { BitBlt(dc, 0, 0, width, height, Some(screen), x, y, SRCCOPY) };
            unsafe {
                ReleaseDC(None, screen);
            }
            blit.is_ok()
        })
    }

    fn draw_into(width: i32, height: i32, paint: impl Fn(HDC) -> bool) -> Option<RgbaImage> {
        let mut bits: *mut c_void = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height asks for a top-down DIB, which matches image
                // row order and avoids a flip when copying pixels out.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            let screen = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0);
            let bitmap = match bitmap {
                Ok(b) => b,
                Err(_) => {
                    let _ = DeleteDC(mem_dc);
                    ReleaseDC(None, screen);
                    return None;
                }
            };
            let previous = SelectObject(mem_dc, HGDIOBJ(bitmap.0));
            let painted = paint(mem_dc);
            let _ = SelectObject(mem_dc, previous);
            let image = if painted && !bits.is_null() {
                Some(from_dib(bits as *const u8, width, height))
            } else {
                None
            };
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen);
            image.filter(|img| !is_blank(img))
        }
    }

    fn from_dib(src: *const u8, width: i32, height: i32) -> RgbaImage {
        let mut out = RgbaImage::new(width as u32, height as u32);
        let stride = (width * 4) as usize;
        for y in 0..height as usize {
            let row = unsafe { std::slice::from_raw_parts(src.add(y * stride), stride) };
            for x in 0..width as usize {
                let b = row[x * 4];
                let g = row[x * 4 + 1];
                let r = row[x * 4 + 2];
                out.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, 255]));
            }
        }
        out
    }

    /// A capture where every pixel is black means the window refused to render
    /// (hardware-composited surfaces sometimes do); treat it as a failure so the
    /// caller can fall back instead of handing the model a black frame.
    pub fn is_blank(image: &RgbaImage) -> bool {
        image.pixels().all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0)
    }
}

/// Other visible windows belonging to the same process: menus, tooltips and
/// dialogs are separate top-level windows, so a screenshot of the app window
/// alone cannot show that a menu opened.
#[cfg(windows)]
pub fn related_windows(pid: u32, exclude_id: u32, limit: usize) -> Result<Vec<WindowInfo>> {
    Ok(win::top_level_for_pid(pid, exclude_id, limit))
}

#[cfg(not(windows))]
pub fn related_windows(pid: u32, exclude_id: u32, limit: usize) -> Result<Vec<WindowInfo>> {
    let mut out: Vec<WindowInfo> = list_windows(None, false)?
        .into_iter()
        .filter(|w| {
            w.pid == pid && w.id != exclude_id && !w.minimized && w.width > 1 && w.height > 1
        })
        .collect();
    out.sort_by_key(|w| w.id);
    out.truncate(limit);
    Ok(out)
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
        input_scale: scale,
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
