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

/// One image inside an observation, with the rectangle it was taken from.
///
/// A window state can carry more than one image: the target window plus the
/// popups its process owns. Menus and tooltips are separate top-level windows,
/// so a coordinate read off a popup image means nothing in the target window's
/// coordinate space — every image therefore declares its own origin and scale
/// and is addressable by id.
#[derive(Debug, Clone, PartialEq)]
pub struct Shot {
    pub id: String,
    /// Window this image shows. Differs from the observation's window for popups.
    pub window_id: u32,
    /// `target` for the observed window, `related` for popups it owns.
    pub role: &'static str,
    /// Physical-pixel top-left of the image.
    pub origin: (i32, i32),
    /// Multiply image pixels by this to get physical pixels.
    pub scale: f32,
    /// Image size in model pixels.
    pub size: (u32, u32),
    /// Average hash of the unscaled capture, for change detection.
    pub hash: u64,
}

/// One observation of one window.
#[derive(Debug, Clone)]
pub struct Observation {
    pub id: String,
    pub window_id: u32,
    /// Combined hash of the window and its popups, for change detection.
    pub hash: Option<u64>,
    /// Every image returned with this observation, target window first.
    pub shots: Vec<Shot>,
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

/// Resolve a point against one named image of an observation.
///
/// `screenshot_id` picks which image the coordinates belong to; `None` means the
/// target window's image. Returns the image alongside the physical point so the
/// caller can decide whether the window needs activating (a popup does not: it
/// is already up, and focusing its owner can dismiss it).
pub fn point_in(
    observation: &Observation,
    screenshot_id: Option<&str>,
    x: f64,
    y: f64,
) -> Option<(Shot, (i32, i32))> {
    let shot = match screenshot_id {
        Some(id) => observation.shots.iter().find(|s| s.id == id)?.clone(),
        None => observation.shots.first()?.clone(),
    };
    if x < 0.0 || y < 0.0 || x > shot.size.0 as f64 || y > shot.size.1 as f64 {
        return None;
    }
    Some((
        shot.clone(),
        (
            shot.origin.0 + (x * shot.scale as f64).round() as i32,
            shot.origin.1 + (y * shot.scale as f64).round() as i32,
        ),
    ))
}

/// The image ids an observation carries, for error messages that should say what
/// the caller could have used.
pub fn shot_ids(observation: &Observation) -> Vec<String> {
    observation.shots.iter().map(|s| s.id.clone()).collect()
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
            signature: format!("/0:Button#{index}"),
            value: String::new(),
            actions: Vec::new(),
            focused: false,
        }
    }

    fn shot(
        id: &str,
        window_id: u32,
        role: &'static str,
        origin: (i32, i32),
        size: (u32, u32),
    ) -> Shot {
        Shot {
            id: id.to_string(),
            window_id,
            role,
            origin,
            scale: 1.0,
            size,
            hash: 7,
        }
    }

    #[test]
    fn observation_maps_image_pixels_to_screen() {
        let id = record(Observation {
            id: next_id(),
            window_id: 7,
            hash: Some(1),
            shots: vec![Shot {
                id: "obs:s0".into(),
                window_id: 7,
                role: "target",
                origin: (100, 200),
                scale: 2.0,
                size: (640, 480),
                hash: 1,
            }],
            nodes: vec![node(0)],
        });
        let found = get(&id).expect("observation should be resolvable");
        assert_eq!(
            point_in(&found, None, 10.0, 20.0).map(|(_, p)| p),
            Some((120, 240))
        );
        assert_eq!(
            point_in(&found, Some("obs:s0"), 10.4, 20.6).map(|(_, p)| p),
            Some((121, 241))
        );
        // Outside the image, in either axis, is refused: a guessed off-image
        // pixel is how automation clicks a neighbour window.
        assert!(point_in(&found, None, 641.0, 20.0).is_none());
        assert!(point_in(&found, None, 10.0, -1.0).is_none());
    }

    #[test]
    fn text_only_observation_has_no_coordinate_space() {
        let id = record(Observation {
            id: next_id(),
            window_id: 8,
            hash: None,
            shots: Vec::new(),
            nodes: vec![node(0)],
        });
        let found = get(&id).expect("observation should be resolvable");
        assert!(point_in(&found, None, 1.0, 1.0).is_none());
        assert!(shot_ids(&found).is_empty());
    }

    #[test]
    fn a_popup_image_maps_in_its_own_coordinate_space() {
        let id = record(Observation {
            id: next_id(),
            window_id: 100,
            hash: Some(1),
            shots: vec![
                shot("obs:s0", 100, "target", (100, 100), (400, 300)),
                shot("obs:s1", 200, "related", (500, 250), (183, 54)),
            ],
            nodes: Vec::new(),
        });
        let found = get(&id).expect("observation should be resolvable");

        // Same image pixels, different windows: the physical points differ.
        let (target_shot, target_point) =
            point_in(&found, Some("obs:s0"), 10.0, 10.0).expect("target");
        assert_eq!(target_shot.role, "target");
        assert_eq!(target_point, (110, 110));

        let (popup_shot, popup_point) =
            point_in(&found, Some("obs:s1"), 10.0, 10.0).expect("popup");
        assert_eq!(popup_shot.window_id, 200);
        assert_eq!(popup_shot.role, "related");
        assert_eq!(popup_point, (510, 260));

        // Outside the popup image, and unknown ids, are both refused.
        assert!(point_in(&found, Some("obs:s1"), 200.0, 10.0).is_none());
        assert!(point_in(&found, Some("obs:nope"), 1.0, 1.0).is_none());
        assert_eq!(
            shot_ids(&found),
            vec!["obs:s0".to_string(), "obs:s1".to_string()]
        );
    }

    #[test]
    fn unknown_observation_id_is_not_resolvable() {
        assert!(get("obs-does-not-exist").is_none());
    }
}
