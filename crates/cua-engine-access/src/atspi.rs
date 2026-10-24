//! Linux AT-SPI2 backend.
//!
//! Talks D-Bus directly with zbus: the session bus exposes `org.a11y.Bus`,
//! whose `GetAddress` returns the accessibility bus. The registry root lists
//! applications as children; each node is an `(bus_name, object_path)` pair.
//!
//! Compiled on every host so the code is type-checked in CI and on Windows,
//! but only dispatched to on non-Windows platforms.

use cua_core::{Error, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

use crate::{control_type_name, UiNode, UiSnapshot};

const A11Y_BUS: &str = "org.a11y.Bus";
const A11Y_PATH: &str = "/org/a11y/bus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

type Kids = Vec<(String, OwnedObjectPath)>;
type Rect = (i32, i32, i32, i32);
type NodeData = (String, u32, Rect, Kids);

fn a11y_connection() -> Result<Connection> {
    let session = Connection::session()
        .map_err(|e| Error::Capture(format!("session bus unavailable: {e}")))?;
    let bus = Proxy::new(&session, A11Y_BUS, A11Y_PATH, A11Y_BUS)
        .map_err(|e| Error::Capture(format!("a11y bus proxy: {e}")))?;
    let address: String = bus
        .call("GetAddress", &())
        .map_err(|e| Error::Capture(format!("GetAddress failed: {e}")))?;
    zbus::blocking::connection::Builder::address(address.as_str())
        .and_then(|b| b.build())
        .map_err(|e| Error::Capture(format!("connect a11y bus at {address}: {e}")))
}

fn children(conn: &Connection, dest: &str, path: &str) -> Result<Vec<(String, OwnedObjectPath)>> {
    let proxy = Proxy::new(conn, dest, path, "org.a11y.atspi.Accessible")
        .map_err(|e| Error::Capture(format!("accessible proxy {path}: {e}")))?;
    let kids: Kids = proxy
        .call("GetChildren", &())
        .map_err(|e| Error::Capture(format!("GetChildren {path}: {e}")))?;
    Ok(kids)
}

fn read_node(conn: &Connection, dest: &str, path: &str) -> NodeData {
    let mut name = String::new();
    let mut role = 0u32;
    let mut kids = Vec::new();

    if let Ok(proxy) = Proxy::new(conn, dest, path, "org.a11y.atspi.Accessible") {
        if let Ok(n) = proxy.get_property::<String>("Name") {
            name = n;
        }
        // AT-SPI exposes the role through methods, not a D-Bus property.
        if let Ok(r) = proxy.call::<_, _, u32>("GetRole", &()) {
            role = r;
        } else if let Ok(name) = proxy.call::<_, _, String>("GetRoleName", &()) {
            role = role_from_name(&name);
        }
        if let Ok(k) = proxy.call::<_, _, Vec<(String, OwnedObjectPath)>>("GetChildren", &()) {
            kids = k;
        }
    }

    let rect = Proxy::new(conn, dest, path, "org.a11y.atspi.Component")
        .ok()
        .and_then(|p| {
            p.call::<_, _, (i32, i32, i32, i32)>("GetExtents", &(0u32,))
                .ok()
        })
        .map(|(x, y, w, h)| (x, y, if w > 0 { w } else { 0 }, if h > 0 { h } else { 0 }))
        .unwrap_or((0, 0, 0, 0));

    (name, role, rect, kids)
}

/// Fallback mapping when only a localized role name is available.
fn role_from_name(name: &str) -> u32 {
    match name.to_ascii_lowercase().as_str() {
        "push button" | "button" => 43,
        "text" => 61,
        "entry" | "text entry" => 79,
        "label" => 29,
        "frame" | "window" => 23,
        "application" => 75,
        "menu item" => 35,
        "list item" => 32,
        "check box" => 7,
        "combo box" => 11,
        "page tab" => 37,
        "dialog" => 16,
        "panel" => 39,
        _ => 68,
    }
}

fn walk(
    max_nodes: usize,
    max_depth: usize,
    visit: &mut dyn FnMut(&Connection, &UiNode, &str, &str) -> bool,
) -> Result<(bool, bool)> {
    let conn = a11y_connection()?;
    let root_kids = children(&conn, REGISTRY, ROOT_PATH)?;
    walk_from(&conn, root_kids, max_nodes, max_depth, visit)
}

/// Walk a subtree rooted at the given accessible objects. Shared by the
/// desktop-wide snapshot and by a window-scoped one, which starts at a single
/// frame instead of at every application.
fn walk_from(
    conn: &Connection,
    starts: Vec<(String, OwnedObjectPath)>,
    max_nodes: usize,
    max_depth: usize,
    visit: &mut dyn FnMut(&Connection, &UiNode, &str, &str) -> bool,
) -> Result<(bool, bool)> {
    let mut truncated = false;
    let mut visited = 0usize;
    let mut stack: Vec<(String, String, usize)> = starts
        .into_iter()
        .rev()
        .map(|(dest, path)| (dest, path.to_string(), 0usize))
        .collect();

    while let Some((dest, path, depth)) = stack.pop() {
        if visited >= max_nodes {
            truncated = true;
            break;
        }
        if depth > max_depth {
            truncated = true;
            continue;
        }

        let (name, role, rect, kids) = read_node(conn, &dest, &path);
        if name.is_empty() && rect.2 == 0 && kids.is_empty() {
            continue;
        }

        visited += 1;
        let node = UiNode {
            index: 0,
            depth,
            control_type: control_type_name(role).to_string(),
            name,
            automation_id: String::new(),
            rect,
            // AT-SPI object paths are already a stable per-element identity, so
            // they serve directly as the signature the index registry keys on.
            signature: path.clone(),
            value: String::new(),
            actions: Vec::new(),
            focused: false,
            selected: false,
        };
        if visit(conn, &node, &dest, &path) {
            return Ok((truncated, true));
        }

        if kids.len() + visited > max_nodes {
            truncated = true;
        }
        for (d, p) in kids.into_iter().rev() {
            stack.push((d, p.to_string(), depth + 1));
        }
    }

    Ok((truncated, false))
}

/// Roles that own a window on screen: frame, dialog and window.
const TOP_LEVEL_ROLES: [u32; 3] = [23, 16, 69];

/// The process that owns a connection on the bus this snapshot is reading.
///
/// `org.a11y.atspi.Application` deliberately exposes no process id — its own
/// documentation lists only the toolkit name, an id assigned by the registry,
/// and locale methods. The bus name each application is reached through belongs
/// to a connection, and the daemon that accepted it knows which process made
/// it, which is enough: `WindowInfo.pid` from the window list and this answer
/// are the same number for the same process.
fn bus_pid(conn: &Connection, bus_name: &str) -> Option<u32> {
    let proxy = Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .ok()?;
    // The daemon answers 0 for a name it does not know, which is a miss rather
    // than an error, so it is not a match either.
    proxy
        .call::<_, _, u32>("GetConnectionUnixProcessID", &(bus_name,))
        .ok()
        .filter(|pid| *pid != 0)
}

/// Which of one application's top-level frames a window refers to.
///
/// A title match wins. With no match, a single frame is the obvious owner; more
/// than one is refused, because acting on the wrong window is worse than
/// reporting that the mapping is unavailable. `Err` carries the frame names that
/// were actually seen, which is what a caller needs to correct a stale title.
fn resolve_frame(frames: &[String], title: &str) -> std::result::Result<usize, Vec<String>> {
    if let Some(index) = frames.iter().position(|have| title_matches(have, title)) {
        return Ok(index);
    }
    if frames.len() == 1 {
        return Ok(0);
    }
    Err(frames.to_vec())
}

/// The report a caller gets when no frame could be chosen. The names the bus
/// actually exposed are part of it: a failure that only repeats what was asked
/// for leaves the caller guessing which side of the match was wrong.
fn no_frame_message(app: &str, title: &str, pid: u32, observed: &[String]) -> String {
    let mut out = format!("no AT-SPI frame matches window `{title}` of app `{app}` (pid {pid})");
    if observed.is_empty() {
        out.push_str("; the accessibility bus lists no applications");
    } else {
        let listed = observed
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = observed.len().saturating_sub(8);
        out.push_str(&format!(
            "; applications on the bus are: {listed}{}",
            if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            }
        ));
    }
    out.push_str("; read the desktop-wide tree with accessibility_tree and act by name when a window cannot be matched");
    out
}

/// Snapshot the subtree of the AT-SPI frame that corresponds to one window.
///
/// The frame is found by process id first: the window list and the accessibility
/// bus agree on the owner of a connection even when the toolkit's X11 class and
/// its accessibility name disagree, which is ordinary for scripted GTK
/// applications (`WM_CLASS` is the script filename) and Electron ones
/// (`electron`). Names remain only as a fallback for when the pid cannot be
/// resolved, and an ambiguous title is still refused rather than guessed.
pub fn snapshot_window(
    app: &str,
    title: &str,
    pid: u32,
    max_nodes: usize,
    max_depth: usize,
) -> Result<UiSnapshot> {
    let conn = a11y_connection()?;
    let apps = children(&conn, REGISTRY, ROOT_PATH)?;

    let mut by_pid: Option<(String, OwnedObjectPath)> = None;
    let mut by_name: Option<(String, OwnedObjectPath)> = None;
    let mut observed: Vec<String> = Vec::new();

    for (dest, path) in &apps {
        let (name, _role, _rect, _kids) = read_node(&conn, dest, path);
        observed.push(if name.is_empty() {
            dest.clone()
        } else {
            format!("{name} [{dest}]")
        });
        if by_pid.is_none() && bus_pid(&conn, dest) == Some(pid) {
            by_pid = Some((dest.clone(), path.clone()));
        }
        if by_name.is_none() && app_matches(&name, dest, app) {
            by_name = Some((dest.clone(), path.clone()));
        }
    }

    let (dest, path) = match by_pid.or(by_name) {
        Some(pair) => pair,
        None => return Err(Error::Capture(no_frame_message(app, title, pid, &observed))),
    };

    let (_name, _role, _rect, kids) = read_node(&conn, &dest, &path);
    let frames: Vec<(String, OwnedObjectPath)> = kids
        .into_iter()
        .filter(|(fd, fp)| {
            let (_n, role, _rect, _) = read_node(&conn, fd, fp);
            TOP_LEVEL_ROLES.contains(&role)
        })
        .collect();
    if frames.is_empty() {
        return Err(Error::Capture(no_frame_message(app, title, pid, &observed)));
    }
    let frame_names: Vec<String> = frames
        .iter()
        .map(|(fd, fp)| read_node(&conn, fd, fp).0)
        .collect();
    let chosen = resolve_frame(&frame_names, title).map_err(|seen| {
        Error::Capture(format!(
            "several AT-SPI frames belong to `{app}` (pid {pid}) and none matches `{title}`: [{}]; pass a more exact title",
            seen.join(", ")
        ))
    })?;

    let (dest, path) = frames[chosen].clone();
    let mut nodes = Vec::new();
    let (truncated, _) = walk_from(
        &conn,
        vec![(dest, path)],
        max_nodes,
        max_depth,
        &mut |_, node, _, _| {
            let mut node = node.clone();
            node.index = nodes.len();
            nodes.push(node);
            false
        },
    )?;
    Ok(UiSnapshot { nodes, truncated })
}

/// The application a window list entry refers to. The window list reports a
/// process name, the toolkit registers a display name, and the bus reports an
/// identifier, so any of them may be the one that matches — this is only the
/// fallback now that a process id can decide it outright.
fn app_matches(at_spi_name: &str, bus_name: &str, want: &str) -> bool {
    let want = want.trim().to_lowercase();
    if want.is_empty() {
        return false;
    }
    let candidates = [at_spi_name.trim().to_lowercase(), bus_name.to_lowercase()];
    candidates.iter().any(|c| {
        // An application that reports no name must not match every query: the
        // reverse-containment leg would ask `want.contains("")`, which is true
        // for any string. A toolkit that has not set `g_get_prgname()` really
        // does look like this, so the empty candidate has to be skipped rather
        // than assumed impossible.
        !c.is_empty() && (*c == want || c.contains(&want) || want.contains(c.as_str()))
    })
}

fn title_matches(at_spi_name: &str, want: &str) -> bool {
    let want = want.trim().to_lowercase();
    if want.is_empty() {
        return false;
    }
    let have = at_spi_name.trim().to_lowercase();
    have == want || have.contains(&want) || want.contains(&have)
}

pub fn snapshot(max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
    let mut nodes = Vec::new();
    let (truncated, _) = walk(max_nodes, max_depth, &mut |_, node, _, _| {
        let mut node = node.clone();
        node.index = nodes.len();
        nodes.push(node);
        false
    })?;
    Ok(UiSnapshot { nodes, truncated })
}

/// How actionable a role is, used to break name collisions.
///
/// Mirrors the Windows side, which prefers [Button, MenuItem, ListItem] over a
/// same-named label; without that, a caption sitting next to a button steals the
/// match and the caller presses nothing.
fn role_priority(control_type: &str) -> u8 {
    match control_type {
        "Button" | "PushButton" => 3u8,
        "MenuItem" | "ListItem" | "PageTab" => 2,
        _ => 1,
    }
}

/// Whether a candidate displaces the incumbent, where "nothing yet" loses to
/// anything. Strictly greater keeps the *first* node among equals, so the answer
/// does not change when two identical siblings swap places between observations.
fn beats(incumbent: Option<u8>, candidate: u8) -> bool {
    match incumbent {
        Some(held) => candidate > held,
        None => true,
    }
}

/// The best match seen so far, split into exact and partial buckets.
#[derive(Default)]
struct MatchWinner {
    exact: Option<(u8, UiNode)>,
    partial: Option<(u8, UiNode)>,
}

impl MatchWinner {
    fn consider(&mut self, needle: &str, node: &UiNode) {
        if node.name.is_empty() || node.rect.2 <= 0 || node.rect.3 <= 0 {
            return;
        }
        let priority = role_priority(&node.control_type);
        let hay = node.name.to_lowercase();
        if hay == needle {
            if beats(self.exact.as_ref().map(|(held, _)| *held), priority) {
                self.exact = Some((priority, node.clone()));
            }
        } else if hay.contains(needle)
            && beats(self.partial.as_ref().map(|(held, _)| *held), priority)
        {
            self.partial = Some((priority, node.clone()));
        }
    }

    fn finish(self) -> Option<UiNode> {
        self.exact.or(self.partial).map(|(_, node)| node)
    }
}

/// Choose which of the *visited* controls a query meant.
///
/// Consuming every node is the point: cutting the walk short at the first exact
/// hit returns whichever same-named control the tree happens to list first, and a
/// Label -- which has no action to fire -- often comes first, because that is
/// what a GTK button's own caption looks like from the outside.
fn pick(needle: &str, visited: impl Iterator<Item = UiNode>) -> Option<UiNode> {
    let needle = needle.to_lowercase();
    let mut winner = MatchWinner::default();
    for node in visited {
        winner.consider(&needle, &node);
    }
    winner.finish()
}

pub fn find(name: &str, max_nodes: usize, max_depth: usize) -> Result<Option<UiNode>> {
    let mut visited = Vec::new();
    walk(max_nodes, max_depth, &mut |_, node, _, _| {
        visited.push(node.clone());
        false
    })?;
    Ok(pick(name, visited.into_iter()))
}

/// Fire the control's first AT-SPI action (usually "click"/"press").
pub fn invoke(name: &str) -> Result<Option<UiNode>> {
    let mut hit = None;
    walk(600, 10, &mut |conn, node, dest, path| {
        if node.name != name || node.rect.2 <= 0 {
            return false;
        }
        if let Ok(action) = Proxy::new(conn, dest, path, "org.a11y.atspi.Action") {
            if action.call::<_, _, bool>("DoAction", &(0i32,)).is_ok() {
                hit = Some(node.clone());
                return true;
            }
        }
        false
    })?;
    Ok(hit)
}

/// Replace text through the EditableText interface.
pub fn set_value(name: &str, text: &str) -> Result<Option<UiNode>> {
    let mut hit = None;
    walk(600, 10, &mut |conn, node, dest, path| {
        if node.name != name || node.rect.2 <= 0 {
            return false;
        }
        if let Ok(editable) = Proxy::new(conn, dest, path, "org.a11y.atspi.EditableText") {
            // SetTextContents answers with a boolean. Decoding it as nothing
            // made a call that the toolkit *had* applied look like a control
            // without the interface, which is the opposite of what the caller
            // needs to know.
            if editable
                .call::<_, _, bool>("SetTextContents", &(text,))
                .unwrap_or(false)
            {
                hit = Some(node.clone());
                return true;
            }
        }
        false
    })?;
    Ok(hit)
}

/// Control state is reachable over AT-SPI -- `org.a11y.atspi.Accessible.GetState`
/// returns a StateSet -- but this backend has not wired it up yet. Answering
/// `Ok(None)` here would be a lie of a specific kind: `None` means "this control
/// reports no state", which callers read as licence to fall back to coordinates,
/// when the real fact is that we did not ask.
pub fn state(_name: &str) -> Result<Option<crate::ControlState>> {
    Err(Error::Unsupported(
        "control state is not read over AT-SPI yet; use the tree from snapshot/find instead".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_names_cover_common_controls() {
        assert_eq!(control_type_name(43), "Button"); // push button
        assert_eq!(control_type_name(61), "Text");
        assert_eq!(control_type_name(75), "Application");
    }

    #[test]
    fn apps_match_by_name_or_bus_id() {
        // The window list reports a process name; AT-SPI reports a display name
        // and the bus name reports the identifier. Either may be the match.
        assert!(app_matches("gedit", ":1.42", "gedit"));
        assert!(app_matches("Firefox", "org.mozilla.firefox", "firefox"));
        assert!(app_matches("org.gnome.Nautilus", ":1.7", "nautilus"));
        assert!(!app_matches("Firefox", ":1.2", ""));
        assert!(!app_matches("Firefox", ":1.2", "gedit"));
    }

    #[test]
    fn titles_match_exactly_before_loosely() {
        assert!(title_matches("Untitled — gedit", "Untitled — gedit"));
        assert!(title_matches("Untitled - Notepad", "notepad"));
        assert!(title_matches("Settings", "Settings window"));
        assert!(!title_matches("Settings", ""));
        assert!(!title_matches("Settings", "gedit"));
    }

    #[test]
    fn a_matching_title_wins_and_an_ambiguous_set_is_refused() {
        let frames = vec!["Untitled — gedit".to_string(), "Settings".to_string()];
        assert_eq!(resolve_frame(&frames, "settings"), Ok(1));
        // One frame needs no title to be identified; several do.
        assert_eq!(
            resolve_frame(&["Only".to_string()], "something else"),
            Ok(0)
        );
        assert_eq!(
            resolve_frame(&frames, "").unwrap_err().len(),
            2,
            "the refusal must name what was available"
        );
        assert!(resolve_frame(&[], "anything").is_err());
    }

    #[test]
    fn a_refused_match_says_what_the_bus_actually_show() {
        let message = no_frame_message(
            "electron",
            "My App",
            4242,
            &["gedit [:1.7]".to_string(), "Navigator [:1.9]".to_string()],
        );
        assert!(
            message.contains("`My App`"),
            "keeps the title that was asked for"
        );
        assert!(
            message.contains("pid 4242"),
            "keeps the owner it was looking for"
        );
        assert!(
            message.contains("gedit [:1.7]"),
            "reports the names the bus really exposed, not just the request"
        );
        assert!(
            message.contains("accessibility_tree"),
            "and points at the route that still works"
        );
        assert!(no_frame_message("x", "y", 1, &[]).contains("no applications"));
    }

    #[test]
    fn a_nameless_application_matches_no_query() {
        // `want.contains("")` is true for every want, so an application that
        // never set its program name used to answer to every lookup and could
        // win the window that `snapshot_window` was trying to identify.
        assert!(!app_matches("", "", "thunar"));
        assert!(!app_matches("", ":1.7", "thunar"));
        assert!(!app_matches("Thunar", "", ""));
        // A bus name is still a usable identity when there is no display name.
        assert!(app_matches("", ":1.7", ":1.7"));
    }

    /// A control as the AT-SPI walk hands one over: the fields the matcher reads
    /// are real, the rest are inert.
    fn node(control_type: &str, name: &str) -> UiNode {
        UiNode {
            index: 0,
            depth: 0,
            control_type: control_type.into(),
            name: name.into(),
            automation_id: String::new(),
            rect: (10, 10, 80, 24),
            signature: String::new(),
            value: String::new(),
            actions: Vec::new(),
            focused: false,
            selected: false,
        }
    }

    #[test]
    fn the_button_wins_over_the_label_listed_before_it() {
        // The probe window is exactly this shape: a bare label, then a button
        // with the same accessible name.
        let visited = vec![
            node("Label", "probe-ambiguous"),
            node("Button", "probe-ambiguous"),
        ];
        let hit = pick("probe-ambiguous", visited.into_iter()).expect("a match");
        assert_eq!(hit.control_type, "Button", "the actionable role must win");
    }

    #[test]
    fn an_exact_name_beats_a_more_actionable_partial_one() {
        let visited = vec![
            node("Button", "probe-submit-now"),
            node("MenuItem", "submit"),
        ];
        let hit = pick("submit", visited.into_iter()).expect("a match");
        assert_eq!(hit.name, "submit", "exact outranks role");
    }

    #[test]
    fn among_equals_the_first_seen_still_wins() {
        let mut left = node("Button", "OK");
        left.automation_id = "left".into();
        let mut right = node("Button", "OK");
        right.automation_id = "right".into();
        let hit = pick("ok", vec![left, right].into_iter()).expect("a match");
        assert_eq!(
            hit.automation_id, "left",
            "ties must not churn between observations"
        );
    }

    #[test]
    fn unnamed_and_off_screen_controls_are_not_candidates() {
        let mut zero_width = node("Button", "OK");
        zero_width.rect = (0, 0, 0, 40);
        let blank = node("Button", "");
        assert!(pick("ok", vec![zero_width, blank].into_iter()).is_none());
    }
}
