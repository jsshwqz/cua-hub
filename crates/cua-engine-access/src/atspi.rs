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

fn app_matches(at_spi_name: &str, bus_name: &str, want: &str) -> bool {
    let want = want.to_lowercase();
    if want.is_empty() {
        return false;
    }
    let candidates = [at_spi_name.to_lowercase(), bus_name.to_lowercase()];
    candidates
        .iter()
        .any(|c| *c == want || c.contains(&want) || want.contains(c.as_str()))
}

fn title_matches(at_spi_name: &str, want: &str) -> bool {
    let want = want.trim().to_lowercase();
    if want.is_empty() {
        return false;
    }
    let have = at_spi_name.trim().to_lowercase();
    have == want || have.contains(&want) || want.contains(&have)
}

/// Snapshot the subtree of the AT-SPI frame that corresponds to one window.
///
/// AT-SPI does not expose X11 window ids, so the frame is found by matching the
/// owning application's name and the frame title — the same two fields the
/// window list reports. A title match is preferred; an application with exactly
/// one top level falls back to it, and anything more specific than that is
/// refused rather than guessed, because acting on the wrong window is worse
/// than reporting that the mapping is unavailable.
pub fn snapshot_window(
    app: &str,
    title: &str,
    max_nodes: usize,
    max_depth: usize,
) -> Result<UiSnapshot> {
    let conn = a11y_connection()?;
    let apps = children(&conn, REGISTRY, ROOT_PATH)?;

    let mut chosen: Option<(String, OwnedObjectPath)> = None;
    let mut ambiguous = false;
    for (dest, path) in &apps {
        let (name, _role, _rect, kids) = read_node(&conn, dest, path);
        if !app_matches(&name, dest, app) {
            continue;
        }
        let tops: Vec<(String, OwnedObjectPath)> = kids
            .iter()
            .filter(|(fd, fp)| {
                let (_n, role, _rect, _) = read_node(&conn, fd, fp);
                TOP_LEVEL_ROLES.contains(&role)
            })
            .cloned()
            .collect();
        if tops.is_empty() {
            continue;
        }
        if let Some(hit) = tops
            .iter()
            .find(|(fd, fp)| title_matches(&read_node(&conn, fd, fp).0, title))
        {
            chosen = Some(hit.clone());
            break;
        }
        if tops.len() == 1 {
            chosen = Some(tops[0].clone());
            break;
        }
        ambiguous = true;
    }

    let (dest, path) = match chosen {
        Some(pair) => pair,
        None if ambiguous => {
            return Err(Error::Capture(format!(
                "several AT-SPI frames belong to `{app}` and none matches `{title}`; pass a more exact title"
            )))
        }
        None => {
            return Err(Error::Capture(format!(
                "no AT-SPI frame matches app `{app}` title `{title}`"
            )))
        }
    };

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

pub fn find(name: &str, max_nodes: usize, max_depth: usize) -> Result<Option<UiNode>> {
    let needle = name.to_lowercase();
    let mut exact: Option<(u8, UiNode)> = None;
    let mut partial: Option<(u8, UiNode)> = None;

    walk(max_nodes, max_depth, &mut |_, node, _, _| {
        if node.name.is_empty() || node.rect.2 <= 0 || node.rect.3 <= 0 {
            return false;
        }
        let role_priority = match node.control_type.as_str() {
            "Button" | "PushButton" => 3u8,
            "MenuItem" | "ListItem" | "PageTab" => 2,
            _ => 1,
        };
        let hay = node.name.to_lowercase();
        if hay == needle {
            if exact
                .as_ref()
                .map(|(p, _)| role_priority > *p)
                .unwrap_or(true)
            {
                exact = Some((role_priority, node.clone()));
            }
            return true;
        }
        if hay.contains(&needle)
            && partial
                .as_ref()
                .map(|(p, _)| role_priority > *p)
                .unwrap_or(true)
        {
            partial = Some((role_priority, node.clone()));
        }
        false
    })?;

    Ok(exact.or(partial).map(|(_, node)| node))
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
            if editable
                .call::<_, _, ()>("SetTextContents", &(text,))
                .is_ok()
            {
                hit = Some(node.clone());
                return true;
            }
        }
        false
    })?;
    Ok(hit)
}

/// AT-SPI state flags are not exposed as a simple property on the D-Bus
/// interface; the caller can still use `snapshot`/`find` for structure.
pub fn state(_name: &str) -> Result<Option<crate::ControlState>> {
    Ok(None)
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
}
