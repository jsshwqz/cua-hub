// Probe the Windows.Graphics.Capture backend for one window and print the
// stage that fails, which is the fastest way to tell "WGC is unavailable here"
// apart from "our wiring is wrong".
//
//   cargo run -p cua-io --example wgc_probe -- <hwnd> <width> <height> [out.png]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let parse = |i: usize| args.get(i).and_then(|v| v.parse::<u32>().ok());
    let (Some(hwnd), Some(width), Some(height)) = (parse(1), parse(2), parse(3)) else {
        eprintln!("usage: wgc_probe <hwnd> <width> <height> [out.png]");
        std::process::exit(2);
    };

    #[cfg(windows)]
    match cua_io::wgc::capture_or_error(hwnd, width, height) {
        Ok(image) => {
            println!("ok: {}x{}", image.width(), image.height());
            if let Some(path) = args.get(4) {
                image.save(path).expect("save probe image");
                println!("saved {path}");
            }
        }
        Err(error) => println!("wgc unavailable: {error}"),
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, width, height);
        println!("wgc is windows-only");
    }
}
