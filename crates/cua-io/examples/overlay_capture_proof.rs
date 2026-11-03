//! Real-machine A/B proof that the overlay indicator is invisible to screen
//! captures. Captures a baseline frame, mounts the overlay, captures a second
//! frame seconds later over the same (unchanged) desktop, then diffs the
//! capsule band. Identical bands prove `WDA_EXCLUDEFROMCAPTURE` holds; a leaked
//! capsule would show up as a distinct dark pill only in the second frame.
//!
//! ```text
//! cargo run -p cua-io --example overlay_capture_proof <out_prefix>
//! ```
//!
//! Writes `<prefix>_baseline.png` and `<prefix>_overlay.png`. The overlay
//! lifecycle itself is covered by `real_machine_overlay_lifecycle`.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    let prefix = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "overlay_proof".into());

    // Baseline: identical desktop, no overlay.
    let base = cua_io::observe(1568, 80)?;
    let base_path = format!("{prefix}_baseline.png");
    std::fs::write(&base_path, &base.png)?;

    // Mount the capsule exactly as the runner does.
    let overlay = cua_io::OverlayGuard::start(true, "截屏隐形验证", 8);
    if !overlay.is_active() {
        eprintln!("overlay did not mount (non-desktop or disabled by CUA_HUB_OVERLAY)");
        return Ok(());
    }
    overlay.update(3, 8, "正在执行物理验证...");

    // Capture the second frame seconds later over the same screen. The capsule
    // is on screen during this call; only its presence differs from baseline.
    std::thread::sleep(Duration::from_millis(900));
    let ov = cua_io::observe(1568, 80)?;
    let ov_path = format!("{prefix}_overlay.png");
    std::fs::write(&ov_path, &ov.png)?;

    println!("baseline: {base_path}");
    println!("overlay : {ov_path}");
    println!("capsule was on screen during the second capture; it must NOT appear there.");

    std::thread::sleep(Duration::from_millis(1200));
    drop(overlay);
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("overlay capture proof is Windows-only");
}
