//! Accessibility-tree snapshots.
//!
//! Phase 2 ships the read side: a compact, text representation of the desktop
//! control tree that can be given to a model as context (hybrid grounding) or
//! printed for debugging. Invoking controls by name comes next.

pub mod atspi;

use cua_core::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiNode {
    /// Index assigned by the caller. `window_state` assigns indexes from a
    /// per-window registry keyed by [`UiNode::signature`], so the same control
    /// keeps the same number across observations until it disappears.
    #[serde(default)]
    pub index: usize,
    pub depth: usize,
    pub control_type: String,
    pub name: String,
    pub automation_id: String,
    pub rect: (i32, i32, i32, i32),
    /// Structural identity: the path of (ordinal, control type, automation id)
    /// from the window root to this node. Names and rectangles are deliberately
    /// excluded so that editing a caption does not renumber the control.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
    /// Current text value, when the control exposes one and it differs from the
    /// name. Lets the model read a field without a second call.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    /// Secondary actions the control actually exposes. Never guess a name that
    /// is not listed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    /// True when this control owns keyboard focus.
    #[serde(default, skip_serializing_if = "is_false")]
    pub focused: bool,
    /// True when this control is currently selected (list items, tabs, rows).
    #[serde(default, skip_serializing_if = "is_false")]
    pub selected: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Build a child's structural signature from its parent's.
///
/// The ordinal keeps same-shaped siblings apart (a list of fifty identical rows
/// still gets fifty distinct signatures) while leaving out anything that changes
/// when the user edits content.
pub fn child_signature(
    parent: &str,
    ordinal: usize,
    control_type: &str,
    automation_id: &str,
) -> String {
    let mut out = String::with_capacity(parent.len() + 24);
    out.push_str(parent);
    out.push('/');
    out.push_str(&ordinal.to_string());
    out.push(':');
    out.push_str(control_type);
    if !automation_id.is_empty() {
        out.push('#');
        out.push_str(automation_id);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSnapshot {
    pub nodes: Vec<UiNode>,
    pub truncated: bool,
}

/// Snapshot of a control's actionable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlState {
    pub name: String,
    pub control_type: String,
    pub enabled: bool,
    pub offscreen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range_value: Option<f64>,
}

/// Snapshot the control tree rooted at one window, so indexes are scoped to the
/// window the caller is about to act on rather than the whole desktop.
///
/// On Windows the window id is the HWND. AT-SPI has no window id, so the Linux
/// backend matches by process id first — the same `pid` the window list reports —
/// and falls back to the app and title the window list reported.
pub fn snapshot_window(
    id: u32,
    app: &str,
    title: &str,
    pid: u32,
    max_nodes: usize,
    max_depth: usize,
) -> Result<UiSnapshot> {
    #[cfg(windows)]
    {
        let _ = (app, title, pid);
        imp::snapshot_window(id, max_nodes, max_depth)
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        atspi::snapshot_window(app, title, pid, max_nodes, max_depth)
    }
}

/// The control that currently owns keyboard focus, if any. Typing into the
/// wrong surface is the most common automation mistake, so callers are expected
/// to check this before sending text.
///
/// **Windows only.** Other platforms answer `Err(Unsupported)`, not `Ok(None)`:
/// `None` reads as "nothing has focus", which is exactly the false clean bill a
/// pre-flight check is supposed to avoid. Callers that treat it as advisory
/// should say so in whatever they report, the way `window_state` does.
pub fn focused_element() -> Result<Option<UiNode>> {
    #[cfg(windows)]
    {
        imp::focused()
    }
    #[cfg(not(windows))]
    {
        Err(cua_core::Error::Unsupported(
            "keyboard focus is read from UIA; the AT-SPI backend does not query it yet".into(),
        ))
    }
}

/// What happened when a secondary action was performed. `state` is read back
/// from the control afterwards so the caller can verify instead of assuming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecondaryOutcome {
    pub action: String,
    pub element: UiNode,
    /// Post-action state where the pattern exposes one: `expanded`,
    /// `collapsed`, `on`, `off`, `selected`, `normal`, `focused`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The actions this element really supports, returned on success too so a
    /// caller that guessed wrong can correct itself from the same response.
    pub supported: Vec<String>,
}

/// Error payload for an unsupported secondary action: the caller gets the real
/// list instead of a bare failure.
#[derive(Debug, Clone)]
pub struct UnsupportedAction {
    pub requested: String,
    pub supported: Vec<String>,
    pub element: UiNode,
}

impl std::fmt::Display for UnsupportedAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.supported.is_empty() {
            write!(
                f,
                "`{}` is not available on this {} and it exposes no secondary actions",
                self.requested, self.element.control_type
            )
        } else {
            write!(
                f,
                "`{}` is not available on this {}; supported: {}",
                self.requested,
                self.element.control_type,
                self.supported.join(", ")
            )
        }
    }
}

/// Text a window's focused document exposes: what the user has selected and the
/// document body. Both are capped — they exist to save a second observation, not
/// to ship an entire file through a tool result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TextContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_text: Option<String>,
}

/// Read the selection and document text of the control that owns focus inside
/// one window. Windows only; other platforms report nothing rather than failing.
pub fn text_context(window_id: u32) -> Result<TextContext> {
    #[cfg(windows)]
    {
        imp::text_context(window_id)
    }
    #[cfg(not(windows))]
    {
        let _ = window_id;
        Ok(TextContext::default())
    }
}

/// Perform a secondary accessibility action on the control identified by its
/// structural signature inside one window: expand a disclosure, raise a window,
/// scroll a pane, toggle a checkbox, select a list item, focus or invoke.
///
/// Signatures come from [`snapshot_window`]; indexes are not used here because
/// they are assigned by the caller and mean nothing to this crate.
pub fn secondary_action(window_id: u32, signature: &str, action: &str) -> Result<SecondaryOutcome> {
    #[cfg(windows)]
    {
        imp::secondary_action(window_id, signature, action)
    }
    #[cfg(not(windows))]
    {
        let _ = (window_id, signature, action);
        Err(cua_core::Error::InvalidAction(
            "perform_secondary_action is only implemented on Windows".into(),
        ))
    }
}

impl UiSnapshot {
    /// Compact prompt-friendly text, one node per line: an index the input tools
    /// accept, the hierarchy as indentation, then whatever state matters for
    /// choosing a target. Rectangles are physical screen pixels.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            out.push_str(&format!("[{}] ", n.index));
            out.push_str(&"  ".repeat(n.depth));
            out.push_str(&n.control_type);
            if !n.name.is_empty() {
                out.push_str(&format!(" \"{}\"", truncate(&n.name, 80)));
            }
            if !n.automation_id.is_empty() {
                out.push_str(&format!(" #{}", n.automation_id));
            }
            if !n.value.is_empty() && n.value != n.name {
                out.push_str(&format!(" value=\"{}\"", truncate(&n.value, 60)));
            }
            if n.focused {
                out.push_str(" FOCUSED");
            }
            if n.selected {
                out.push_str(" SELECTED");
            }
            if !n.actions.is_empty() {
                out.push_str(&format!(" actions={}", n.actions.join(",")));
            }
            let (x, y, w, h) = n.rect;
            if w > 0 && h > 0 {
                out.push_str(&format!(" ({x},{y},{w},{h})"));
            }
            out.push('\n');
        }
        if self.truncated {
            out.push_str("... (truncated)\n");
        }
        out
    }

    /// Tree-formatted lines for the controls that are currently selected, so a
    /// caller can see a list's or tab strip's selection without scanning the
    /// whole tree.
    pub fn selected_lines(&self) -> Vec<String> {
        let selected: Vec<UiNode> = self.nodes.iter().filter(|n| n.selected).cloned().collect();
        if selected.is_empty() {
            return Vec::new();
        }
        UiSnapshot {
            nodes: selected,
            truncated: false,
        }
        .to_text()
        .lines()
        .map(str::to_string)
        .collect()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Snapshot the desktop control tree, bounded by node count and depth.
///
/// Depth 2 is cheap enough for per-step model context; deeper walks can take
/// seconds on busy desktops.
pub fn snapshot(max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
    #[cfg(windows)]
    {
        imp::snapshot(max_nodes, max_depth)
    }
    #[cfg(not(windows))]
    {
        atspi::snapshot(max_nodes, max_depth)
    }
}

/// Find the first control whose name matches `name` (exact match wins over
/// substring), returning its rectangle so the caller can click its center.
pub fn find(name: &str, max_nodes: usize, max_depth: usize) -> Result<Option<UiNode>> {
    #[cfg(windows)]
    {
        imp::find(name, max_nodes, max_depth)
    }
    #[cfg(not(windows))]
    {
        atspi::find(name, max_nodes, max_depth)
    }
}

/// Invoke a control through its accessibility Invoke pattern. Returns `None`
/// when the control has no invoke pattern (caller should fall back to a click).
pub fn invoke(name: &str) -> Result<Option<UiNode>> {
    #[cfg(windows)]
    {
        imp::invoke(name)
    }
    #[cfg(not(windows))]
    {
        atspi::invoke(name)
    }
}

/// Set a control's text through its Value/EditableText pattern. Returns `None`
/// when unsupported (caller can fall back to focus + typing).
pub fn set_value(name: &str, text: &str) -> Result<Option<UiNode>> {
    #[cfg(windows)]
    {
        imp::set_value(name, text)
    }
    #[cfg(not(windows))]
    {
        atspi::set_value(name, text)
    }
}

/// Read common state fields from a control.
///
/// `Ok(None)` means no control of that name was found. Both platforms read
/// enabled / offscreen / toggle / expanded; on AT-SPI `value` and `range_value`
/// are still `None`, because the text lives behind the Text interface and a
/// slider's number behind the Value one, neither of which is queried here.
pub fn state(name: &str) -> Result<Option<ControlState>> {
    #[cfg(windows)]
    {
        imp::state(name)
    }
    #[cfg(not(windows))]
    {
        atspi::state(name)
    }
}

/// Resolve a snapshot-scoped index to a node by taking a fresh snapshot.
pub fn node_at(index: usize, max_nodes: usize, max_depth: usize) -> Result<Option<UiNode>> {
    Ok(snapshot(max_nodes, max_depth)?
        .nodes
        .into_iter()
        .find(|node| node.index == index))
}

/// Bring a top-level window to the foreground. Uses the foreground thread
/// attachment trick because Windows blocks SetForegroundWindow from a
/// background process otherwise.
#[cfg(windows)]
pub fn focus_window(id: u32) -> Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
        SetForegroundWindow, ShowWindow, SwitchToThisWindow, SW_RESTORE,
    };

    if id == 0 {
        return Err(cua_core::Error::Capture("invalid window id 0".into()));
    }
    unsafe {
        let hwnd = HWND(id as *mut _);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if GetForegroundWindow() == hwnd {
            return Ok(());
        }
        // Windows only lets the foreground process call SetForegroundWindow.
        // Borrow the foreground thread's input queue for the call, then let go
        // immediately; attaching to the foreground (not the target) thread
        // avoids the deadlock seen when the target sits in a modal loop.
        let foreground = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        let current = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != current
            && AttachThreadInput(current, foreground_thread, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(current, foreground_thread, false);
        }
        if wait_for_foreground(hwnd, 300) {
            return Ok(());
        }
        // Last resort for windows that refuse plain activation.
        SwitchToThisWindow(hwnd, true);
        if wait_for_foreground(hwnd, 500) {
            return Ok(());
        }
        return Err(cua_core::Error::Capture(format!(
            "Windows refused to focus window {id}"
        )));
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// Activation is asynchronous: the OS may report the new foreground window a
/// few frames after SetForegroundWindow returns. Poll instead of trusting the
/// instant, because input sent to a window that never came forward lands
/// somewhere else.
#[cfg(windows)]
fn wait_for_foreground(hwnd: windows::Win32::Foundation::HWND, timeout_ms: u64) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if unsafe { GetForegroundWindow() } == hwnd {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

#[cfg(not(windows))]
pub fn focus_window(_id: u32) -> Result<()> {
    Err(cua_core::Error::InvalidAction(
        "focus_window is only implemented on Windows".into(),
    ))
}

/// Map a control-type id to a short readable name. UIA uses ids in the 50000
/// range; AT-SPI roles are small integers (see `AtspiRole`).
fn control_type_name(id: u32) -> &'static str {
    match id {
        // AT-SPI roles
        0 => "Invalid",
        4 => "Alert",
        7 => "CheckBox",
        11 => "ComboBox",
        16 => "Dialog",
        23 => "Frame",
        24 => "GlassPane",
        29 => "Label",
        31 => "List",
        32 => "ListItem",
        33 => "Menu",
        34 => "MenuBar",
        35 => "MenuItem",
        37 => "PageTab",
        39 => "Panel",
        42 => "PopupMenu",
        43 => "Button",
        44 => "ProgressBar",
        45 => "RadioButton",
        49 => "ScrollPane",
        50 => "Separator",
        51 => "Slider",
        54 => "StatusBar",
        55 => "Table",
        56 => "TableCell",
        61 => "Text",
        62 => "ToggleButton",
        63 => "ToolBar",
        65 => "ToolTip",
        66 => "Tree",
        67 => "TreeTable",
        69 => "Window",
        75 => "Application",
        79 => "Entry",
        80 => "Paragraph",
        81 => "Ruler",
        // UIA control types
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        _ => "Unknown",
    }
}

#[cfg(windows)]
mod imp {
    use super::{child_signature, control_type_name, UiNode, UiSnapshot};
    use cua_core::{Error, Result};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomationElement, IUIAutomationValuePattern, UIA_ValuePatternId,
        UIA_PROPERTY_ID,
    };

    pub fn snapshot(max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
        walk_snapshot(Root::Desktop, max_nodes, max_depth)
    }

    pub fn snapshot_window(id: u32, max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
        walk_snapshot(Root::Window(id), max_nodes, max_depth)
    }

    fn walk_snapshot(root: Root, max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
        let mut nodes = Vec::new();
        let (truncated, _) = walk(root, max_nodes, max_depth, |node, _element| {
            let mut node = node.clone();
            node.index = nodes.len();
            nodes.push(node);
            false
        })?;
        Ok(UiSnapshot { nodes, truncated })
    }

    /// Limits for signature lookup: an action target may sit deeper in the tree
    /// than a context snapshot is allowed to go, and giving up early would look
    /// like "the control vanished".
    const FIND_MAX_NODES: usize = 4_000;
    const FIND_MAX_DEPTH: usize = 32;

    /// Re-walk one window and return the live element whose structural
    /// signature matches. Acting through a fresh element matters: the cached
    /// element from an older snapshot may point at a control that has since been
    /// destroyed, and UIA would then act on nothing at all.
    pub fn element_for_signature(
        window_id: u32,
        signature: &str,
    ) -> Result<Option<(IUIAutomationElement, UiNode)>> {
        let mut found = None;
        walk(
            Root::Window(window_id),
            FIND_MAX_NODES,
            FIND_MAX_DEPTH,
            |node, element| {
                if node.signature == signature {
                    found = Some((element.clone(), node.clone()));
                    return true;
                }
                false
            },
        )?;
        Ok(found)
    }

    pub fn secondary_action(
        window_id: u32,
        signature: &str,
        action: &str,
    ) -> Result<super::SecondaryOutcome> {
        use windows::Win32::UI::Accessibility::{
            ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
            IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
            IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern,
            IUIAutomationTogglePattern, IUIAutomationWindowPattern, ScrollAmount_NoAmount,
            ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement, ToggleState_Off,
            ToggleState_On, UIA_ExpandCollapsePatternId, UIA_InvokePatternId, UIA_ScrollPatternId,
            UIA_SelectionItemPatternId, UIA_TogglePatternId, UIA_WindowPatternId,
            WindowVisualState_Normal,
        };

        let Some((element, node)) = element_for_signature(window_id, signature)? else {
            return Err(Error::InvalidAction(format!(
                "no control matching signature `{signature}` in window {window_id}; re-run window_state"
            )));
        };
        let supported = node.actions.clone();
        let requested = action.trim().to_string();
        let key = normalize_action(&requested);
        if !supported.iter().any(|a| normalize_action(a) == key) {
            return Err(Error::InvalidAction(
                super::UnsupportedAction {
                    requested,
                    supported,
                    element: node,
                }
                .to_string(),
            ));
        }

        let state: Option<String> = match key.as_str() {
            "invoke" => {
                let pattern: IUIAutomationInvokePattern =
                    unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
                        .map_err(|e| Error::Capture(format!("UIA Invoke pattern: {e}")))?;
                unsafe { pattern.Invoke() }
                    .map_err(|e| Error::Capture(format!("UIA Invoke failed: {e}")))?;
                None
            }
            "expand" | "collapse" => {
                let pattern: IUIAutomationExpandCollapsePattern =
                    unsafe { element.GetCurrentPatternAs(UIA_ExpandCollapsePatternId) }
                        .map_err(|e| Error::Capture(format!("UIA ExpandCollapse pattern: {e}")))?;
                if key == "expand" {
                    unsafe { pattern.Expand() }
                        .map_err(|e| Error::Capture(format!("UIA Expand failed: {e}")))?;
                } else {
                    unsafe { pattern.Collapse() }
                        .map_err(|e| Error::Capture(format!("UIA Collapse failed: {e}")))?;
                }
                unsafe { pattern.CurrentExpandCollapseState() }
                    .ok()
                    .map(|s| {
                        if s == ExpandCollapseState_Expanded {
                            "expanded".to_string()
                        } else if s == ExpandCollapseState_Collapsed {
                            "collapsed".to_string()
                        } else {
                            "partially_expanded".to_string()
                        }
                    })
            }
            "raise" => {
                let pattern: IUIAutomationWindowPattern =
                    unsafe { element.GetCurrentPatternAs(UIA_WindowPatternId) }
                        .map_err(|e| Error::Capture(format!("UIA Window pattern: {e}")))?;
                unsafe { pattern.SetWindowVisualState(WindowVisualState_Normal) }
                    .map_err(|e| Error::Capture(format!("UIA Raise failed: {e}")))?;
                Some("normal".to_string())
            }
            "scrollup" | "scrolldown" | "scrollleft" | "scrollright" => {
                let pattern: IUIAutomationScrollPattern =
                    unsafe { element.GetCurrentPatternAs(UIA_ScrollPatternId) }
                        .map_err(|e| Error::Capture(format!("UIA Scroll pattern: {e}")))?;
                let (horizontal, vertical) = match key.as_str() {
                    "scrollup" => (ScrollAmount_NoAmount, ScrollAmount_SmallDecrement),
                    "scrolldown" => (ScrollAmount_NoAmount, ScrollAmount_SmallIncrement),
                    "scrollleft" => (ScrollAmount_SmallDecrement, ScrollAmount_NoAmount),
                    _ => (ScrollAmount_SmallIncrement, ScrollAmount_NoAmount),
                };
                unsafe { pattern.Scroll(horizontal, vertical) }
                    .map_err(|e| Error::Capture(format!("UIA Scroll failed: {e}")))?;
                let h = unsafe { pattern.CurrentHorizontalScrollPercent() }.unwrap_or(-1.0);
                let v = unsafe { pattern.CurrentVerticalScrollPercent() }.unwrap_or(-1.0);
                Some(format!("scroll {h:.1}%/{v:.1}%"))
            }
            "toggle" => {
                let pattern: IUIAutomationTogglePattern =
                    unsafe { element.GetCurrentPatternAs(UIA_TogglePatternId) }
                        .map_err(|e| Error::Capture(format!("UIA Toggle pattern: {e}")))?;
                unsafe { pattern.Toggle() }
                    .map_err(|e| Error::Capture(format!("UIA Toggle failed: {e}")))?;
                unsafe { pattern.CurrentToggleState() }.ok().map(|s| {
                    if s == ToggleState_On {
                        "on".to_string()
                    } else if s == ToggleState_Off {
                        "off".to_string()
                    } else {
                        "indeterminate".to_string()
                    }
                })
            }
            "select" => {
                let pattern: IUIAutomationSelectionItemPattern =
                    unsafe { element.GetCurrentPatternAs(UIA_SelectionItemPatternId) }
                        .map_err(|e| Error::Capture(format!("UIA SelectionItem pattern: {e}")))?;
                unsafe { pattern.Select() }
                    .map_err(|e| Error::Capture(format!("UIA Select failed: {e}")))?;
                unsafe { pattern.CurrentIsSelected() }
                    .ok()
                    .map(|b| {
                        if b.as_bool() {
                            "selected"
                        } else {
                            "not_selected"
                        }
                    })
                    .map(str::to_string)
            }
            "focus" => {
                unsafe { element.SetFocus() }
                    .map_err(|e| Error::Capture(format!("UIA SetFocus failed: {e}")))?;
                Some("focused".to_string())
            }
            other => {
                return Err(Error::InvalidAction(format!(
                    "`{other}` is not a supported secondary action"
                )))
            }
        };

        Ok(super::SecondaryOutcome {
            action: requested,
            element: node,
            state,
            supported,
        })
    }

    /// Action names arrive with arbitrary spacing and case ("Scroll Up",
    /// "scroll_up", "SCROLLUP"). Compare on a single normalized form.
    fn normalize_action(action: &str) -> String {
        action
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect()
    }

    /// The element that owns keyboard focus right now.
    pub fn focused() -> Result<Option<UiNode>> {
        use windows::Win32::UI::Accessibility::IUIAutomation;

        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| Error::Capture(format!("UIA init: {e}")))?;
        Ok(unsafe { automation.GetFocusedElement() }
            .ok()
            .map(|el| element_to_node(&el)))
    }

    /// Selection and document text via the Text pattern.
    ///
    /// The focused element is tried first, then the window root, then a handful
    /// of text-bearing controls from the tree: providers disagree about which
    /// element owns the pattern (Chromium answers on the root, document editors
    /// usually only on the nested Document control), so stopping after two probes
    /// would report "no text" for most apps.
    pub fn text_context(window_id: u32) -> Result<super::TextContext> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Accessibility::IUIAutomation;

        const SELECTION_MAX: i32 = 4_000;
        const DOCUMENT_MAX: i32 = 8_000;

        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| Error::Capture(format!("UIA init: {e}")))?;

        let mut candidates: Vec<IUIAutomationElement> = Vec::new();
        candidates.extend(unsafe { automation.GetFocusedElement() }.ok());
        candidates.extend(unsafe { automation.ElementFromHandle(HWND(window_id as *mut _)) }.ok());

        if let Some(context) = read_text(&candidates, SELECTION_MAX, DOCUMENT_MAX) {
            return Ok(context);
        }

        // Bounded, and only reached when the cheap probes missed: walking the
        // tree again costs a second UIA round trip per level.
        candidates = text_candidates(window_id)?;
        Ok(read_text(&candidates, SELECTION_MAX, DOCUMENT_MAX).unwrap_or_default())
    }

    /// The first element among `candidates` that answers the Text pattern with
    /// something worth returning.
    fn read_text(
        candidates: &[IUIAutomationElement],
        selection_max: i32,
        document_max: i32,
    ) -> Option<super::TextContext> {
        use windows::Win32::UI::Accessibility::{IUIAutomationTextPattern, UIA_TextPatternId};

        for element in candidates {
            let Ok(pattern) = (unsafe {
                element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            }) else {
                continue;
            };
            let selected_text = first_selection_text(&pattern, selection_max);
            let document_text = unsafe { pattern.DocumentRange() }
                .ok()
                .and_then(|range| unsafe { range.GetText(document_max) }.ok())
                .map(|b| super::truncate(&b.to_string(), document_max as usize));
            if selected_text.is_none() && document_text.is_none() {
                continue;
            }
            return Some(super::TextContext {
                selected_text,
                document_text,
            });
        }
        None
    }

    /// Text-bearing controls worth probing for the Text pattern, in document
    /// order, capped at a few per window.
    fn text_candidates(window_id: u32) -> Result<Vec<IUIAutomationElement>> {
        let mut found = Vec::new();
        walk(Root::Window(window_id), 400, 16, |node, element| {
            if matches!(
                node.control_type.as_str(),
                "Document" | "Edit" | "Text" | "ComboBox" | "List" | "Table"
            ) {
                found.push(element.clone());
                if found.len() >= 6 {
                    return true;
                }
            }
            false
        })?;
        Ok(found)
    }

    /// The first range of the current selection, as text.
    fn first_selection_text(
        pattern: &windows::Win32::UI::Accessibility::IUIAutomationTextPattern,
        max: i32,
    ) -> Option<String> {
        let array = unsafe { pattern.GetSelection() }.ok()?;
        if unsafe { array.Length() }.unwrap_or(0) <= 0 {
            return None;
        }
        let range = unsafe { array.GetElement(0) }.ok()?;
        let text = unsafe { range.GetText(max) }.ok()?.to_string();
        if text.is_empty() {
            None
        } else {
            Some(super::truncate(&text, max as usize))
        }
    }

    pub fn find(name: &str, _max_nodes: usize, _max_depth: usize) -> Result<Option<UiNode>> {
        Ok(find_element(name)?
            .map(|el| element_to_node(&el))
            .filter(|n| n.rect.2 > 0 && n.rect.3 > 0))
    }

    pub fn invoke(name: &str) -> Result<Option<UiNode>> {
        use windows::Win32::UI::Accessibility::{IUIAutomationInvokePattern, UIA_InvokePatternId};

        let Some(element) = find_element(name)? else {
            return Ok(None);
        };
        let node = element_to_node(&element);
        let pattern: windows::core::Result<IUIAutomationInvokePattern> =
            unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) };
        match pattern {
            Ok(p) => {
                unsafe { p.Invoke() }
                    .map_err(|e| Error::Capture(format!("UIA Invoke failed: {e}")))?;
                Ok(Some(node))
            }
            Err(_) => Ok(None),
        }
    }

    pub fn set_value(name: &str, text: &str) -> Result<Option<UiNode>> {
        use windows::core::BSTR;
        use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_ValuePatternId};

        let Some(element) = find_element(name)? else {
            return Ok(None);
        };
        let node = element_to_node(&element);
        let pattern: windows::core::Result<IUIAutomationValuePattern> =
            unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) };
        match pattern {
            Ok(p) => {
                unsafe { p.SetValue(&BSTR::from(text)) }
                    .map_err(|e| Error::Capture(format!("UIA SetValue failed: {e}")))?;
                Ok(Some(node))
            }
            Err(_) => Ok(None),
        }
    }

    pub fn state(name: &str) -> Result<Option<super::ControlState>> {
        use windows::Win32::UI::Accessibility::{
            ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
            ExpandCollapseState_PartiallyExpanded, IUIAutomationExpandCollapsePattern,
            IUIAutomationRangeValuePattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
            ToggleState_Indeterminate, ToggleState_Off, ToggleState_On,
            UIA_ExpandCollapsePatternId, UIA_RangeValuePatternId, UIA_TogglePatternId,
            UIA_ValuePatternId,
        };

        let Some(element) = find_element(name)? else {
            return Ok(None);
        };
        let node = element_to_node(&element);

        let toggle = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        }
        .ok()
        .and_then(|p| unsafe { p.CurrentToggleState().ok() })
        .map(|s| {
            if s == ToggleState_On {
                "on"
            } else if s == ToggleState_Off {
                "off"
            } else if s == ToggleState_Indeterminate {
                "indeterminate"
            } else {
                "unknown"
            }
        })
        .map(str::to_string);

        let value =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
                .ok()
                .and_then(|p| unsafe { p.CurrentValue().ok() })
                .map(|b| b.to_string());

        let expanded = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                UIA_ExpandCollapsePatternId,
            )
        }
        .ok()
        .and_then(|p| unsafe { p.CurrentExpandCollapseState().ok() })
        .map(|s| {
            if s == ExpandCollapseState_Expanded {
                "expanded"
            } else if s == ExpandCollapseState_Collapsed {
                "collapsed"
            } else if s == ExpandCollapseState_PartiallyExpanded {
                "partially_expanded"
            } else {
                "leaf"
            }
        })
        .map(str::to_string);

        let range_value = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
        }
        .ok()
        .and_then(|p| unsafe { p.CurrentValue().ok() });

        Ok(Some(super::ControlState {
            name: node.name,
            control_type: node.control_type,
            enabled: unsafe { element.CurrentIsEnabled() }
                .map(|b| b.as_bool())
                .unwrap_or(false),
            offscreen: unsafe { element.CurrentIsOffscreen() }
                .map(|b| b.as_bool())
                .unwrap_or(false),
            toggle,
            value,
            expanded,
            range_value,
        }))
    }

    fn find_element(name: &str) -> Result<Option<IUIAutomationElement>> {
        use windows::core::BSTR;
        use windows::Win32::System::Variant::VARIANT;
        use windows::Win32::UI::Accessibility::{
            IUIAutomation, TreeScope_Subtree, UIA_AutomationIdPropertyId,
            UIA_ControlTypePropertyId, UIA_NamePropertyId,
        };

        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| Error::Capture(format!("UIA init: {e}")))?;
        let root = unsafe { automation.GetRootElement() }
            .map_err(|e| Error::Capture(format!("UIA root: {e}")))?;

        // `id:<AutomationId>` targets controls whose Name is dynamic or empty
        // (e.g. Win32 edit id 15, Calculator's num1Button).
        if let Some(id) = name.strip_prefix("id:") {
            let condition = unsafe {
                automation.CreatePropertyCondition(
                    UIA_AutomationIdPropertyId,
                    &VARIANT::from(BSTR::from(id)),
                )
            }
            .map_err(|e| Error::Capture(format!("UIA condition: {e}")))?;
            if let Ok(el) = unsafe { root.FindFirst(TreeScope_Subtree, &condition) } {
                return Ok(Some(el));
            }
            return Ok(None);
        }

        let name_cond = unsafe {
            automation.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(BSTR::from(name)))
        }
        .map_err(|e| Error::Capture(format!("UIA condition: {e}")))?;

        // Prefer actionable controls: the Start menu and its button share a
        // name, but only the button (control type 50000) is clickable chrome.
        for type_id in [50000i32, 50011, 50007] {
            let type_cond = unsafe {
                automation
                    .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(type_id))
            };
            let Ok(type_cond) = type_cond else { continue };
            let Ok(combined) = (unsafe { automation.CreateAndCondition(&name_cond, &type_cond) })
            else {
                continue;
            };
            if let Ok(el) = unsafe { root.FindFirst(TreeScope_Subtree, &combined) } {
                return Ok(Some(el));
            }
        }

        if let Ok(el) = unsafe { root.FindFirst(TreeScope_Subtree, &name_cond) } {
            return Ok(Some(el));
        }
        Ok(None)
    }

    fn element_to_node(el: &IUIAutomationElement) -> UiNode {
        use windows::Win32::UI::Accessibility::{
            IUIAutomationSelectionItemPattern, UIA_SelectionItemPatternId,
        };

        UiNode {
            index: 0,
            depth: 0,
            control_type: unsafe { el.CurrentControlType() }
                .map(|id| control_type_name(id.0 as u32))
                .unwrap_or("Unknown")
                .to_string(),
            name: unsafe { el.CurrentName() }
                .map(|b| b.to_string())
                .unwrap_or_default(),
            automation_id: unsafe { el.CurrentAutomationId() }
                .map(|b| b.to_string())
                .unwrap_or_default(),
            rect: unsafe { el.CurrentBoundingRectangle() }
                .map(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top))
                .unwrap_or((0, 0, 0, 0)),
            signature: String::new(),
            value: unsafe {
                el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            }
            .ok()
            .and_then(|p| unsafe { p.CurrentValue().ok() })
            .map(|b| super::truncate(&b.to_string(), 160))
            .unwrap_or_default(),
            actions: current_actions(el),
            focused: unsafe { el.CurrentHasKeyboardFocus() }
                .map(|b| b.as_bool())
                .unwrap_or(false),
            selected: unsafe {
                el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
            }
            .ok()
            .and_then(|p| unsafe { p.CurrentIsSelected().ok() })
            .map(|b| b.as_bool())
            .unwrap_or(false),
        }
    }

    /// Which secondary actions an element exposes, read live. Only used on the
    /// single-element paths (`find`, `invoke`, `focused`), where a handful of
    /// pattern probes is free; the tree walk uses cached availability instead.
    /// Text a field currently holds.
    ///
    /// Read live rather than from the cache: providers such as Win11 Notepad's
    /// edit control answer the cached Value pattern with an empty string, which
    /// made a typed value invisible to the tree and the diff. Live reads are
    /// restricted to the control types where a value is short and meaningful, so
    /// a document-sized text body is never pulled into a snapshot.
    fn node_value(el: &IUIAutomationElement, control_type: &str) -> String {
        use windows::Win32::UI::Accessibility::{
            IUIAutomationValuePattern, UIA_IsValuePatternAvailablePropertyId,
        };

        if !matches!(control_type, "Edit" | "ComboBox" | "Spinner") {
            return String::new();
        }
        if !cached_flag(el, UIA_IsValuePatternAvailablePropertyId) {
            return String::new();
        }
        let Ok(pattern) =
            (unsafe { el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) })
        else {
            return String::new();
        };
        let value = unsafe { pattern.CurrentValue() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        super::truncate(&value, 160)
    }

    /// Read a cached boolean property. UIA hands these back as a VARIANT, so go
    /// through the OLE conversion helper instead of poking at the union.
    fn cached_flag(el: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> bool {
        use windows::Win32::System::Variant::{VariantClear, VariantToBooleanWithDefault};
        let Ok(mut variant) = (unsafe { el.GetCachedPropertyValue(property) }) else {
            return false;
        };
        let value = unsafe { VariantToBooleanWithDefault(&variant, false) }.as_bool();
        let _ = unsafe { VariantClear(&mut variant) };
        value
    }

    /// Read a cached integer property the same way.
    ///
    /// Pattern *state* is read through properties rather than
    /// `GetCachedPatternAs`: on real providers (Explorer's list items, Notepad's
    /// edit) the cached pattern interface comes back empty even when the state
    /// property is populated, which silently turned every `selected` and
    /// expanded-state read into "false".
    fn cached_int(el: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> Option<i32> {
        use windows::Win32::System::Variant::{VariantClear, VariantToInt32};
        let Ok(mut variant) = (unsafe { el.GetCachedPropertyValue(property) }) else {
            return None;
        };
        let value = unsafe { VariantToInt32(&variant) }.ok();
        let _ = unsafe { VariantClear(&mut variant) };
        value
    }

    /// Secondary actions from cached pattern-availability flags. Probing patterns
    /// live costs one cross-process COM call each, which turns a 200-node walk
    /// into seconds; the availability flags arrive with the children.
    fn cached_actions(el: &IUIAutomationElement) -> Vec<String> {
        use windows::Win32::UI::Accessibility::{
            UIA_ExpandCollapseExpandCollapseStatePropertyId,
            UIA_IsExpandCollapsePatternAvailablePropertyId, UIA_IsInvokePatternAvailablePropertyId,
            UIA_IsScrollPatternAvailablePropertyId, UIA_IsSelectionItemPatternAvailablePropertyId,
            UIA_IsTogglePatternAvailablePropertyId, UIA_IsWindowPatternAvailablePropertyId,
            UIA_ScrollHorizontallyScrollablePropertyId, UIA_ScrollVerticallyScrollablePropertyId,
        };

        let mut actions = Vec::new();
        if cached_flag(el, UIA_IsInvokePatternAvailablePropertyId) {
            actions.push("Invoke".to_string());
        }
        if cached_flag(el, UIA_IsExpandCollapsePatternAvailablePropertyId) {
            // ExpandCollapseState: 0 collapsed, 1 expanded, 2 partially
            // expanded, 3 leaf node. Only the direction that is actually
            // available is advertised, so the caller never asks to collapse
            // something that is already closed.
            match cached_int(el, UIA_ExpandCollapseExpandCollapseStatePropertyId) {
                Some(0) => actions.push("Expand".to_string()),
                Some(1) => actions.push("Collapse".to_string()),
                _ => {
                    actions.push("Expand".to_string());
                    actions.push("Collapse".to_string());
                }
            }
        }
        if cached_flag(el, UIA_IsScrollPatternAvailablePropertyId) {
            if cached_flag(el, UIA_ScrollVerticallyScrollablePropertyId) {
                actions.push("Scroll Up".to_string());
                actions.push("Scroll Down".to_string());
            }
            if cached_flag(el, UIA_ScrollHorizontallyScrollablePropertyId) {
                actions.push("Scroll Left".to_string());
                actions.push("Scroll Right".to_string());
            }
        }
        if cached_flag(el, UIA_IsTogglePatternAvailablePropertyId) {
            actions.push("Toggle".to_string());
        }
        if cached_flag(el, UIA_IsSelectionItemPatternAvailablePropertyId) {
            actions.push("Select".to_string());
        }
        if cached_flag(el, UIA_IsWindowPatternAvailablePropertyId) {
            actions.push("Raise".to_string());
        }
        if unsafe { el.CachedIsKeyboardFocusable() }
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            actions.push("Focus".to_string());
        }
        actions
    }

    /// Which secondary actions an element exposes, read live. Only used on the
    /// single-element paths (`find`, `invoke`, `focused`), where a handful of
    /// pattern probes is free; the tree walk uses cached availability instead.
    fn current_actions(el: &IUIAutomationElement) -> Vec<String> {
        use windows::Win32::UI::Accessibility::{
            ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
            IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
            IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern,
            IUIAutomationTogglePattern, IUIAutomationWindowPattern, UIA_ExpandCollapsePatternId,
            UIA_InvokePatternId, UIA_ScrollPatternId, UIA_SelectionItemPatternId,
            UIA_TogglePatternId, UIA_WindowPatternId,
        };

        let mut actions = Vec::new();
        if unsafe { el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) }
            .is_ok()
        {
            actions.push("Invoke".to_string());
        }
        if let Ok(pattern) = unsafe {
            el.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                UIA_ExpandCollapsePatternId,
            )
        } {
            match unsafe { pattern.CurrentExpandCollapseState() } {
                Ok(s) if s == ExpandCollapseState_Expanded => actions.push("Collapse".to_string()),
                Ok(s) if s == ExpandCollapseState_Collapsed => actions.push("Expand".to_string()),
                _ => {
                    actions.push("Expand".to_string());
                    actions.push("Collapse".to_string());
                }
            }
        }
        if let Ok(pattern) =
            unsafe { el.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId) }
        {
            if unsafe { pattern.CurrentVerticallyScrollable() }
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                actions.push("Scroll Up".to_string());
                actions.push("Scroll Down".to_string());
            }
            if unsafe { pattern.CurrentHorizontallyScrollable() }
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                actions.push("Scroll Left".to_string());
                actions.push("Scroll Right".to_string());
            }
        }
        if unsafe { el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) }
            .is_ok()
        {
            actions.push("Toggle".to_string());
        }
        if unsafe {
            el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
        }
        .is_ok()
        {
            actions.push("Select".to_string());
        }
        if unsafe { el.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId) }
            .is_ok()
        {
            actions.push("Raise".to_string());
        }
        if unsafe { el.CurrentIsKeyboardFocusable() }
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            actions.push("Focus".to_string());
        }
        actions
    }

    /// Everything the walk needs to know about one element, gathered either from
    /// the cache (descendants) or live (the root).
    struct Facts {
        name: String,
        control_type: String,
        automation_id: String,
        rect: (i32, i32, i32, i32),
        value: String,
        actions: Vec<String>,
        focused: bool,
        selected: bool,
    }

    /// Whether a control is part of the current selection, from the cache.
    fn cached_selected(el: &IUIAutomationElement) -> bool {
        use windows::Win32::UI::Accessibility::UIA_SelectionItemIsSelectedPropertyId;
        cached_flag(el, UIA_SelectionItemIsSelectedPropertyId)
    }

    fn cached_facts(el: &IUIAutomationElement) -> Facts {
        let control_type = unsafe { el.CachedControlType() }
            .map(|id| control_type_name(id.0 as u32))
            .unwrap_or("Unknown")
            .to_string();
        Facts {
            name: unsafe { el.CachedName() }
                .map(|b| b.to_string())
                .unwrap_or_default(),
            control_type: control_type.clone(),
            automation_id: unsafe { el.CachedAutomationId() }
                .map(|b| b.to_string())
                .unwrap_or_default(),
            rect: unsafe { el.CachedBoundingRectangle() }
                .map(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top))
                .unwrap_or((0, 0, 0, 0)),
            value: node_value(el, &control_type),
            actions: cached_actions(el),
            focused: unsafe { el.CachedHasKeyboardFocus() }
                .map(|b| b.as_bool())
                .unwrap_or(false),
            selected: cached_selected(el),
        }
    }

    fn live_facts(el: &IUIAutomationElement) -> Facts {
        let node = element_to_node(el);
        Facts {
            name: node.name,
            control_type: node.control_type,
            automation_id: node.automation_id,
            rect: node.rect,
            value: node.value,
            actions: node.actions,
            focused: node.focused,
            selected: node.selected,
        }
    }

    /// What the walk should be rooted at.
    enum Root {
        Desktop,
        Window(u32),
    }

    /// Depth-first walk in document order. `visit` returns true to stop early
    /// and receives the live element so callers can act on the exact node they
    /// matched instead of re-finding it by name.
    fn walk(
        root_kind: Root,
        max_nodes: usize,
        max_depth: usize,
        mut visit: impl FnMut(&UiNode, &IUIAutomationElement) -> bool,
    ) -> Result<(bool, bool)> {
        // RPC_E_CHANGED_MODE is fine: COM is already initialised differently.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        use windows::Win32::UI::Accessibility::{
            IUIAutomation, IUIAutomationCacheRequest, TreeScope_Children, TreeScope_Element,
            UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId,
            UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_HasKeyboardFocusPropertyId,
            UIA_IsEnabledPropertyId, UIA_IsExpandCollapsePatternAvailablePropertyId,
            UIA_IsInvokePatternAvailablePropertyId, UIA_IsKeyboardFocusablePropertyId,
            UIA_IsOffscreenPropertyId, UIA_IsScrollPatternAvailablePropertyId,
            UIA_IsSelectionItemPatternAvailablePropertyId, UIA_IsTogglePatternAvailablePropertyId,
            UIA_IsValuePatternAvailablePropertyId, UIA_IsWindowPatternAvailablePropertyId,
            UIA_NamePropertyId, UIA_ScrollHorizontallyScrollablePropertyId,
            UIA_ScrollVerticallyScrollablePropertyId, UIA_SelectionItemIsSelectedPropertyId,
        };

        // One cache request batches every property fetch into the COM call that
        // returns the children, which is dramatically faster than walking with
        // CurrentX() getters (36s -> under 1s in practice).
        //
        // TreeScope_Element is deliberate: with TreeScope_Children only the root's
        // immediate children arrive with cached properties, and `GetCachedChildren`
        // on those children returns an empty array, which silently flattens every
        // tree to one level (menus looked empty, Explorer looked like four panes).
        // Walking with FindAllBuildCache per node caches each level as it is
        // fetched, so depth is real and memory stays bounded by max_nodes.
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| Error::Capture(format!("UIA init: {e}")))?;
        let cache: IUIAutomationCacheRequest = unsafe { automation.CreateCacheRequest() }
            .map_err(|e| Error::Capture(format!("UIA cache request: {e}")))?;
        unsafe {
            cache
                .AddProperty(UIA_NamePropertyId)
                .and_then(|_| cache.AddProperty(UIA_ControlTypePropertyId))
                .and_then(|_| cache.AddProperty(UIA_AutomationIdPropertyId))
                .and_then(|_| cache.AddProperty(UIA_BoundingRectanglePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsOffscreenPropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsEnabledPropertyId))
                .and_then(|_| cache.AddProperty(UIA_HasKeyboardFocusPropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsKeyboardFocusablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsInvokePatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsExpandCollapsePatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsScrollPatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsTogglePatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsSelectionItemPatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsWindowPatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_IsValuePatternAvailablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_ExpandCollapseExpandCollapseStatePropertyId))
                .and_then(|_| cache.AddProperty(UIA_ScrollVerticallyScrollablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_ScrollHorizontallyScrollablePropertyId))
                .and_then(|_| cache.AddProperty(UIA_SelectionItemIsSelectedPropertyId))
                .and_then(|_| cache.SetTreeScope(TreeScope_Element))
                .map_err(|e| Error::Capture(format!("UIA cache setup: {e}")))?;
        }
        let anywhere = unsafe { automation.CreateTrueCondition() }
            .map_err(|e| Error::Capture(format!("UIA condition: {e}")))?;

        let root = match root_kind {
            Root::Desktop => unsafe { automation.GetRootElementBuildCache(&cache) },
            Root::Window(id) => unsafe {
                automation.ElementFromHandleBuildCache(
                    windows::Win32::Foundation::HWND(id as *mut _),
                    &cache,
                )
            },
        }
        .map_err(|e| Error::Capture(format!("UIA root: {e}")))?;

        let mut visited = 0usize;
        let mut truncated = false;
        let mut stack: Vec<(IUIAutomationElement, usize, String, usize)> =
            vec![(root, 0, String::new(), 0)];

        while let Some((element, depth, parent_signature, ordinal)) = stack.pop() {
            if visited >= max_nodes {
                truncated = true;
                break;
            }
            if depth > max_depth {
                truncated = true;
                continue;
            }

            // The element a BuildCache walk is rooted at comes back with empty
            // cached properties — TreeScope_Children populates descendants only —
            // so read the root live. Hosted apps answer with a hollow placeholder
            // either way and are filtered out below.
            let facts = if depth == 0 {
                live_facts(&element)
            } else {
                let offscreen = unsafe { element.CachedIsOffscreen() }
                    .map(|b| b.as_bool())
                    .unwrap_or(false);
                if offscreen {
                    continue;
                }
                cached_facts(&element)
            };
            let signature = child_signature(
                &parent_signature,
                ordinal,
                &facts.control_type,
                &facts.automation_id,
            );

            // Skip anonymous chrome to keep the tree readable. A hollow root is
            // skipped too: hosted apps (WinUI inside ApplicationFrameHost) answer
            // with a placeholder element that has no name or rectangle, and its
            // window title and bounds already come from the window list.
            if !(facts.name.is_empty() && facts.automation_id.is_empty() && facts.rect.2 == 0) {
                visited += 1;
                let node = UiNode {
                    index: 0,
                    depth,
                    control_type: facts.control_type,
                    name: facts.name,
                    automation_id: facts.automation_id,
                    rect: facts.rect,
                    signature: signature.clone(),
                    value: facts.value,
                    actions: facts.actions,
                    focused: facts.focused,
                    selected: facts.selected,
                };
                if visit(&node, &element) {
                    return Ok((truncated, true));
                }
            }

            if let Ok(array) =
                unsafe { element.FindAllBuildCache(TreeScope_Children, &anywhere, &cache) }
            {
                let len = unsafe { array.Length() }.unwrap_or(0).max(0) as usize;
                let mut children = Vec::new();
                for i in 0..len {
                    if let Ok(child) = unsafe { array.GetElement(i as i32) } {
                        children.push((child, i));
                    }
                    if children.len() >= max_nodes {
                        truncated = true;
                        break;
                    }
                }
                for (child, child_ordinal) in children.into_iter().rev() {
                    stack.push((child, depth + 1, signature.clone(), child_ordinal));
                }
            }
        }

        Ok((truncated, false))
    }
}
