//! Linux AT-SPI2 backend.
//!
//! Talks D-Bus directly with zbus: the session bus exposes `org.a11y.Bus`,
//! whose `GetAddress` returns the accessibility bus. The registry root lists
//! applications as children; each node is an `(bus_name, object_path)` pair.
//!
//! Compiled on every host so the code is type-checked in CI and on Windows,
//! but only dispatched to on non-Windows platforms.

use cua_core::{Error, Result};
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

use crate::{control_type_name, ActionReport, ControlState, Effect, UiNode, UiSnapshot};

const A11Y_BUS: &str = "org.a11y.Bus";
const A11Y_PATH: &str = "/org/a11y/bus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

type Kids = Vec<(String, OwnedObjectPath)>;
type Rect = (i32, i32, i32, i32);
type NodeData = (String, u32, Rect, Kids);

/// How long to wait for one AT-SPI reply before calling the application deaf.
///
/// The D-Bus default is around 25 seconds, and a hung application would take a
/// whole observation down with it -- which is worse than a shorter tree, because
/// the shorter tree used to arrive looking complete. Unanswered reads are
/// reported in `UiSnapshot::unreadable` instead.
const CALL_TIMEOUT: Duration = Duration::from_millis(1200);

/// Do not poke a bus that just went deaf, for this long.
///
/// A read that overruns its deadline cannot be cancelled: the worker thread stays
/// parked on the socket. The cooldown keeps that to one parked thread per wedged
/// application per window instead of one per observation, which is the difference
/// between a survivable leak and an unbounded one.
const WEDGED_COOLDOWN: Duration = Duration::from_secs(30);

/// Buses known to have gone deaf, with when they last did.
static WEDGED: std::sync::Mutex<Option<std::collections::HashMap<String, Instant>>> =
    std::sync::Mutex::new(None);

/// A read that the peer did not answer in time.
enum Stall {
    /// No reply within the deadline; the read is now parked on a worker thread.
    NoReply,
    /// Not even attempted: the same bus went deaf inside the cooldown window.
    CoolingDown,
}

/// Why a single bus call ended the way it did.
#[derive(Debug)]
enum Attempt<T> {
    /// The toolkit replied.
    Answered(T),
    /// It answered "I do not implement that": a fact about the control.
    NoPattern(String),
    /// It said nothing, or was already known deaf. We know nothing here.
    Unanswered(String),
}

/// One method call, deadline-bounded and classified.
///
/// The three-way split is the point. Before this, silence fell through to the
/// same branch as "no such interface", so an application that never answered was
/// reported as a control that cannot be invoked -- and the caller's documented
/// response to that is a coordinate click.
fn attempted<T, F>(conn: &Connection, bus: &str, work: F) -> Attempt<T>
where
    T: Send + 'static,
    F: FnOnce(Connection) -> std::result::Result<T, String> + Send + 'static,
{
    let owned = conn.clone();
    classify(timed(bus, move || work(owned)))
}

/// The classification on its own, so it can be tested without a bus: silence, an
/// absent interface and a reply are three different facts.
fn classify<T>(outcome: std::result::Result<std::result::Result<T, String>, Stall>) -> Attempt<T> {
    match outcome {
        Err(Stall::NoReply) => {
            Attempt::Unanswered(format!("no reply within {}ms", CALL_TIMEOUT.as_millis()))
        }
        Err(Stall::CoolingDown) => Attempt::Unanswered(format!(
            "not attempted: this bus was deaf within the last {}s",
            WEDGED_COOLDOWN.as_secs()
        )),
        Ok(Ok(value)) => Attempt::Answered(value),
        Ok(Err(reason)) => match unanswered(&reason) {
            Some(_) => Attempt::Unanswered(reason),
            None => Attempt::NoPattern(reason),
        },
    }
}

/// The one failure a caller must not act on, phrased so they cannot misread it.
fn unanswered_call(what: &str, name: &str, reason: &str) -> Error {
    Error::Lookup(format!(
        "did not get an answer for {what} on {name:?}: {reason}. This is not \"the control has no {what} pattern\" -- nobody replied, so the control may well support it"
    ))
}

/// Build a proxy and call one method, keeping the bus's own wording for failures.
fn proxy_call<T, B>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> std::result::Result<T, String>
where
    T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    match Proxy::new(conn, dest, path, iface) {
        Ok(proxy) => proxy
            .call::<_, _, T>(method, body)
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn mark_wedged(bus: &str) {
    if let Ok(mut guard) = WEDGED.lock() {
        let map = guard.get_or_insert_with(Default::default);
        map.insert(bus.to_string(), Instant::now());
    }
}

/// Clear any recorded deaf-bus cooldowns so a caller retrying an empty or
/// failed snapshot can attempt a fresh bus read immediately.
pub fn reset_wedged() {
    if let Ok(mut guard) = WEDGED.lock() {
        if let Some(map) = guard.as_mut() {
            map.clear();
        }
    }
}

fn is_wedged(bus: &str) -> bool {
    let Ok(guard) = WEDGED.lock() else {
        return false;
    };
    let Some(map) = guard.as_ref() else {
        return false;
    };
    map.get(bus)
        .is_some_and(|since| since.elapsed() < WEDGED_COOLDOWN)
}

/// Run one round of bus reads where it cannot pin the caller.
///
/// zbus 5's blocking API has no client-side reply timeout: `Proxy::call` and
/// `blocking::Connection::call_method` both just `block_on` the async call, and
/// `Builder::method_timeout` is honoured only by the async `Connection::call`,
/// which the blocking type does not expose. Measured on CI (run 40118056194): a
/// connection built with `method_timeout(1200ms)` still had not returned after
/// 180 seconds against a peer under `SIGSTOP`.
///
/// So the deadline lives here. The work runs on a worker thread holding its own
/// clone of the connection; the caller waits `CALL_TIMEOUT` and, when that
/// expires, reports the subtree as unreadable rather than hanging the walk.
fn timed<T, F>(bus: &str, work: F) -> std::result::Result<T, Stall>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if is_wedged(bus) {
        return Err(Stall::CoolingDown);
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel::<T>(1);
    // A panicked worker is treated as no reply: the node is then reported rather
    // than silently absent, which is the whole point of the ledger.
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map_err(|_| Stall::NoReply)?;
    match receiver.recv_timeout(CALL_TIMEOUT) {
        Ok(value) => Ok(value),
        Err(_) => {
            mark_wedged(bus);
            Err(Stall::NoReply)
        }
    }
}

/// How long one tree walk may spend waiting on the bus in total.
///
/// The per-call timeout bounds a single read; this bounds the observation. A
/// desktop with several wedged applications would otherwise multiply
/// `calls x timeout` into something the caller cannot wait for, and "give up
/// politely and say what was not attempted" is the contract.
const WALK_BUDGET: Duration = Duration::from_millis(6000);

/// What one traversal produced: the nodes it read, plus what it could not read.
struct WalkReport {
    truncated: bool,
    /// How many nodes were actually looked at, so a capped search can say
    /// how far it got rather than only that it stopped.
    visited: usize,
    unreadable: Vec<crate::Unreadable>,
}

/// Did the control simply not implement this, or did nobody answer?
///
/// Only the second belongs in `unreadable`. Most containers have no Component
/// interface and no Name property, and calling those failures would bury the
/// application that is really stuck.
fn unanswered(reason: &str) -> Option<String> {
    // Strip spaces and lowercase: zbus renders a D-Bus failure as the interface
    // name (`org.freedesktop.DBus.Error.UnknownInterface`) next to a human
    // sentence ("No such interface ... on object"), and either half can be the one
    // that says the interface is absent rather than unanswered.
    let flat: String = reason
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let absent = [
        "unknowninterface",
        "unknownproperty",
        "unknownmethod",
        "unknownobject",
        "nosuchinterface",
        "notsupported",
    ]
    .iter()
    .any(|needle| flat.contains(needle));
    (!absent).then(|| shorten(reason))
}

/// Keep the bus's own wording but not its whole banner.
fn shorten(reason: &str) -> String {
    let one_line = reason.lines().next().unwrap_or_default().trim();
    let trimmed = one_line.trim_start_matches("io error: ").trim();
    trimmed.chars().take(160).collect()
}

/// Record a read that went unanswered, if it did.
fn note(problems: &mut Vec<String>, what: &str, reason: &str) {
    if let Some(text) = unanswered(reason) {
        problems.push(format!("{what}: {text}"));
    }
}

fn a11y_connection() -> Result<Connection> {
    let session = Connection::session()
        .map_err(|e| Error::Capture(format!("session bus unavailable: {e}")))?;
    let bus = Proxy::new(&session, A11Y_BUS, A11Y_PATH, A11Y_BUS)
        .map_err(|e| Error::Capture(format!("a11y bus proxy: {e}")))?;
    let address: String = bus
        .call("GetAddress", &())
        .map_err(|e| Error::Capture(format!("GetAddress failed: {e}")))?;
    zbus::blocking::connection::Builder::address(address.as_str())
        // Applies only to the async `Connection::call` path; what actually bounds
        // a wedged peer here is `timed`, so do not read this as the safety net.
        .and_then(|b| b.method_timeout(CALL_TIMEOUT).build())
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

/// The node's fields alone.
///
/// For the window-matching reads, where a failure already surfaces as a refusal
/// that names what the bus did show -- reporting it twice would be noise.
fn read_node_data(conn: &Connection, dest: &str, path: &str) -> NodeData {
    read_node(conn, dest, path).0
}

/// Read one node, within the walk's reply deadline.
///
/// Returns an empty node plus the reason when the peer is deaf, so the caller can
/// report the hole instead of losing it.
fn read_node(conn: &Connection, dest: &str, path: &str) -> (NodeData, Vec<u32>, Vec<String>) {
    let conn = conn.clone();
    let dest_owned = dest.to_string();
    let path_owned = path.to_string();
    match timed(dest, move || read_node_raw(&conn, &dest_owned, &path_owned)) {
        Ok(triple) => triple,
        Err(Stall::NoReply) => (
            (String::new(), 0, (0, 0, 0, 0), Vec::new()),
            Vec::new(),
            vec![format!("no reply within {}ms", CALL_TIMEOUT.as_millis())],
        ),
        Err(Stall::CoolingDown) => (
            (String::new(), 0, (0, 0, 0, 0), Vec::new()),
            Vec::new(),
            vec![format!(
                "not re-attempted: this bus was deaf within the last {}s",
                WEDGED_COOLDOWN.as_secs()
            )],
        ),
    }
}

/// Read one node, keeping a note of any read that went unanswered.
///
/// The node is still returned with whatever did arrive: a control whose name is
/// unknown but whose rectangle answered is useful, and dropping it silently was
/// the old failure. What changed is that the caller now also learns which reads
/// nobody answered.
fn read_node_raw(conn: &Connection, dest: &str, path: &str) -> (NodeData, Vec<u32>, Vec<String>) {
    let mut name = String::new();
    let mut role = 0u32;
    let mut kids = Vec::new();
    let mut states = Vec::new();
    let mut problems = Vec::new();

    match Proxy::new(conn, dest, path, "org.a11y.atspi.Accessible") {
        Ok(proxy) => {
            match proxy.get_property::<String>("Name") {
                Ok(n) => name = n,
                Err(e) => note(&mut problems, "name", &e.to_string()),
            }
            // AT-SPI exposes the role through methods, not a D-Bus property.
            match proxy.call::<_, _, u32>("GetRole", &()) {
                Ok(r) => role = r,
                Err(e) => {
                    // Some toolkits only answer the name form; only if both are
                    // silent is this a read nobody answered.
                    let reason = e.to_string();
                    match proxy.call::<_, _, String>("GetRoleName", &()) {
                        Ok(role_name) => role = role_from_name(&role_name),
                        Err(e2) => note(&mut problems, "role", &format!("{reason}; {e2}")),
                    }
                }
            }
            match proxy.call::<_, _, Kids>("GetChildren", &()) {
                Ok(k) => kids = k,
                Err(e) => note(&mut problems, "children", &e.to_string()),
            }
            if problems.is_empty() {
                states = proxy
                    .call::<_, _, Vec<u32>>("GetState", &())
                    .unwrap_or_default();
            }
        }
        Err(e) => note(&mut problems, "accessible", &e.to_string()),
    }

    let rect = match Proxy::new(conn, dest, path, "org.a11y.atspi.Component") {
        Ok(proxy) => match proxy.call::<_, _, (i32, i32, i32, i32)>("GetExtents", &(0u32,)) {
            Ok((x, y, w, h)) => (x, y, if w > 0 { w } else { 0 }, if h > 0 { h } else { 0 }),
            Err(e) => {
                note(&mut problems, "extents", &e.to_string());
                (0, 0, 0, 0)
            }
        },
        // Not every control is a component, so a proxy failure here is expected.
        Err(_) => (0, 0, 0, 0),
    };

    ((name, role, rect, kids), states, problems)
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
) -> Result<WalkReport> {
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
) -> Result<WalkReport> {
    let mut truncated = false;
    let mut unreadable: Vec<crate::Unreadable> = Vec::new();
    let mut visited = 0usize;
    let deadline = Instant::now() + WALK_BUDGET;
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
        if Instant::now() >= deadline {
            // Named by the first node it refused to attempt: the walk does not
            // know what else would have answered, and saying so is the point.
            truncated = true;
            unreadable.push(crate::Unreadable {
                bus: dest.clone(),
                path: path.clone(),
                reason: format!(
                    "not attempted: the {}ms walk budget ran out after {visited} node(s)",
                    WALK_BUDGET.as_millis()
                ),
            });
            break;
        }
        if depth > max_depth {
            truncated = true;
            continue;
        }

        let ((name, role, rect, kids), words, problems) = read_node(conn, &dest, &path);
        // Recorded whether or not the node survives: a control whose rectangle
        // nobody answered is still a hole in what the caller can trust.
        if !problems.is_empty() {
            unreadable.push(crate::Unreadable {
                bus: dest.clone(),
                path: path.clone(),
                reason: problems.join("; "),
            });
        }
        if name.is_empty() && rect.2 == 0 && kids.is_empty() {
            continue;
        }

        visited += 1;
        let control_type = control_type_name(role).to_string();
        let (checked, selected, focused) = node_states_from_words(&control_type, &words);
        let node = UiNode {
            index: 0,
            depth,
            control_type,
            name,
            automation_id: String::new(),
            rect,
            // AT-SPI object paths are already a stable per-element identity, so
            // they serve directly as the signature the index registry keys on.
            signature: path.clone(),
            value: String::new(),
            actions: Vec::new(),
            focused,
            selected,
            checked,
        };
        if visit(conn, &node, &dest, &path) {
            return Ok(WalkReport {
                truncated,
                visited,
                unreadable,
            });
        }

        if kids.len() + visited > max_nodes {
            truncated = true;
        }
        for (d, p) in kids.into_iter().rev() {
            stack.push((d, p.to_string(), depth + 1));
        }
    }

    Ok(WalkReport {
        truncated,
        visited,
        unreadable,
    })
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
        let (name, _role, _rect, _kids) = read_node_data(&conn, dest, path);
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

    let (_name, _role, _rect, kids) = read_node_data(&conn, &dest, &path);
    let frames: Vec<(String, OwnedObjectPath)> = kids
        .into_iter()
        .filter(|(fd, fp)| {
            let (_n, role, _rect, _) = read_node_data(&conn, fd, fp);
            TOP_LEVEL_ROLES.contains(&role)
        })
        .collect();
    if frames.is_empty() {
        return Err(Error::Capture(no_frame_message(app, title, pid, &observed)));
    }
    let frame_names: Vec<String> = frames
        .iter()
        .map(|(fd, fp)| read_node_data(&conn, fd, fp).0)
        .collect();
    let chosen = resolve_frame(&frame_names, title).map_err(|seen| {
        Error::Capture(format!(
            "several AT-SPI frames belong to `{app}` (pid {pid}) and none matches `{title}`: [{}]; pass a more exact title",
            seen.join(", ")
        ))
    })?;

    let (dest, path) = frames[chosen].clone();
    let mut nodes = Vec::new();
    let report = walk_from(
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
    Ok(UiSnapshot {
        nodes,
        truncated: report.truncated,
        unreadable: report.unreadable,
    })
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
    let report = walk(max_nodes, max_depth, &mut |_, node, _, _| {
        let mut node = node.clone();
        node.index = nodes.len();
        nodes.push(node);
        false
    })?;
    Ok(UiSnapshot {
        nodes,
        truncated: report.truncated,
        unreadable: report.unreadable,
    })
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

/// The walk ran out of tree before it met the control.
///
/// `Ok(None)` means "this control does not expose that pattern", and callers act
/// on it by falling back to coordinates. A search that stopped early is a
/// different fact, and dressing it up as that one is how a capped walk turns into
/// a click on the wrong pixel.
fn capped(name: &str, action: &str, visited: usize, max_nodes: usize, max_depth: usize) -> Error {
    Error::Lookup(format!(
        "did not reach {action} on {name:?}: the walk stopped after {visited} node(s) at depth {max_depth} with the tree still truncated (max_nodes={max_nodes}); raise max_nodes, or fall back to coordinates knowingly -- this is not \"the control lacks the pattern\""
    ))
}

/// Why a name-based action that fired nothing should say what it says.
///
/// Kept apart from the walk so the ordering has a test. Silence outranks a
/// truncated walk because meeting the control and getting no reply is the more
/// specific fact; `None` is left for the one case where `Ok(None)` is honest,
/// namely a walk that finished and found the control without the pattern.
fn action_miss(
    name: &str,
    action: &str,
    silence: Option<&str>,
    walked: &WalkReport,
    max_nodes: usize,
    max_depth: usize,
) -> Option<Error> {
    if let Some(reason) = silence {
        return Some(unanswered_call(action, name, reason));
    }
    if walked.truncated {
        return Some(capped(name, action, walked.visited, max_nodes, max_depth));
    }
    None
}

/// Read one control's state without a second walk, swallowing a failed read.
///
/// A failed readback is not an error: the action it follows already answered. It
/// becomes `unverifiable`, which the caller can act on, instead of an `Err` that
/// would suggest the action itself did not happen.
fn read_state(conn: &Connection, node: &UiNode, dest: &str, path: &str) -> Option<ControlState> {
    state_words(conn, dest, path)
        .ok()
        .map(|words| decode_state(node, &words))
}

/// Fire the control's first AT-SPI action (usually "click"/"press").
///
/// A control nobody answered is an `Err`, not `Ok(None)`: the caller's documented
/// response to `None` is a coordinate click, and dropping to coordinates because a
/// toolkit was silent is exactly the mistake this backend keeps having to unmake.
pub fn invoke(name: &str) -> Result<Option<ActionReport>> {
    invoke_with(
        name,
        crate::DEFAULT_ACTION_NODES,
        crate::DEFAULT_ACTION_DEPTH,
    )
}

/// [`invoke`] with the traversal limits spelled out.
pub fn invoke_with(name: &str, max_nodes: usize, max_depth: usize) -> Result<Option<ActionReport>> {
    let mut hit = None;
    let mut silence: Option<String> = None;
    let report = walk(max_nodes, max_depth, &mut |conn, node, dest, path| {
        if node.name != name || node.rect.2 <= 0 {
            return false;
        }
        // Read the control before firing it. "The pattern answered" and "the
        // control did something" are two facts, and the audit found a panel button
        // that gave the first answer while opening nothing.
        let before = read_state(conn, node, dest, path);
        let dest_owned = dest.to_string();
        let path_owned = path.to_string();
        match attempted(conn, dest, move |conn| {
            proxy_call::<bool, _>(
                &conn,
                &dest_owned,
                &path_owned,
                "org.a11y.atspi.Action",
                "DoAction",
                &(0i32,),
            )
        }) {
            // DoAction answers with its own success boolean, so a reply of
            // `false` is "the toolkit refused", not "the control fired": only the
            // first counts as an invoke.
            Attempt::Answered(true) => {
                let mut after = read_state(conn, node, dest, path);
                let mut effect = crate::judge_effect(before.as_ref(), after.as_ref());
                if effect == Effect::Unchanged {
                    // One bounded re-read: a toolkit that updates on the next paint
                    // looks untouched to a call made immediately after the action.
                    std::thread::sleep(Duration::from_millis(crate::EFFECT_SETTLE_MS));
                    after = read_state(conn, node, dest, path);
                    effect = crate::judge_effect(before.as_ref(), after.as_ref());
                }
                hit = Some(ActionReport {
                    node: node.clone(),
                    effect,
                    state: after,
                });
                return true;
            }
            Attempt::Answered(false) => {}
            Attempt::Unanswered(reason) => {
                silence = Some(reason);
            }
            Attempt::NoPattern(_) => {}
        }
        false
    })?;
    if hit.is_none() {
        if let Some(miss) = action_miss(
            name,
            "invoke",
            silence.as_deref(),
            &report,
            max_nodes,
            max_depth,
        ) {
            return Err(miss);
        }
    }
    Ok(hit)
}

/// Replace text through the EditableText interface. See [`invoke`] for why a
/// silent toolkit is an `Err` here rather than a `None`.
pub fn set_value(name: &str, text: &str) -> Result<Option<ActionReport>> {
    set_value_with(
        name,
        text,
        crate::DEFAULT_ACTION_NODES,
        crate::DEFAULT_ACTION_DEPTH,
    )
}

/// [`set_value`] with the traversal limits spelled out.
///
/// The verdict here is `unverifiable` on this backend, not because the write did
/// not happen but because reading a field's text needs the Text interface, which
/// `control_state` does not query yet (see ROADMAP). `Changed` is only claimed when
/// the control answers with the exact text asked for.
pub fn set_value_with(
    name: &str,
    text: &str,
    max_nodes: usize,
    max_depth: usize,
) -> Result<Option<ActionReport>> {
    let mut hit = None;
    let mut silence: Option<String> = None;
    let report = walk(max_nodes, max_depth, &mut |conn, node, dest, path| {
        if node.name != name || node.rect.2 <= 0 {
            return false;
        }
        let dest_owned = dest.to_string();
        let path_owned = path.to_string();
        let text_owned = text.to_string();
        match attempted(conn, dest, move |conn| {
            // SetTextContents answers with a boolean. Decoding it as nothing
            // made a call that the toolkit *had* applied look like a control
            // without the interface, which is the opposite of what the caller
            // needs to know.
            proxy_call::<bool, _>(
                &conn,
                &dest_owned,
                &path_owned,
                "org.a11y.atspi.EditableText",
                "SetTextContents",
                &(text_owned,),
            )
        }) {
            Attempt::Answered(true) => {
                let state = read_state(conn, node, dest, path);
                hit = Some(ActionReport {
                    node: node.clone(),
                    effect: crate::judge_text_effect(state.as_ref(), text),
                    state,
                });
                return true;
            }
            Attempt::Answered(false) => {}
            Attempt::Unanswered(reason) => {
                silence = Some(reason);
            }
            Attempt::NoPattern(_) => {}
        }
        false
    })?;
    if hit.is_none() {
        if let Some(miss) = action_miss(
            name,
            "set_value",
            silence.as_deref(),
            &report,
            max_nodes,
            max_depth,
        ) {
            return Err(miss);
        }
    }
    Ok(hit)
}

/// `AtspiStateType` ordinals, counted from the enum in at-spi2-core's
/// `atspi-constants.h` (INVALID is 0 and every following name takes the next
/// number). `GetState` answers an array of u32 bitmasks in that order, so the
/// ordinal doubles as the bit index and anything from 32 up lives in word 1.
mod atspi_state {
    pub const CHECKED: u32 = 4;
    pub const COLLAPSED: u32 = 5;
    pub const EXPANDABLE: u32 = 9;
    pub const EXPANDED: u32 = 10;
    pub const FOCUSED: u32 = 12;
    pub const SELECTED: u32 = 23;
    pub const SENSITIVE: u32 = 24;
    pub const SHOWING: u32 = 25;
    pub const VISIBLE: u32 = 30;
    pub const INDETERMINATE: u32 = 32;
    pub const CHECKABLE: u32 = 41;
}

/// Whether a `GetState` reply has the given state ordinal set.
fn has_state(words: &[u32], ordinal: u32) -> bool {
    words
        .get(ordinal as usize / 32)
        .is_some_and(|mask| mask & (1 << (ordinal % 32)) != 0)
}

/// Extract `(checked, selected, focused)` for a tree node from its AT-SPI
/// `GetState` bitset.
fn node_states_from_words(control_type: &str, words: &[u32]) -> (Option<bool>, bool, bool) {
    if words.is_empty() {
        return (None, false, false);
    }
    let checkable = has_state(words, atspi_state::CHECKABLE)
        || has_state(words, atspi_state::CHECKED)
        || matches!(control_type, "CheckBox" | "RadioButton" | "ToggleButton");
    let checked = if checkable && !has_state(words, atspi_state::INDETERMINATE) {
        Some(has_state(words, atspi_state::CHECKED))
    } else {
        None
    };
    let selected = has_state(words, atspi_state::SELECTED);
    let focused = has_state(words, atspi_state::FOCUSED);
    (checked, selected, focused)
}

/// Map an AT-SPI state set onto the fields this backend reports, in the same
/// vocabulary the Windows side uses: `on` / `off` / `indeterminate`, and
/// `expanded` / `collapsed` / `partially_expanded` / `leaf`.
///
/// `toggle` and `expanded` stay absent unless the control says it can be toggled
/// or expanded. Emitting `off` for every button would read as "a toggle that is
/// currently off", which is a claim about a state we never observed.
fn decode_state(node: &UiNode, words: &[u32]) -> crate::ControlState {
    let checkable = has_state(words, atspi_state::CHECKABLE)
        || has_state(words, atspi_state::CHECKED)
        // Gate on the role too: a toolkit is free to expose a check button as
        // checkable-by-role without setting CHECKABLE.
        || matches!(node.control_type.as_str(), "CheckBox" | "RadioButton");
    let toggle = checkable.then(|| {
        if has_state(words, atspi_state::INDETERMINATE) {
            "indeterminate"
        } else if has_state(words, atspi_state::CHECKED) {
            "on"
        } else {
            "off"
        }
        .to_string()
    });

    let expanded = has_state(words, atspi_state::EXPANDABLE).then(|| {
        if has_state(words, atspi_state::EXPANDED) {
            "expanded"
        } else if has_state(words, atspi_state::COLLAPSED) {
            "collapsed"
        } else {
            "leaf"
        }
        .to_string()
    });

    crate::ControlState {
        name: node.name.clone(),
        control_type: node.control_type.clone(),
        // SENSITIVE is "the user can act on this"; a widget that is merely not
        // on screen keeps it, which is why offscreen is a separate field.
        enabled: has_state(words, atspi_state::SENSITIVE),
        offscreen: !(has_state(words, atspi_state::VISIBLE)
            && has_state(words, atspi_state::SHOWING)),
        toggle,
        // The current text needs the Text interface and a slider's number the
        // Value one. Neither is read here, so the fields stay absent rather than
        // being inferred from the name.
        value: None,
        range_value: None,
        expanded,
    }
}

/// `org.a11y.atspi.Accessible.GetState`: no arguments, an array of u32.
///
/// Bounded like every other call: `control_state` on a wedged application used to
/// wait on the peer forever, which is the same silence the tree walk now reports.
fn state_words(conn: &Connection, dest: &str, path: &str) -> Result<Vec<u32>> {
    let dest_owned = dest.to_string();
    let path_owned = path.to_string();
    match attempted(conn, dest, move |owned| {
        proxy_call::<Vec<u32>, _>(
            &owned,
            &dest_owned,
            &path_owned,
            "org.a11y.atspi.Accessible",
            "GetState",
            &(),
        )
    }) {
        Attempt::Answered(words) => Ok(words),
        Attempt::NoPattern(reason) => Err(Error::Capture(format!("GetState {path}: {reason}"))),
        Attempt::Unanswered(reason) => Err(Error::Lookup(format!(
            "did not get state for {path}: {reason}. This is not \"the control has no state\""
        ))),
    }
}

/// Read one control's state by accessible name.
///
/// An off-screen control is still returned: `offscreen: true` is the answer the
/// caller needs, and refusing to look it up would push them into clicking a
/// coordinate for a control we can describe.
pub fn state(name: &str) -> Result<Option<crate::ControlState>> {
    let mut found = None;
    let mut failure: Option<Error> = None;
    walk(600, 10, &mut |conn, node, dest, path| {
        if node.name != name {
            return false;
        }
        match state_words(conn, dest, path) {
            Ok(words) => found = Some(decode_state(node, &words)),
            Err(e) => failure = Some(e),
        }
        true
    })?;
    match failure {
        Some(e) => Err(e),
        None => Ok(found),
    }
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
            checked: None,
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

    /// A state set the way the D-Bus reply carries it: one u32 per 32 ordinals.
    fn words_for(ordinals: &[u32]) -> Vec<u32> {
        let mut out = vec![0u32; 2];
        for ordinal in ordinals {
            out[*ordinal as usize / 32] |= 1 << (ordinal % 32);
        }
        out
    }

    #[test]
    fn the_state_ordinals_are_pinned_to_the_header() {
        // Literal positions counted from `AtspiStateType` in at-spi2-core's
        // atspi-constants.h, where INVALID is 0 and every following name takes
        // the next number. The other tests build their bitmaps *from* these
        // constants, so they cannot catch a constant that drifted -- this test
        // can. What the constants really mean on a live widget is proven by the
        // probe's own `toggled` log, not by anything here.
        assert_eq!(atspi_state::CHECKED, 4);
        assert_eq!(atspi_state::COLLAPSED, 5);
        assert_eq!(atspi_state::EXPANDABLE, 9);
        assert_eq!(atspi_state::EXPANDED, 10);
        assert_eq!(atspi_state::FOCUSED, 12);
        assert_eq!(atspi_state::SELECTED, 23);
        assert_eq!(atspi_state::SENSITIVE, 24);
        assert_eq!(atspi_state::SHOWING, 25);
        assert_eq!(atspi_state::VISIBLE, 30);
        assert_eq!(atspi_state::INDETERMINATE, 32);
        assert_eq!(atspi_state::CHECKABLE, 41);
    }

    #[test]
    fn node_states_extracts_checked_selected_and_focused() {
        assert_eq!(
            node_states_from_words("CheckBox", &[]),
            (None, false, false)
        );
        let checked_group = words_for(&[atspi_state::CHECKABLE, atspi_state::CHECKED]);
        assert_eq!(
            node_states_from_words("Group", &checked_group),
            (Some(true), false, false)
        );
        let unchecked_radio = words_for(&[atspi_state::SENSITIVE]);
        assert_eq!(
            node_states_from_words("RadioButton", &unchecked_radio),
            (Some(false), false, false)
        );
        let selected_item = words_for(&[atspi_state::SELECTED, atspi_state::FOCUSED]);
        assert_eq!(
            node_states_from_words("ListItem", &selected_item),
            (None, true, true)
        );
    }

    #[test]
    fn a_silent_toolkit_is_not_reported_as_a_missing_pattern() {
        // The proof of the action-side fix, and deliberately not a CI check: only
        // here can all three outcomes be forced regardless of which application
        // the walk happens to reach first.
        assert!(matches!(classify(Ok(Ok(true))), Attempt::Answered(true)));

        match classify::<bool>(Err(Stall::NoReply)) {
            Attempt::Unanswered(reason) => assert!(
                reason.contains("no reply within"),
                "the deadline must say it was the deadline: {reason}"
            ),
            other => panic!("a stalled call must be Unanswered, got {other:?}"),
        }
        match classify::<bool>(Err(Stall::CoolingDown)) {
            Attempt::Unanswered(reason) => assert!(reason.contains("was deaf"), "{reason}"),
            other => panic!("a cooled-down bus is still silence, got {other:?}"),
        }

        // "I do not implement that" is a fact about the control.
        match classify::<bool>(Ok(Err(
            "org.freedesktop.DBus.Error.UnknownInterface: nope".into()
        ))) {
            Attempt::NoPattern(reason) => assert!(reason.contains("UnknownInterface"), "{reason}"),
            other => panic!("an absent interface must be NoPattern, got {other:?}"),
        }

        // The bus's own "nobody answered" is not an absent interface. This is the
        // pair that used to collapse into the same branch.
        match classify::<bool>(Ok(Err(
            "org.freedesktop.DBus.Error.NoReply: Did not receive a reply".into(),
        ))) {
            Attempt::Unanswered(_) => {}
            other => panic!("a NoReply is silence, not absence, got {other:?}"),
        }
    }

    #[test]
    fn silence_is_reported_as_silence_to_the_caller() {
        let message =
            unanswered_call("invoke", "probe-toggle", "no reply within 1200ms").to_string();
        assert!(
            message.starts_with("accessibility lookup failed: "),
            "not a capture problem, not a pattern problem: {message}"
        );
        assert!(
            message.contains("did not get an answer for invoke"),
            "{message}"
        );
        assert!(
            message.contains("This is not \"the control has no invoke pattern\""),
            "must name the misreading it prevents: {message}"
        );
    }

    #[test]
    fn a_capped_search_blames_the_cap_not_the_control() {
        // The whole point of item 7: this message is what keeps a caller from
        // reading "no pattern" and dropping to a coordinate click.
        let message = capped("probe-entry", "set_value", 1, 1, 10).to_string();
        assert!(
            message.starts_with("accessibility lookup failed: "),
            "the label must not send the reader to a screenshot problem: {message}"
        );
        assert!(
            message.contains("did not reach set_value on \"probe-entry\""),
            "{message}"
        );
        assert!(message.contains("after 1 node(s)"), "{message}");
        assert!(message.contains("max_nodes=1"), "{message}");
        assert!(
            message.contains("not \"the control lacks the pattern\""),
            "must name the misreading it is preventing: {message}"
        );
        assert!(
            !message.to_lowercase().contains("no such pattern"),
            "the old wording must not survive in the new one"
        );
    }

    fn readable(toggle: Option<&str>, expanded: Option<&str>) -> crate::ControlState {
        crate::ControlState {
            name: "probe-toggle".to_string(),
            control_type: "CheckBox".to_string(),
            enabled: true,
            offscreen: false,
            toggle: toggle.map(str::to_string),
            value: None,
            expanded: expanded.map(str::to_string),
            range_value: None,
        }
    }

    fn walked(truncated: bool, visited: usize) -> WalkReport {
        WalkReport {
            truncated,
            visited,
            unreadable: Vec::new(),
        }
    }

    #[test]
    fn an_answered_action_that_moves_the_control_says_changed() {
        assert_eq!(
            crate::judge_effect(
                Some(&readable(Some("off"), None)),
                Some(&readable(Some("on"), None))
            ),
            Effect::Changed
        );
    }

    #[test]
    fn an_answered_action_that_moves_nothing_says_unchanged() {
        // The audit's shape: the panel button answered DoAction and opened nothing.
        // Reporting that as a plain success is what made the call untrustworthy.
        assert_eq!(
            crate::judge_effect(
                Some(&readable(None, Some("collapsed"))),
                Some(&readable(None, Some("collapsed")))
            ),
            Effect::Unchanged
        );
    }

    #[test]
    fn an_unreadable_readback_is_never_reported_as_unchanged() {
        // The other direction matters just as much: "nothing moved" is a claim that
        // talks the caller out of a retry, so it cannot rest on a read that failed.
        let before = readable(Some("off"), None);
        assert_eq!(
            crate::judge_effect(Some(&before), None),
            Effect::Unverifiable
        );
        assert_eq!(
            crate::judge_effect(None, Some(&before)),
            Effect::Unverifiable
        );
        assert_eq!(crate::judge_effect(None, None), Effect::Unverifiable);
    }

    #[test]
    fn a_plain_button_that_shows_no_state_is_not_reported_unchanged() {
        // The flood guard. A press that works looks exactly like this on the control
        // itself, so calling it "unchanged" would tell the caller the action did
        // nothing every time they invoke a button -- and a verdict that is wrong that
        // often gets ignored, which is how the original bug stayed trusted.
        assert_eq!(
            crate::judge_effect(Some(&readable(None, None)), Some(&readable(None, None))),
            Effect::Unverifiable
        );
    }

    #[test]
    fn what_identifies_a_control_is_not_evidence_of_an_effect() {
        let before = readable(Some("off"), None);
        let mut after = readable(Some("off"), None);
        after.name = "someone renamed me".to_string();
        after.control_type = "Text".to_string();
        assert_eq!(
            crate::judge_effect(Some(&before), Some(&after)),
            Effect::Unchanged,
            "a rename is not a state the action moved"
        );
    }

    #[test]
    fn a_slider_that_answers_twice_with_nan_did_not_move() {
        // NaN != NaN, so an equality check here would report a change every single
        // time for any control whose range read is not a number.
        let mut before = readable(Some("off"), None);
        let mut after = readable(Some("off"), None);
        before.range_value = Some(12.5);
        after.range_value = Some(12.5);
        assert_eq!(
            crate::judge_effect(Some(&before), Some(&after)),
            Effect::Unchanged
        );
        before.range_value = Some(f64::NAN);
        after.range_value = Some(f64::NAN);
        assert_eq!(
            crate::judge_effect(Some(&before), Some(&after)),
            Effect::Unchanged,
            "NaN must not invent a change"
        );
    }

    #[test]
    fn a_text_assignment_is_judged_against_the_text_asked_for() {
        let mut answered = readable(None, None);
        answered.value = Some("hello".to_string());
        assert_eq!(
            crate::judge_text_effect(Some(&answered), "hello"),
            Effect::Changed
        );
        answered.value = Some("hell".to_string());
        assert_eq!(
            crate::judge_text_effect(Some(&answered), "hello"),
            Effect::Unchanged,
            "a field holding other text did not take this one"
        );
        // AT-SPI does not read a field's text yet: no verdict rather than a guess.
        assert_eq!(
            crate::judge_text_effect(Some(&readable(None, None)), "hello"),
            Effect::Unverifiable
        );
        assert_eq!(
            crate::judge_text_effect(None, "hello"),
            Effect::Unverifiable
        );
    }

    #[test]
    fn silence_outranks_a_truncated_walk_in_the_answer() {
        let miss = action_miss(
            "probe-toggle",
            "invoke",
            Some("no reply within 1200ms"),
            &walked(true, 600),
            600,
            10,
        )
        .unwrap()
        .to_string();
        assert!(miss.contains("did not get an answer"), "{miss}");
        assert!(
            !miss.contains("did not reach"),
            "meeting the control is the more specific fact: {miss}"
        );
    }

    #[test]
    fn a_walk_that_ran_out_without_silence_blames_the_cap() {
        let miss = action_miss("probe-toggle", "invoke", None, &walked(true, 1), 1, 10)
            .unwrap()
            .to_string();
        assert!(miss.contains("did not reach"), "{miss}");
    }

    #[test]
    fn a_walk_that_finished_and_found_nothing_is_not_an_error() {
        // The one case where Ok(None) is the honest answer: the control was seen and
        // does not expose the pattern.
        assert!(action_miss("probe-toggle", "invoke", None, &walked(false, 42), 600, 10).is_none());
    }

    #[test]
    fn a_state_ordinal_is_a_bit_index_into_the_reply_words() {
        // Ordinal 4 is word 0 bit 4; ordinal 32 is word 1 bit 0. Getting this
        // wrong would silently report a different state than the one set.
        let words = words_for(&[atspi_state::CHECKED, atspi_state::INDETERMINATE]);
        assert!(has_state(&words, atspi_state::CHECKED));
        assert!(has_state(&words, atspi_state::INDETERMINATE));
        assert!(!has_state(&words, atspi_state::CHECKABLE));
        assert!(
            !has_state(&[], atspi_state::CHECKED),
            "a short reply misses, it does not panic"
        );
    }

    #[test]
    fn a_checked_box_reports_toggle_and_a_button_does_not() {
        let on = words_for(&[
            atspi_state::SENSITIVE,
            atspi_state::VISIBLE,
            atspi_state::SHOWING,
            atspi_state::CHECKABLE,
            atspi_state::CHECKED,
        ]);
        let decoded = decode_state(&node("CheckBox", "probe-toggle"), &on);
        assert_eq!(decoded.toggle.as_deref(), Some("on"));
        assert!(decoded.enabled);
        assert!(!decoded.offscreen);
        assert_eq!(
            decoded.value, None,
            "text is not read yet, so it must stay absent"
        );
        assert_eq!(decoded.range_value, None);

        let off = words_for(&[
            atspi_state::SENSITIVE,
            atspi_state::VISIBLE,
            atspi_state::SHOWING,
            atspi_state::CHECKABLE,
        ]);
        assert_eq!(
            decode_state(&node("CheckBox", "probe-toggle"), &off)
                .toggle
                .as_deref(),
            Some("off")
        );

        // A push button that does not claim to be checkable must not come back
        // with a toggle that happens to be off -- that is a state we never saw.
        let plain = words_for(&[
            atspi_state::SENSITIVE,
            atspi_state::VISIBLE,
            atspi_state::SHOWING,
        ]);
        let button = decode_state(&node("Button", "probe-submit"), &plain);
        assert_eq!(button.toggle, None);
        assert_eq!(button.expanded, None);

        // A button that does report CHECKABLE is believed, role notwithstanding.
        assert_eq!(
            decode_state(&node("Button", "mute"), &off)
                .toggle
                .as_deref(),
            Some("off")
        );
    }

    #[test]
    fn indeterminate_is_read_from_the_second_word() {
        let mixed = words_for(&[
            atspi_state::CHECKABLE,
            atspi_state::CHECKED,
            atspi_state::INDETERMINATE,
        ]);
        assert_eq!(
            decode_state(&node("CheckBox", "tri"), &mixed)
                .toggle
                .as_deref(),
            Some("indeterminate"),
            "a partly-applied state is not the same as checked"
        );
    }

    #[test]
    fn off_screen_is_not_the_same_as_disabled() {
        let sensitive_only = words_for(&[atspi_state::SENSITIVE]);
        let decoded = decode_state(&node("Button", "scrolled away"), &sensitive_only);
        assert!(decoded.enabled, "still actionable");
        assert!(decoded.offscreen, "just not visible right now");
    }

    #[test]
    fn only_an_expandable_control_gets_an_expanded_field() {
        let collapsed = words_for(&[atspi_state::EXPANDABLE, atspi_state::COLLAPSED]);
        assert_eq!(
            decode_state(&node("Button", "more"), &collapsed)
                .expanded
                .as_deref(),
            Some("collapsed")
        );
        let expanded = words_for(&[atspi_state::EXPANDABLE, atspi_state::EXPANDED]);
        assert_eq!(
            decode_state(&node("Button", "more"), &expanded)
                .expanded
                .as_deref(),
            Some("expanded")
        );
        assert_eq!(
            decode_state(&node("Button", "plain"), &words_for(&[])).expanded,
            None
        );
    }

    #[test]
    fn a_read_that_never_returns_is_bounded_and_not_retried() {
        // No bus needed: the point is that a call which never comes back cannot
        // pin the caller, and that the caller does not keep spawning helpers
        // against the same deaf peer.
        let bus = "[:9.999-deadline-test]";
        let started = Instant::now();
        let first = timed(bus, || loop {
            std::thread::sleep(Duration::from_millis(50));
        });
        let waited = started.elapsed();
        assert!(
            matches!(first, Err(Stall::NoReply)),
            "a stuck read must return"
        );
        assert!(
            waited < Duration::from_secs(10),
            "the caller waited {waited:?}, not indefinitely"
        );

        let second_started = Instant::now();
        let second = timed(bus, || 7u32);
        assert!(
            matches!(second, Err(Stall::CoolingDown)),
            "a bus already known deaf is not poked again"
        );
        assert!(
            second_started.elapsed() < Duration::from_millis(200),
            "the cooldown path must not wait a second deadline"
        );

        // And an unrelated bus is still tried, so one wedged application does not
        // shade the whole desktop.
        let other = timed("[:9.998-other-bus]", || 42u32);
        assert!(matches!(other, Ok(42)), "other peers are unaffected");
    }

    #[test]
    fn a_missing_interface_is_not_reported_as_an_unanswered_read() {
        // One sample per marker the classifier recognises, so dropping any single
        // one of them reddens exactly one case. (An earlier draft put two markers
        // in one sentence, and quietly covered for its own removal.)
        for absent in [
            "org.freedesktop.DBus.Error.UnknownInterface: Thread main reply",
            "No such interface 'org.a11y.atspi.Component' on object at path /org/a11y/atspi/accessible/1",
            "org.freedesktop.DBus.Error.UnknownProperty: Property Name not found",
            "org.freedesktop.DBus.Error.NotSupported: this method is not implemented",
            // Verbatim from a real tree (CI run 40118967046): an application root
            // that has no Component interface. The first version of the
            // classifier reported this as a hole, which is precisely the noise
            // that would bury a genuinely wedged application.
            "org.freedesktop.DBus.Error.UnknownMethod: Method \"GetExtents\" with signature \"u\" on interface \"org.a11y.atspi.Component\" does not exist",
        ] {
            assert_eq!(
                unanswered(absent),
                None,
                "not implementing an interface is normal, not a hung app: {absent}"
            );
        }
    }

    #[test]
    fn a_read_that_did_not_get_an_answer_is_reported() {
        for stuck in [
            "org.freedesktop.DBus.Error.NoReply: Did not receive a reply. Possible causes include: the remote application did not send a reply",
            "FfiError: timeout expired",
            "io error: connection closed",
            "Exec timeout(25s)",
        ] {
            let noted = unanswered(stuck);
            assert!(noted.is_some(), "must be reported, not swallowed: {stuck}");
            assert!(
                noted.unwrap().chars().count() <= 160,
                "the reason must not eat the context window"
            );
        }
    }

    #[test]
    fn a_reason_is_kept_to_one_bounded_line() {
        let long = "io error: boom\nsecond line of the banner\nthird";
        assert_eq!(shorten(long), "boom");
        let padded = "x".repeat(400);
        assert_eq!(shorten(&padded).chars().count(), 160);
    }

    #[test]
    fn the_ledger_names_a_few_holes_and_counts_the_rest() {
        use crate::{unreadable_note, Unreadable};
        assert_eq!(unreadable_note(&[]), None, "a complete tree says nothing");

        let hole = |i: usize| Unreadable {
            bus: if i == 0 {
                String::new()
            } else {
                format!(":1.{i}")
            },
            path: format!("/org/a11y/atspi/accessible/{i}"),
            reason: "children: NoReply".into(),
        };
        let many: Vec<Unreadable> = (0..8).map(hole).collect();
        let note = unreadable_note(&many).expect("eight holes must be reported");
        assert!(note.contains("/org/a11y/atspi/accessible/0"), "{note}");
        assert!(note.contains("[?]"), "an unnamed bus shows as ?");
        assert!(
            note.contains("...and 2 more unreadable subtree(s)"),
            "the cap must still tell the caller how many there were: {note}"
        );
        assert!(
            !note.contains("/org/a11y/atspi/accessible/7"),
            "past the cap only the count is shown"
        );
    }
}
