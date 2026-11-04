//! Real-machine A/B proof that the supervision control bar is invisible to
//! screen captures. Mounts the bar through the production supervision hub,
//! grabs baseline and overlay frames back-to-back through the same
//! `cua_io::observe` pipeline the vision engine feeds to the model, and saves
//! them so a reviewer can diff the bar band: identical bands prove
//! `WDA_EXCLUDEFROMCAPTURE` holds even though the bar is interactive
//! (clickable buttons + drag handle).
//!
//! ```text
//! cargo run -p cua-io --example overlay_capture_proof <out_prefix>
//! ```
//!
//! Writes `<prefix>_baseline.png` and `<prefix>_overlay.png`. The bar is on
//! screen during the second capture; it must NOT appear in the pixels.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    let prefix = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "overlay_proof".into());

    // Baseline: identical desktop, no bar.
    let base = cua_io::observe(1568, 80)?;
    let base_path = format!("{prefix}_baseline.png");
    std::fs::write(&base_path, &base.png)?;

    // Mount the interactive control bar exactly as a supervised run does.
    let sup = cua_io::supervise::ensure(true, "截屏隐形验证", 8)
        .ok_or("control bar did not mount (non-desktop or CUA_HUB_SUPERVISE off)")?;
    sup.update(3, 8, "正在执行物理验证...");

    // Capture the second frame seconds later over the same screen. The bar is
    // interactive (buttons live, drag live) during this call; only its
    // presence differs from baseline.
    std::thread::sleep(Duration::from_millis(900));
    let ov = cua_io::observe(1568, 80)?;
    let ov_path = format!("{prefix}_overlay.png");
    std::fs::write(&ov_path, &ov.png)?;

    println!("baseline: {base_path}");
    println!("overlay : {ov_path}");
    println!("interactive bar was on screen during the second capture; it must NOT appear there.");
    println!("now click its buttons / drag it / press Ctrl+Alt+O to verify interactively.");

    std::thread::sleep(Duration::from_millis(1500));
    sup.end_task("验证完成 · 待机");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("overlay capture proof is Windows-only");
}
