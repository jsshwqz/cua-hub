//! The desktop half of the browser engine's cursor hint.
//!
//! The browser engine works out where an element sits on screen but deliberately
//! does not link an input backend, so it can keep running where there is no
//! pointer at all. The move itself therefore lives here — the one crate that
//! already depends on both sides.

use std::sync::Arc;

use anyhow::anyhow;
use cua_core::{Action, Point};
use cua_engine_browser::PointerMover;

/// Move the real cursor onto a screen point given in desktop-independent pixels.
///
/// The engine speaks DIP because a page cannot see the OS scaling; `dpi_scale` is
/// the multiply back to the physical pixels the input layer works in — the same
/// boundary every other coordinate in this process crosses. Nothing is pressed:
/// this is a pointer sitting still on purpose, not an input.
pub(crate) fn pointer_mover() -> PointerMover {
    Arc::new(|to: Point| {
        cua_io::apply(&Action::Move { to }, cua_io::dpi_scale())
            .map_err(|e| anyhow!("move cursor to ({},{}) failed: {e}", to.x, to.y))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_engine_browser::{launch, LaunchOptions};

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    }

    /// Real Chrome, real desktop, real cursor. The hint has to put the pointer on
    /// the element and hold it there long enough that the page's own click is
    /// measurably later than the aim — and the pointer has to end up where the
    /// engine aimed it *after* the DIP-to-physical multiply, which is the one part
    /// of this feature no mock can witness.
    ///
    /// Nothing here is simulated, which is why CI cannot run it: there is no
    /// pointer to move. Locally: `cargo test -p cua-cli -- --ignored`
    #[tokio::test]
    #[ignore = "needs a local Chrome/Edge and a desktop session with a real cursor"]
    async fn the_cursor_reaches_the_target_before_the_page_is_clicked() {
        let mut session = launch(LaunchOptions {
            headed: true,
            ..Default::default()
        })
        .await
        .expect("launch a headed browser");

        session
            .eval(
                r#"(() => {
                    document.body.innerHTML =
                      '<button id=go style="position:absolute;left:140px;top:140px;width:240px;height:80px">go</button>';
                    window.__clickedAt = 0;
                    document.getElementById('go')
                      .addEventListener('click', () => { window.__clickedAt = Date.now(); });
                })()"#,
            )
            .await
            .expect("build the fixture page");

        let aimed = Arc::new(std::sync::Mutex::new(Vec::<(i64, Point)>::new()));
        let sink = aimed.clone();
        let real = pointer_mover();
        session.set_pointer_hint(Some(Arc::new(move |p| {
            sink.lock().unwrap().push((now_ms(), p));
            real(p)
        })));

        let note = session.click("#go").await.expect("click the fixture");

        let shots = aimed.lock().unwrap().clone();
        assert_eq!(shots.len(), 1, "one aim per action, got {shots:?}");
        let (aimed_at, point) = shots[0];

        let clicked_at = session
            .eval("window.__clickedAt")
            .await
            .unwrap()
            .as_f64()
            .unwrap_or_default();
        assert!(clicked_at > 0.0, "the page never saw a click: {note}");
        let dwell = clicked_at as i64 - aimed_at;
        assert!(
            dwell >= 250,
            "the page was clicked {dwell}ms after aiming; the hint should have held ~300ms first"
        );

        // The engine aims in DIP; the screen speaks physical pixels.
        let scale = cua_io::dpi_scale();
        let want = (
            (point.x as f32 * scale).round() as i32,
            (point.y as f32 * scale).round() as i32,
        );
        let got = cua_io::cursor_position().expect("read the real cursor");
        assert!(
            (got.0 - want.0).abs() <= 3 && (got.1 - want.1).abs() <= 3,
            "cursor ended at {got:?}, but the aim mapped to {want:?} (dip {point:?} x {scale})"
        );
        eprintln!(
            "[pointer] aimed dip=({},{}) scale={scale:.2} expected_px=({},{}) actual_px=({},{}) dwell={dwell}ms",
            point.x, point.y, want.0, want.1, got.0, got.1
        );
    }

    /// The other half of the promise: with no window on screen there is nothing to
    /// point at, so the hint stays out of the way and the action still happens.
    #[tokio::test]
    #[ignore = "needs a local Chrome/Edge"]
    async fn a_headless_run_still_acts_and_never_reaches_for_the_cursor() {
        let mut session = launch(LaunchOptions {
            ..Default::default()
        })
        .await
        .expect("launch a headless browser");

        session
            .eval(
                r#"(() => {
                    document.body.innerHTML = '<input id=field>';
                })()"#,
            )
            .await
            .expect("build the fixture page");

        let touched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = touched.clone();
        let real = pointer_mover();
        session.set_pointer_hint(Some(Arc::new(move |p| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            real(p)
        })));

        let note = session
            .type_text("#field", "typed anyway")
            .await
            .expect("type into the fixture");
        assert!(
            !touched.load(std::sync::atomic::Ordering::SeqCst),
            "a headless browser has no cursor to move, yet the mover was called"
        );
        assert!(
            note.contains("typed 12 chars into #field") && note.contains("value reads as sent"),
            "the action itself must not change: {note}"
        );
        eprintln!("[pointer] headless: mover never called, action note = {note}");
    }
}
