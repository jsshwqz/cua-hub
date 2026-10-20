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
    visit: &mut dyn FnMut(&UiNode) -> bool,
) -> Result<(bool, bool)> {
    let conn = a11y_connection()?;
    let root_kids = children(&conn, REGISTRY, ROOT_PATH)?;

    let mut truncated = false;
    let mut visited = 0usize;
    let mut stack: Vec<(String, String, usize)> = root_kids
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

        let (name, role, rect, kids) = read_node(&conn, &dest, &path);
        if name.is_empty() && rect.2 == 0 && kids.is_empty() {
            continue;
        }

        visited += 1;
        let node = UiNode {
            depth,
            control_type: control_type_name(role).to_string(),
            name,
            automation_id: String::new(),
            rect,
        };
        if visit(&node) {
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

pub fn snapshot(max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
    let mut nodes = Vec::new();
    let (truncated, _) = walk(max_nodes, max_depth, &mut |node| {
        nodes.push(node.clone());
        false
    })?;
    Ok(UiSnapshot { nodes, truncated })
}

pub fn find(name: &str, max_nodes: usize, max_depth: usize) -> Result<Option<UiNode>> {
    let needle = name.to_lowercase();
    let mut exact: Option<(u8, UiNode)> = None;
    let mut partial: Option<(u8, UiNode)> = None;

    walk(max_nodes, max_depth, &mut |node| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_names_cover_common_controls() {
        assert_eq!(control_type_name(43), "Button"); // push button
        assert_eq!(control_type_name(61), "Text");
        assert_eq!(control_type_name(75), "Application");
    }
}
