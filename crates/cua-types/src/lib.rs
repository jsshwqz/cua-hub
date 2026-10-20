//! 统一动作协议：任何"大脑"（模型/provider）只能输出这组动作，
//! 任何"执行层"只认这组动作。换大脑不换手，换手不换大脑。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 一个原子动作。大脑输出 `Vec<Action>` 表示一步计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// 截屏（结果通过 Observation 返回）
    Screenshot,
    /// 移动鼠标到绝对坐标
    MouseMove { x: i32, y: i32 },
    /// 点击；at 为空表示在当前鼠标位置点击
    Click {
        button: MouseButton,
        at: Option<(i32, i32)>,
    },
    /// 双击
    DoubleClick { at: (i32, i32) },
    /// 滚动（正数向下/向右，与 enigo 语义一致：dy 正数向下滚）
    Scroll { dx: i32, dy: i32 },
    /// 拖拽
    Drag { from: (i32, i32), to: (i32, i32) },
    /// 输入一段文本（走剪贴板/键盘注入由执行层决定）
    TypeText { text: String },
    /// 按键或组合键，如 "Return"、"ctrl+l"、"alt+f4"
    KeyPress { combo: String },
    /// 鼠标按钮单独按下/抬起（配合移动/点击做组合操作，如按住拖选）
    MouseButtonState { button: MouseButton, pressed: bool },
    /// 键盘按键单独按下/抬起（如先按住 Shift 再点选多个文件）
    KeyState { combo: String, pressed: bool },
    /// 把文本写入剪贴板
    SetClipboard { text: String },
    /// 读取剪贴板文本（只读）
    GetClipboard,
    /// 查询主屏尺寸与当前鼠标位置（只读）
    ScreenInfo,
    /// 等待
    Wait { ms: u64 },
    /// 启动程序（高风险：可拉起任意进程）
    LaunchApp { program: String, args: Vec<String> },
}

/// 安全等级：安全围栏按等级决定是否需要确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyLevel {
    /// 无副作用
    Safe,
    /// 改变外部状态（点击/输入）
    Input,
    /// 高风险（拉起任意程序等）
    Risky,
}

impl Action {
    pub fn safety(&self) -> SafetyLevel {
        match self {
            Action::Screenshot
            | Action::GetClipboard
            | Action::ScreenInfo
            | Action::MouseMove { .. }
            | Action::Wait { .. } => SafetyLevel::Safe,
            Action::LaunchApp { .. } => SafetyLevel::Risky,
            _ => SafetyLevel::Input,
        }
    }

    /// 简短的人类可读描述，用于审计日志与确认提示。
    pub fn describe(&self) -> String {
        match self {
            Action::Screenshot => "screenshot".to_string(),
            Action::MouseMove { x, y } => format!("mouse_move({x},{y})"),
            Action::Click { button, at } => format!("click({button:?} at {at:?})"),
            Action::DoubleClick { at } => format!("double_click(at {at:?})"),
            Action::Scroll { dx, dy } => format!("scroll({dx},{dy})"),
            Action::Drag { from, to } => format!("drag({from:?} -> {to:?})"),
            Action::TypeText { text } => {
                let preview: String = text.chars().take(40).collect();
                format!(
                    "type_text({preview:?}{})",
                    if text.len() > 40 { "…" } else { "" }
                )
            }
            Action::KeyPress { combo } => format!("key_press({combo})"),
            Action::MouseButtonState { button, pressed } => {
                format!(
                    "mouse_{}({button:?})",
                    if *pressed { "down" } else { "up" }
                )
            }
            Action::KeyState { combo, pressed } => {
                format!(
                    "key_{}({combo})",
                    if *pressed { "down" } else { "up" }
                )
            }
            // 剪贴板内容不进日志，只记长度
            Action::SetClipboard { text } => {
                format!("set_clipboard({} chars)", text.chars().count())
            }
            Action::GetClipboard => "clipboard_get".to_string(),
            Action::ScreenInfo => "screen_info".to_string(),
            Action::Wait { ms } => format!("wait({ms}ms)"),
            Action::LaunchApp { program, args } => {
                format!("launch_app({program} {})", args.join(" "))
            }
        }
    }
}

/// 一次观测：执行层回给大脑的"现在屏幕长什么样"。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub width: u32,
    pub height: u32,
    /// 截图落盘路径（大脑/日志用它取图）
    pub screenshot_path: Option<PathBuf>,
    /// 毫秒级 Unix 时间戳
    pub taken_at_ms: u128,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_json_roundtrip() {
        let a = Action::Click {
            button: MouseButton::Left,
            at: Some((10, 20)),
        };
        let s = serde_json::to_string(&a).unwrap();
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn safety_levels() {
        assert_eq!(Action::Screenshot.safety(), SafetyLevel::Safe);
        assert_eq!(
            Action::TypeText { text: "x".into() }.safety(),
            SafetyLevel::Input
        );
        assert_eq!(
            Action::LaunchApp {
                program: "xterm".into(),
                args: vec![]
            }
            .safety(),
            SafetyLevel::Risky
        );
    }
}
