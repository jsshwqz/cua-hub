//! Windows.Graphics.Capture as an alternative window-surface backend.
//!
//! `PrintWindow` asks a window to redraw itself into our DC, which works for
//! ordinary apps and survives occlusion, but returns a black rectangle for
//! surfaces that never paint through GDI: hardware-accelerated video, DRM
//! protected content, some GPU-composited games and overlays. WGC captures the
//! compositor's output for the window instead, so those surfaces come through.
//!
//! It is used as a fallback when PrintWindow produces a blank image, or forced
//! with `CUA_HUB_CAPTURE_BACKEND=wgc` for diagnostics. Failures carry the stage
//! that refused, because "WGC did not work" is not actionable while "the window
//! refused a capture item" is. The capture contract (physical pixels,
//! caller-supplied size) is identical for both backends.

use image::RgbaImage;

/// Capture one window's compositor output, or explain which stage failed.
///
/// `width`/`height` are the window's physical size; the returned image is
/// cropped to the frame's content size, which can be smaller mid-resize.
pub fn capture_or_error(hwnd: u32, width: u32, height: u32) -> Result<RgbaImage, String> {
    use std::time::{Duration, Instant};

    use windows::core::{Interface as _, HSTRING};
    use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
    use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Win32::Foundation::{HMODULE, HWND};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::System::WinRT::RoGetActivationFactory;

    // Turn a COM result into a stage-tagged error.
    macro_rules! stage {
        ($name:expr, $expr:expr) => {
            match $expr {
                Ok(value) => value,
                Err(error) => return Err(format!("{}: {error}", $name)),
            }
        };
    }

    // WGC is a WinRT API: the calling thread needs a COM apartment. MTA is what
    // the rest of cua-io already uses.
    let _ = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };

    // A BGRA-capable device: the capture pool only hands out B8G8R8A8 surfaces.
    let mut device: Option<ID3D11Device> = None;
    stage!("d3d11 device", unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    });
    let device = device.ok_or_else(|| "d3d11 device: none returned".to_string())?;
    let dxgi: IDXGIDevice = stage!("dxgi device", device.cast());
    let inspectable = stage!("winrt device", unsafe {
        CreateDirect3D11DeviceFromDXGIDevice(&dxgi)
    });
    let d3d_device: IDirect3DDevice = stage!("winrt device cast", inspectable.cast());

    // The hwnd factory is a WinRT interop interface, not a static method.
    let interop: IGraphicsCaptureItemInterop = stage!("capture factory", unsafe {
        RoGetActivationFactory(&HSTRING::from(
            "Windows.Graphics.Capture.GraphicsCaptureItem",
        ))
    });
    let item: GraphicsCaptureItem = stage!("capture item for window", unsafe {
        interop.CreateForWindow(HWND(hwnd as *mut _))
    });
    let size = stage!("item size", item.Size());
    let (item_w, item_h) = (size.Width.max(1) as u32, size.Height.max(1) as u32);

    let pool = stage!(
        "frame pool",
        Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
    );
    let session = stage!("capture session", pool.CreateCaptureSession(&item));
    // Without this the capture includes the system's cursor, which would bake
    // our own pointer into every screenshot. Older builds lack the property, so
    // a failure here is not fatal.
    let _ = session.SetIsCursorCaptureEnabled(false);
    stage!("start capture", session.StartCapture());

    // The first frame arrives on the next compositor tick; give it a moment and
    // then give up so the caller can fall back.
    let deadline = Instant::now() + Duration::from_millis(3_000);
    let frame = loop {
        match pool.TryGetNextFrame() {
            Ok(frame) => break frame,
            Err(_) => {
                if Instant::now() >= deadline {
                    let _ = session.Close();
                    let _ = pool.Close();
                    return Err("no frame within 3000ms".to_string());
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        }
    };

    let content = stage!("frame content size", frame.ContentSize());
    let surface = stage!("frame surface", frame.Surface());
    let access: IDirect3DDxgiInterfaceAccess = stage!("dxgi interface access", surface.cast());
    // The surface wraps a texture on the device we created, so asking for the
    // D3D11 interface gives us something we can copy out of.
    let texture: ID3D11Texture2D = stage!("texture from surface", unsafe { access.GetInterface() });

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging: Option<ID3D11Texture2D> = None;
    stage!("staging texture", unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut staging))
    });
    let staging = staging.ok_or_else(|| "staging texture: none returned".to_string())?;

    let context = stage!("immediate context", unsafe { device.GetImmediateContext() });
    unsafe { context.CopyResource(&staging, &texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    stage!("map staging", unsafe {
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
    });
    if mapped.pData.is_null() {
        unsafe { context.Unmap(&staging, 0) };
        return Err("map staging: null data".to_string());
    }
    let copy_w = (content.Width.max(0) as u32)
        .min(desc.Width)
        .min(width.max(item_w));
    let copy_h = (content.Height.max(0) as u32)
        .min(desc.Height)
        .min(height.max(item_h));
    let mut out = RgbaImage::new(copy_w, copy_h);
    let stride = mapped.RowPitch as usize;
    let src = mapped.pData as *const u8;
    for y in 0..copy_h {
        let row = unsafe {
            std::slice::from_raw_parts(src.add(y as usize * stride), copy_w as usize * 4)
        };
        for x in 0..copy_w {
            let b = row[x as usize * 4];
            let g = row[x as usize * 4 + 1];
            let r = row[x as usize * 4 + 2];
            out.put_pixel(x, y, image::Rgba([r, g, b, 255]));
        }
    }
    unsafe { context.Unmap(&staging, 0) };
    let _ = frame.Close();
    let _ = session.Close();
    let _ = pool.Close();
    Ok(out)
}

/// Capture one window's compositor output. `None` means "unavailable, use
/// another backend"; [`capture_or_error`] carries the reason.
pub fn capture(hwnd: u32, width: u32, height: u32) -> Option<RgbaImage> {
    capture_or_error(hwnd, width, height).ok()
}
