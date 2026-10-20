//! X11 截屏：GetImage 取根窗口 ZPixmap，按 visual 的 RGB mask 解码为 RGBA。
//! 只支持 bits_per_pixel=32 的 24/32 位色深（Xvfb 与主流桌面默认即此）。

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};

pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    /// RGBA 像素，长度 = width * height * 4
    pub rgba: Vec<u8>,
}

pub fn capture_root() -> Result<CapturedImage> {
    let (conn, screen_num) = x11rb::connect(None).context("connect to X server (DISPLAY set?)")?;
    let setup = conn.setup().clone();
    let screen = &setup.roots[screen_num];
    let (width, height) = (
        screen.width_in_pixels as u32,
        screen.height_in_pixels as u32,
    );

    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            0,
            0,
            screen.width_in_pixels,
            screen.height_in_pixels,
            !0,
        )
        .context("send GetImage")?
        .reply()
        .context("GetImage reply")?;

    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == reply.depth)
        .context("pixmap format for depth")?;
    if format.bits_per_pixel != 32 {
        bail!(
            "unsupported bits_per_pixel {} (expected 32)",
            format.bits_per_pixel
        );
    }

    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == reply.visual)
        .context("root visual not found")?;
    let (rm, gm, bm) = (visual.red_mask, visual.green_mask, visual.blue_mask);
    let lsb_first = setup.image_byte_order == ImageOrder::LSB_FIRST;

    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for chunk in reply.data.chunks_exact(4) {
        let px = if lsb_first {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        rgba.push(unmask(px, rm));
        rgba.push(unmask(px, gm));
        rgba.push(unmask(px, bm));
        rgba.push(255);
    }

    Ok(CapturedImage {
        width,
        height,
        rgba,
    })
}

fn unmask(px: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let v = (px & mask) >> shift;
    ((v as u64 * 255) / max as u64) as u8
}

/// 截屏并保存为 PNG，返回 (宽, 高, 路径)。
pub fn capture_to_png(path: &std::path::Path) -> Result<(u32, u32)> {
    let img = capture_root()?;
    let buf = image::RgbaImage::from_raw(img.width, img.height, img.rgba)
        .context("build RGBA image buffer")?;
    buf.save(path).context("save PNG")?;
    Ok((img.width, img.height))
}
