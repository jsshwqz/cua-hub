//! Stable element indexes and accessibility-tree diffing.
//!
//! A fresh tree per observation forces the model to re-read hundreds of lines
//! and to renumber every target after each action. Two things fix that:
//!
//! * **Stable indexes.** Each control's structural signature (its path of
//!   ordinal + control type + automation id) is mapped to an index that
//!   survives as long as the control does, so `[42]` keeps meaning the same
//!   button across observations.
//! * **Diffs.** Once a baseline exists, only the changed lines are returned, in
//!   unified-diff form. Unchanged controls keep their index; a removed index is
//!   retired rather than reused, because a reused number that silently points at
//!   a different control is how automation clicks the wrong thing.
//!
//! When the registry cannot guarantee that (first observation of a window, an
//! explicit reset, or too many controls to track) the caller is told the
//! baseline was rebuilt and that every previously handed-out index is invalid.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use cua_engine_access::{UiNode, UiSnapshot};

/// How the returned tree text relates to what the caller saw before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    /// Complete tree; indexes are valid from this observation only.
    BaselineReset,
    /// Complete tree for a window that had no baseline yet.
    Full,
    /// Unified diff against the previous observation.
    Diff,
    /// Nothing changed; the previous index mapping still applies.
    Unchanged,
}

impl TreeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TreeMode::BaselineReset => "baseline_reset",
            TreeMode::Full => "full",
            TreeMode::Diff => "diff",
            TreeMode::Unchanged => "unchanged",
        }
    }
}

/// What `window_state` should hand back for the accessibility half of an
/// observation.
#[derive(Debug, Clone)]
pub struct TreeView {
    pub mode: TreeMode,
    /// Full tree text, or the unified diff, or empty when nothing changed.
    pub text: String,
    /// Instruction for the caller, present whenever acting on a stale index
    /// would be possible.
    pub notice: Option<String>,
    pub node_count: usize,
    /// Lines added or removed relative to the baseline.
    pub changed_lines: usize,
}

/// Windows tracked at once. Beyond this the least recently used entry is
/// dropped, which costs a baseline reset for that window and nothing else.
const MAX_WINDOWS: usize = 24;
/// Indexes tracked per window. A tree this large is not something a model
/// should be reading line by line anyway.
const MAX_INDEXES: usize = 8192;
/// Above this many diff cells the quadratic DP is not worth it; return the full
/// tree instead.
const MAX_DIFF_CELLS: usize = 16_000_000;

#[derive(Default)]
struct WindowEntry {
    /// signature -> index
    indexes: HashMap<String, u32>,
    /// Signatures seen in the latest observation.
    alive: HashSet<String>,
    /// Signatures seen in the observation before that. Kept so a control that
    /// flickers out for one frame does not come back with a new index.
    previous: HashSet<String>,
    next_index: u32,
    baseline: Option<String>,
    last_used: u64,
}

fn registry() -> &'static Mutex<HashMap<u32, WindowEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u32, WindowEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Key a control is remembered by. The walk supplies a structural signature;
/// anything that arrives without one (AT-SPI nodes on a host where the object
/// path was unavailable) falls back to a positional key so indexing still works.
fn key_for(node: &UiNode) -> String {
    if node.signature.is_empty() {
        format!(
            "/{}:{}:{}:{}",
            node.depth, node.control_type, node.automation_id, node.name
        )
    } else {
        node.signature.clone()
    }
}

/// One window's tree inside a grouped observation.
pub struct TreePart<'a> {
    pub window_id: u32,
    pub title: String,
    /// True for popups owned by the observed window's process.
    pub related: bool,
    pub snapshot: &'a mut UiSnapshot,
}

fn render_part(part: &TreePart<'_>) -> String {
    let mut out = String::new();
    if part.related {
        out.push_str(&format!(
            "# related window {} \"{}\"\n",
            part.window_id, part.title
        ));
    }
    out.push_str(&part.snapshot.to_text());
    out
}

/// Assign stable indexes to one or more trees that belong to the same
/// observation and produce the single text the caller should see.
///
/// The target window comes first; related popups follow under a header line, so
/// a menu opening or closing shows up in the diff like any other change. All
/// parts share one index namespace, which is what makes an `element_index`
/// unambiguous across the whole observation — including indexes that point into
/// a popup.
pub fn observe_group(
    scope_id: u32,
    mut parts: Vec<TreePart<'_>>,
    disable_diffing: bool,
) -> TreeView {
    let node_count: usize = parts.iter().map(|p| p.snapshot.nodes.len()).sum();
    let Ok(mut guard) = registry().lock() else {
        // A poisoned registry must not take the tool down with it: hand back a
        // full tree with sequential indexes.
        let mut text = String::new();
        let mut next = 0usize;
        for part in parts.iter_mut() {
            for node in &mut part.snapshot.nodes {
                node.index = next;
                next += 1;
            }
            text.push_str(&render_part(part));
        }
        return TreeView {
            mode: TreeMode::BaselineReset,
            text,
            notice: Some(RESET_NOTICE.to_string()),
            node_count,
            changed_lines: node_count,
        };
    };

    if disable_diffing {
        guard.remove(&scope_id);
    }
    // Evict the least recently used window when the registry is full.
    if !guard.contains_key(&scope_id) && guard.len() >= MAX_WINDOWS {
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(id, _)| *id)
        {
            guard.remove(&oldest);
        }
    }

    let tick = tick();
    let mut reset = false;
    let entry = guard.entry(scope_id).or_default();
    entry.last_used = tick;

    if entry.indexes.len() > MAX_INDEXES {
        entry.indexes.clear();
        entry.alive.clear();
        entry.previous.clear();
        entry.next_index = 0;
        entry.baseline = None;
        reset = true;
    }

    let mut current: HashSet<String> = HashSet::with_capacity(node_count);
    let mut text = String::new();
    for part in parts.iter_mut() {
        // The window id is part of the key: the same-shaped control in a popup
        // and in the owner window must not share an index.
        for node in &mut part.snapshot.nodes {
            let key = format!("{}|{}", part.window_id, key_for(node));
            let index = match entry.indexes.get(&key) {
                Some(index) => *index as usize,
                None => {
                    let next = entry.next_index;
                    entry.next_index += 1;
                    entry.indexes.insert(key.clone(), next);
                    next as usize
                }
            };
            node.index = index;
            current.insert(key);
        }
        text.push_str(&render_part(part));
    }

    // Retire signatures that have been gone for two observations, so the map
    // cannot grow without bound. A retired index is never handed out again.
    entry.indexes.retain(|key, _| {
        current.contains(key) || entry.alive.contains(key) || entry.previous.contains(key)
    });
    entry.previous = std::mem::take(&mut entry.alive);
    entry.alive = current;

    let baseline = entry.baseline.take();
    entry.baseline = Some(text.clone());

    let (mode, out, notice, changed) = match baseline {
        None if reset || disable_diffing => (
            TreeMode::BaselineReset,
            text.clone(),
            Some(RESET_NOTICE.to_string()),
            count_lines(&text),
        ),
        // No baseline to compare against: the whole tree is new information.
        None => (TreeMode::Full, text.clone(), None, count_lines(&text)),
        Some(previous) => {
            if previous == text {
                (
                    TreeMode::Unchanged,
                    String::new(),
                    Some(
                        "accessibility tree unchanged; the element_index mapping you already have still applies"
                            .to_string(),
                    ),
                    0,
                )
            } else {
                match unified_diff(&previous, &text, 2) {
                    Some(diff) if diff.len() < text.len() => (
                        TreeMode::Diff,
                        diff.clone(),
                        Some(DIFF_NOTICE.to_string()),
                        count_changed(&diff),
                    ),
                    _ => (TreeMode::Full, text.clone(), None, count_lines(&text)),
                }
            }
        }
    };

    TreeView {
        mode,
        text: out,
        notice,
        node_count,
        changed_lines: changed,
    }
}

const RESET_NOTICE: &str = "new accessibility baseline: discard every element_index you hold for this window and pick targets again from this tree, even if an old number reappears";
const DIFF_NOTICE: &str = "unified diff against your previous observation: '-' lines are gone (their element_index is retired and will not be reused), '+' lines are new or changed, ' ' lines are unchanged and keep their element_index; apply removals before additions";

fn tick() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

fn count_lines(text: &str) -> usize {
    text.lines().count()
}

fn count_changed(diff: &str) -> usize {
    diff.lines()
        .filter(|line| (line.starts_with('+') || line.starts_with('-')) && !line.starts_with("@@"))
        .count()
}

/// Unified diff over lines: context lines start with a space, removals with `-`,
/// additions with `+`, and each changed region is introduced by `@@`. Returns
/// `None` when the texts are equal or when the comparison would be too large to
/// be worth it.
pub fn unified_diff(old: &str, new: &str, context: usize) -> Option<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a == b {
        return None;
    }
    if a.len().saturating_mul(b.len()) > MAX_DIFF_CELLS {
        return None;
    }

    // Longest common subsequence lengths; backtrack to get the edit script.
    let cols = b.len() + 1;
    let mut lcs = vec![0u32; (a.len() + 1) * cols];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * cols + j] = if a[i] == b[j] {
                lcs[(i + 1) * cols + j + 1] + 1
            } else {
                lcs[(i + 1) * cols + j].max(lcs[i * cols + j + 1])
            };
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Op {
        Same,
        Delete,
        Insert,
    }
    let mut ops: Vec<(Op, &str)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            ops.push((Op::Same, a[i]));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * cols + j] >= lcs[i * cols + j + 1] {
            ops.push((Op::Delete, a[i]));
            i += 1;
        } else {
            ops.push((Op::Insert, b[j]));
            j += 1;
        }
    }
    while i < a.len() {
        ops.push((Op::Delete, a[i]));
        i += 1;
    }
    while j < b.len() {
        ops.push((Op::Insert, b[j]));
        j += 1;
    }

    // Group edits into hunks: changes closer than twice the context window share
    // one hunk, so the model sees a contiguous region instead of fragments.
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (op, _))| *op != Op::Same)
        .map(|(idx, _)| idx)
        .collect();
    if changed.is_empty() {
        return None;
    }

    let mut out = String::new();
    let mut group_start = 0usize;
    while group_start < changed.len() {
        let mut group_end = group_start;
        while group_end + 1 < changed.len()
            && changed[group_end + 1] <= changed[group_end] + context * 2
        {
            group_end += 1;
        }
        let first = changed[group_start].saturating_sub(context);
        let last = (changed[group_end] + context).min(ops.len() - 1);

        // Line numbers are 1-based counts of the lines each side had before here.
        let mut old_no = 0usize;
        let mut new_no = 0usize;
        for (op, _) in ops.iter().take(first) {
            match op {
                Op::Same => {
                    old_no += 1;
                    new_no += 1;
                }
                Op::Delete => old_no += 1,
                Op::Insert => new_no += 1,
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut body = String::new();
        for (op, line) in ops.iter().take(last + 1).skip(first) {
            match op {
                Op::Same => {
                    body.push(' ');
                    body.push_str(line);
                    body.push('\n');
                    old_count += 1;
                    new_count += 1;
                }
                Op::Delete => {
                    body.push('-');
                    body.push_str(line);
                    body.push('\n');
                    old_count += 1;
                }
                Op::Insert => {
                    body.push('+');
                    body.push_str(line);
                    body.push('\n');
                    new_count += 1;
                }
            }
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_no + 1,
            old_count,
            new_no + 1,
            new_count
        ));
        out.push_str(&body);
        group_start = group_end + 1;
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(signature: &str, name: &str, depth: usize) -> UiNode {
        UiNode {
            index: 0,
            depth,
            control_type: "Button".into(),
            name: name.into(),
            automation_id: String::new(),
            rect: (1, 2, 3, 4),
            signature: signature.into(),
            value: String::new(),
            actions: vec!["Invoke".into()],
            focused: false,
            selected: false,
        }
    }

    fn typed(signature: &str, control_type: &str, name: &str, depth: usize) -> UiNode {
        UiNode {
            control_type: control_type.into(),
            ..node(signature, name, depth)
        }
    }

    fn snapshot(nodes: Vec<UiNode>) -> UiSnapshot {
        UiSnapshot {
            nodes,
            truncated: false,
            unreadable: Vec::new(),
        }
    }

    fn observe(window: u32, snap: &mut UiSnapshot, disable: bool) -> TreeView {
        observe_group(
            window,
            vec![TreePart {
                window_id: window,
                title: String::new(),
                related: false,
                snapshot: snap,
            }],
            disable,
        )
    }

    /// A window big enough that a diff is genuinely smaller than the tree, which
    /// is the condition under which `observe` prefers diffing.
    fn filler(count: usize) -> Vec<UiNode> {
        (0..count)
            .map(|i| {
                typed(
                    &format!("/0:Window/{i}:Pane"),
                    "Pane",
                    &format!("pane {i}"),
                    1,
                )
            })
            .collect()
    }

    /// Each test uses its own window id so the process-wide registry cannot make
    /// one test depend on another's baseline.
    #[test]
    fn first_observation_returns_a_full_tree() {
        let mut snap = snapshot(vec![
            typed("/0:Window", "Window", "Notepad", 0),
            typed("/0:Window/1:Button", "Button", "OK", 1),
        ]);
        let view = observe(9001, &mut snap, false);
        assert_eq!(view.mode, TreeMode::Full);
        assert!(
            view.text.contains("[0] Window \"Notepad\""),
            "{}",
            view.text
        );
        assert!(
            view.text.contains("[1]   Button \"OK\" actions=Invoke"),
            "{}",
            view.text
        );
        assert_eq!(snap.nodes[1].index, 1);
    }

    #[test]
    fn indexes_survive_unrelated_changes() {
        let window = 9002;
        let mut first_nodes = vec![typed("/0:Window", "Window", "Notepad", 0)];
        first_nodes.extend(filler(20));
        first_nodes.push(typed("/0:Window/99:Edit", "Edit", "body", 1));
        let mut first = snapshot(first_nodes);
        observe(window, &mut first, false);
        let edit_index = first.nodes.last().expect("edit node").index;

        // The user types into the body: one line changes, nothing renumbers.
        let mut second_nodes = vec![typed("/0:Window", "Window", "Notepad", 0)];
        second_nodes.extend(filler(20));
        second_nodes.push(typed("/0:Window/99:Edit", "Edit", "body", 1));
        second_nodes.last_mut().expect("edit node").value = "hello".into();
        let mut second = snapshot(second_nodes);
        let view = observe(window, &mut second, false);

        assert_eq!(second.nodes.last().expect("edit node").index, edit_index);
        assert_eq!(view.mode, TreeMode::Diff, "diff was {:?}", view.text);
        assert!(view.text.starts_with("@@"), "{}", view.text);
        assert!(
            view.text.contains('-'),
            "expected a removal in {}",
            view.text
        );
        assert!(
            view.text.contains('+'),
            "expected an addition in {}",
            view.text
        );
        assert!(
            view.text.lines().count() < 22,
            "diff should be far shorter than the tree: {}",
            view.text
        );
        assert_eq!(view.changed_lines, 2);
    }

    #[test]
    fn unchanged_tree_reports_no_diff() {
        let window = 9003;
        let mut first = snapshot(vec![typed("/0:Window", "Window", "Notepad", 0)]);
        observe(window, &mut first, false);
        let mut second = snapshot(vec![typed("/0:Window", "Window", "Notepad", 0)]);
        let view = observe(window, &mut second, false);
        assert_eq!(view.mode, TreeMode::Unchanged);
        assert!(view.text.is_empty());
        assert_eq!(second.nodes[0].index, first.nodes[0].index);
        assert!(view.notice.expect("notice").contains("still applies"));
    }

    #[test]
    fn a_retired_index_is_not_handed_to_a_different_control() {
        let window = 9004;
        let mut first = snapshot(vec![
            typed("/0:Window", "Window", "Notepad", 0),
            typed("/0:Window/1:Button", "Button", "OK", 1),
        ]);
        observe(window, &mut first, false);
        assert_eq!(first.nodes[1].index, 1);

        // The button goes away.
        let mut second = snapshot(vec![typed("/0:Window", "Window", "Notepad", 0)]);
        observe(window, &mut second, false);

        // A different control arrives: it must get a fresh number, never 1.
        let mut third = snapshot(vec![
            typed("/0:Window", "Window", "Notepad", 0),
            typed("/0:Window/1:Edit", "Edit", "search", 1),
        ]);
        observe(window, &mut third, false);
        assert_ne!(third.nodes[1].index, 1);
    }

    #[test]
    fn disable_diffing_rebuilds_the_baseline() {
        let window = 9005;
        let mut first = snapshot(vec![
            typed("/0:Window", "Window", "Notepad", 0),
            typed("/0:Window/1:Button", "Button", "OK", 1),
        ]);
        observe(window, &mut first, false);
        let mut second = snapshot(vec![typed("/0:Window", "Window", "Notepad", 0)]);
        let view = observe(window, &mut second, true);
        assert_eq!(view.mode, TreeMode::BaselineReset);
        assert!(view
            .notice
            .expect("notice")
            .contains("discard every element_index"));
        assert!(view.text.contains("[0] Window"), "{}", view.text);
        // Numbering restarts, so the caller cannot mix old and new indexes.
        assert_eq!(second.nodes[0].index, 0);
    }

    #[test]
    fn diff_marks_changed_region_only() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nX\ne\nf\ng\nh\n";
        let diff = unified_diff(old, new, 1).expect("texts differ");
        assert!(diff.contains("-d"));
        assert!(diff.contains("+X"));
        assert!(!diff.contains("-a"));
        assert!(diff.contains(" c\n"));
        assert_eq!(count_changed(&diff), 2);
    }

    #[test]
    fn identical_texts_have_no_diff() {
        assert!(unified_diff("a\nb\n", "a\nb\n", 2).is_none());
    }

    #[test]
    fn multiple_regions_produce_multiple_hunks() {
        let old: String = (0..40).map(|i| format!("line{i}\n")).collect();
        let new: String = (0..40)
            .map(|i| {
                if i == 2 || i == 30 {
                    format!("changed{i}\n")
                } else {
                    format!("line{i}\n")
                }
            })
            .collect();
        let diff = unified_diff(&old, &new, 2).expect("texts differ");
        assert_eq!(diff.matches("@@ -").count(), 2, "diff was {diff}");
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+changed30"));
        assert!(
            !diff.contains("line15"),
            "far-away context leaked in: {diff}"
        );
    }

    #[test]
    fn grouped_observation_shares_one_index_namespace() {
        let window = 9006;
        let popup = 9007;
        let mut target = snapshot(vec![typed("/0:Window", "Window", "Notepad", 0)]);
        let mut menu = snapshot(vec![
            typed("/0:Menu", "Menu", "格式(O)", 0),
            typed("/0:Menu/1:MenuItem", "MenuItem", "字体(F)...", 1),
        ]);
        let view = observe_group(
            window,
            vec![
                TreePart {
                    window_id: window,
                    title: "Notepad".into(),
                    related: false,
                    snapshot: &mut target,
                },
                TreePart {
                    window_id: popup,
                    title: String::new(),
                    related: true,
                    snapshot: &mut menu,
                },
            ],
            false,
        );
        assert_eq!(view.mode, TreeMode::Full);
        // Target first, then the popup under its header, one shared numbering.
        assert_eq!(target.nodes[0].index, 0);
        assert_eq!(menu.nodes[0].index, 1);
        assert_eq!(menu.nodes[1].index, 2);
        assert!(
            view.text.contains(&format!("# related window {popup}")),
            "{}",
            view.text
        );

        // The popup closing removes its lines; on a tree this small the diff is
        // not shorter than the full text, so a full tree without the popup is
        // the honest answer.
        let mut target2 = snapshot(vec![typed("/0:Window", "Window", "Notepad", 0)]);
        let view2 = observe_group(
            window,
            vec![TreePart {
                window_id: window,
                title: "Notepad".into(),
                related: false,
                snapshot: &mut target2,
            }],
            false,
        );
        assert_ne!(view2.mode, TreeMode::Unchanged);
        assert!(!view2.text.contains("# related window"), "{}", view2.text);
        assert!(!view2.text.contains("MenuItem"), "{}", view2.text);
        assert_eq!(target2.nodes[0].index, 0);
    }

    #[test]
    fn appended_and_removed_lines_diff_correctly() {
        let old = "[0] Window \"A\"\n[1]   Button \"OK\"\n";
        let new = "[0] Window \"A\"\n[1]   Button \"OK\"\n[2]   Button \"Cancel\"\n";
        let diff = unified_diff(old, new, 2).expect("texts differ");
        assert!(diff.contains("+[2]   Button \"Cancel\""));
        assert!(
            !diff.lines().any(|line| line.starts_with('-')),
            "nothing was removed: {diff}"
        );
    }
}
