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
    /// Snapshot-scoped index, stable until the tree changes.
    #[serde(default)]
    pub index: usize,
    pub depth: usize,
    pub control_type: String,
    pub name: String,
    pub automation_id: String,
    pub rect: (i32, i32, i32, i32),
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
pub fn snapshot_window(id: u32, max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
    #[cfg(windows)]
    {
        imp::snapshot_window(id, max_nodes, max_depth)
    }
    #[cfg(not(windows))]
    {
        let _ = (id, max_nodes, max_depth);
        Err(cua_core::Error::InvalidAction(
            "window-scoped accessibility is only implemented on Windows".into(),
        ))
    }
}

/// The control that currently owns keyboard focus, if any. Typing into the
/// wrong surface is the most common automation mistake, so callers are expected
/// to check this before sending text.
pub fn focused_element() -> Result<Option<UiNode>> {
    #[cfg(windows)]
    {
        imp::focused()
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

impl UiSnapshot {
    /// Compact prompt-friendly text, one node per line.
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
    use super::{control_type_name, UiNode, UiSnapshot};
    use cua_core::{Error, Result};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomationElement};

    pub fn snapshot(max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
        walk_snapshot(Root::Desktop, max_nodes, max_depth)
    }

    pub fn snapshot_window(id: u32, max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
        walk_snapshot(Root::Window(id), max_nodes, max_depth)
    }

    fn walk_snapshot(root: Root, max_nodes: usize, max_depth: usize) -> Result<UiSnapshot> {
        let mut nodes = Vec::new();
        let (truncated, _) = walk(root, max_nodes, max_depth, |node| {
            let mut node = node.clone();
            node.index = nodes.len();
            nodes.push(node);
            false
        })?;
        Ok(UiSnapshot { nodes, truncated })
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
        }
    }

    /// What the walk should be rooted at.
    enum Root {
        Desktop,
        Window(u32),
    }

    /// Depth-first walk in document order. `visit` returns true to stop early.
    fn walk(
        root_kind: Root,
        max_nodes: usize,
        max_depth: usize,
        mut visit: impl FnMut(&UiNode) -> bool,
    ) -> Result<(bool, bool)> {
        // RPC_E_CHANGED_MODE is fine: COM is already initialised differently.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        use windows::Win32::UI::Accessibility::{
            IUIAutomation, IUIAutomationCacheRequest, TreeScope_Children,
            UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId,
            UIA_IsOffscreenPropertyId, UIA_NamePropertyId,
        };

        // One cache request batches every property fetch into the single COM
        // call that returns the children, which is dramatically faster than
        // walking with CurrentX() getters (36s -> under 1s in practice).
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
                .and_then(|_| cache.SetTreeScope(TreeScope_Children))
                .map_err(|e| Error::Capture(format!("UIA cache setup: {e}")))?;
        }

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
        let mut stack: Vec<(IUIAutomationElement, usize)> = vec![(root, 0)];

        while let Some((element, depth)) = stack.pop() {
            if visited >= max_nodes {
                truncated = true;
                break;
            }
            if depth > max_depth {
                truncated = true;
                continue;
            }

            let offscreen = unsafe { element.CachedIsOffscreen() }
                .map(|b| b.as_bool())
                .unwrap_or(false);
            if offscreen {
                continue;
            }

            let name = unsafe { element.CachedName() }
                .map(|b| b.to_string())
                .unwrap_or_default();
            let control_type = unsafe { element.CachedControlType() }
                .map(|id| control_type_name(id.0 as u32))
                .unwrap_or("Unknown")
                .to_string();
            let automation_id = unsafe { element.CachedAutomationId() }
                .map(|b| b.to_string())
                .unwrap_or_default();
            let rect = unsafe { element.CachedBoundingRectangle() }
                .map(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top))
                .unwrap_or((0, 0, 0, 0));

            // Skip anonymous chrome to keep the tree readable.
            if !(name.is_empty() && automation_id.is_empty() && rect.2 == 0) {
                visited += 1;
                let node = UiNode {
                    index: 0,
                    depth,
                    control_type,
                    name,
                    automation_id,
                    rect,
                };
                if visit(&node) {
                    return Ok((truncated, true));
                }
            }

            if let Ok(array) = unsafe { element.GetCachedChildren() } {
                let len = unsafe { array.Length() }.unwrap_or(0).max(0) as usize;
                let mut children = Vec::new();
                for i in 0..len {
                    if let Ok(child) = unsafe { array.GetElement(i as i32) } {
                        children.push(child);
                    }
                    if children.len() >= max_nodes {
                        truncated = true;
                        break;
                    }
                }
                for child in children.into_iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }

        Ok((truncated, false))
    }
}
