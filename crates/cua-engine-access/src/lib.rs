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

impl UiSnapshot {
    /// Compact prompt-friendly text, one node per line.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
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
        let mut nodes = Vec::new();
        let (truncated, _) = walk(max_nodes, max_depth, |node| {
            nodes.push(node.clone());
            false
        })?;
        Ok(UiSnapshot { nodes, truncated })
    }

    pub fn find(name: &str, _max_nodes: usize, _max_depth: usize) -> Result<Option<UiNode>> {
        use windows::core::BSTR;
        use windows::Win32::System::Variant::VARIANT;
        use windows::Win32::UI::Accessibility::{
            IUIAutomation, TreeScope_Subtree, UIA_ControlTypePropertyId, UIA_NamePropertyId,
        };

        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| Error::Capture(format!("UIA init: {e}")))?;
        let root = unsafe { automation.GetRootElement() }
            .map_err(|e| Error::Capture(format!("UIA root: {e}")))?;
        let name_cond = unsafe {
            automation.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(BSTR::from(name)))
        }
        .map_err(|e| Error::Capture(format!("UIA condition: {e}")))?;

        // Prefer actionable controls: the Start menu and its button share a
        // name, but only the button (control type 50000) is clickable chrome.
        let mut node = None;
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
                node = Some(element_to_node(&el));
                break;
            }
        }

        if node.is_none() {
            if let Ok(el) = unsafe { root.FindFirst(TreeScope_Subtree, &name_cond) } {
                node = Some(element_to_node(&el));
            }
        }
        Ok(node.filter(|n| n.rect.2 > 0 && n.rect.3 > 0))
    }

    fn element_to_node(el: &IUIAutomationElement) -> UiNode {
        UiNode {
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

    /// Depth-first walk in document order. `visit` returns true to stop early.
    fn walk(
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

        let root = unsafe { automation.GetRootElementBuildCache(&cache) }
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
