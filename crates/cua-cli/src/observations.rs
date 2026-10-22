//! Short-lived window observations.
//!
//! A screenshot and an accessibility tree are only meaningful together with the
//! rectangle they were taken from: the model reasons in image pixels, and the
//! window may have moved by the time it acts. `window_state` records what it
//! captured here, and input tools resolve their targets against that record so
//! the caller never has to do DPI or origin arithmetic and a stale observation
//! fails loudly instead of clicking the wrong thing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// One observation of one window.
#[derive(Debug, Clone)]
pub struct Observation {
    pub id: String,
    pub window_id: u32,
    /// Physical-pixel top-left of the captured image, when a screenshot was taken.
    pub origin: Option<(i32, i32)>,
    /// Multiply image pixels by this to get physical pixels.
    pub scale: f32,
    /// Image size in model pixels.
    pub size: Option<(u32, u32)>,
    /// Average hash of the unscaled capture, for change detection.
    pub hash: Option<u64>,
    /// Indexed controls, in the order the caller saw them.
    pub nodes: Vec<cua_engine_access::UiNode>,
}

/// How many observations stay resolvable.
const MAX_KEEP: usize = 16;

fn store() -> &'static Mutex<Vec<Observation>> {
    static STORE: OnceLock<Mutex<Vec<Observation>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// Mint an observation id that is unique per process.
pub fn next_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("obs-{n:x}")
}

pub fn record(observation: Observation) -> String {
    let id = observation.id.clone();
    if let Ok(mut guard) = store().lock() {
        guard.push(observation);
        if guard.len() > MAX_KEEP {
            let excess = guard.len() - MAX_KEEP;
            guard.drain(0..excess);
        }
    }
    id
}

pub fn get(id: &str) -> Option<Observation> {
    store()
        .lock()
        .ok()
        .and_then(|guard| guard.iter().find(|o| o.id == id).cloned())
}

/// Convert a point in the observation's image to a physical screen point.
/// Coordinates outside the observed image are rejected: they cannot be mapped,
/// and clicking a guessed off-image pixel is how automation hits a neighbour
/// window.
pub fn to_screen(observation: &Observation, x: f64, y: f64) -> Option<(i32, i32)> {
    let (origin_x, origin_y) = observation.origin?;
    if let Some((w, h)) = observation.size {
        if x < 0.0 || y < 0.0 || x > w as f64 || y > h as f64 {
            return None;
        }
    }
    Some((
        origin_x + (x * observation.scale as f64).round() as i32,
        origin_y + (y * observation.scale as f64).round() as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: usize) -> cua_engine_access::UiNode {
        cua_engine_access::UiNode {
            index,
            depth: 1,
            control_type: "Button".into(),
            name: format!("button {index}"),
            automation_id: String::new(),
            rect: (10, 20, 30, 40),
        }
    }

    #[test]
    fn observation_maps_image_pixels_to_screen() {
        let id = record(Observation {
            id: next_id(),
            window_id: 7,
            origin: Some((100, 200)),
            scale: 2.0,
            size: Some((640, 480)),
            hash: Some(1),
            nodes: vec![node(0)],
        });
        let found = get(&id).expect("observation should be resolvable");
        assert_eq!(to_screen(&found, 10.0, 20.0), Some((120, 240)));
        assert_eq!(to_screen(&found, 10.4, 20.6), Some((121, 241)));
        assert_eq!(to_screen(&found, 641.0, 20.0), None);
        assert_eq!(to_screen(&found, 10.0, -1.0), None);
    }

    #[test]
    fn text_only_observation_has_no_coordinate_space() {
        let id = record(Observation {
            id: next_id(),
            window_id: 8,
            origin: None,
            scale: 1.0,
            size: None,
            hash: None,
            nodes: vec![node(0)],
        });
        let found = get(&id).expect("observation should be resolvable");
        assert_eq!(to_screen(&found, 1.0, 1.0), None);
    }

    #[test]
    fn unknown_observation_id_is_not_resolvable() {
        assert!(get("obs-does-not-exist").is_none());
    }
}
