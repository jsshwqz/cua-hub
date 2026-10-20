//! Accessibility-tree snapshots.
//!
//! Phase 2 ships the read side: a compact, text representation of the desktop
//! control tree that can be given to a model as context (hybrid grounding) or
//! printed for debugging. Invoking controls by name comes next.

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
    imp::snapshot(max_nodes, max_depth)
}

/// Map a UIA control-type id to a short readable name.
#[allow(dead_code)]
fn control_type_name(id: u32) -> &'static str {
    match id {
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

        let mut nodes = Vec::new();
        let mut truncated = false;
        let mut stack: Vec<(IUIAutomationElement, usize)> = vec![(root, 0)];

        while let Some((element, depth)) = stack.pop() {
            if nodes.len() >= max_nodes {
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
            if !offscreen {
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
                    nodes.push(UiNode {
                        depth,
                        control_type,
                        name,
                        automation_id,
                        rect,
                    });
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
        }

        Ok(UiSnapshot { nodes, truncated })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::UiSnapshot;
    use cua_core::{Error, Result};

    pub fn snapshot(_max_nodes: usize, _max_depth: usize) -> Result<UiSnapshot> {
        Err(Error::Capture(
            "accessibility snapshot is only implemented on Windows so far".into(),
        ))
    }
}
