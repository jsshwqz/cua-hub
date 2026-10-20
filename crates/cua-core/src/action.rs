use serde::{Deserialize, Serialize};

/// Absolute screen coordinate in physical pixels (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

/// Interpreter used for [`Action::RunCommand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellKind {
    /// PowerShell on Windows, `/bin/sh` elsewhere.
    #[default]
    Auto,
    PowerShell,
    Bash,
    Cmd,
}

/// An engine-agnostic operation on the machine.
///
/// Engines return these; the runner executes them through `cua-io`.
/// Keeping actions declarative makes dry-runs, logging and replay trivial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Capture a fresh screenshot. Normally implicit between steps.
    Screenshot,
    /// Click at a point.
    Click {
        at: Point,
        #[serde(default)]
        button: MouseButton,
        #[serde(default)]
        double: bool,
    },
    /// Move the pointer without clicking.
    Move { to: Point },
    /// Press, move, release.
    Drag { from: Point, to: Point },
    /// Type a string.
    Type {
        text: String,
        #[serde(default)]
        interval_ms: u64,
    },
    /// Press a key or chord, e.g. `"Return"`, `"ctrl+c"`, `"alt+Tab"`.
    Key { combo: String },
    /// Scroll at a point. Positive `dy` scrolls down.
    Scroll {
        at: Point,
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
    },
    /// Pause for `ms` milliseconds.
    Wait { ms: u64 },
    /// Run a shell command and feed its output back to the engine.
    RunCommand {
        command: String,
        #[serde(default)]
        shell: ShellKind,
    },
    /// Task completed successfully.
    Done { summary: String },
    /// Task cannot continue.
    Fail { reason: String },
}

impl Action {
    /// True for actions that change the machine state.
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Action::Screenshot | Action::Move { .. } | Action::Wait { .. } | Action::Done { .. }
        )
    }

    /// Coarse risk label used by the runner's confirmation gate.
    pub fn risk(&self) -> Risk {
        match self {
            Action::Screenshot | Action::Move { .. } | Action::Wait { .. } => Risk::Safe,
            Action::Done { .. } | Action::Fail { .. } => Risk::None,
            Action::Click { .. } | Action::Scroll { .. } => Risk::Low,
            Action::RunCommand { command, .. } => {
                const BLOCKED: &[&str] = &[
                    "rm -rf /",
                    "rm -rf ~",
                    "mkfs",
                    "format ",
                    "diskpart",
                    "del /f /s",
                    "del /s /q c:",
                    "remove-item -recurse c:\\",
                    "clear-disk",
                    "initialize-disk",
                    "shutdown",
                    "reboot",
                    "stop-computer",
                    "drop database",
                    "bcdedit",
                    "reg delete hklm",
                ];
                let c = command.to_ascii_lowercase();
                if BLOCKED.iter().any(|b| c.contains(b)) {
                    Risk::High
                } else {
                    Risk::Medium
                }
            }
            Action::Type { text, .. } => {
                let t = text.to_ascii_lowercase();
                const DANGEROUS: &[&str] = &[
                    "rm -rf",
                    "format ",
                    "del /f",
                    "drop table",
                    "shutdown",
                    "mkfs",
                    "diskpart",
                    "remove-item -recurse",
                ];
                if DANGEROUS.iter().any(|d| t.contains(d)) {
                    Risk::High
                } else {
                    Risk::Low
                }
            }
            Action::Key { combo } => {
                let c = combo.to_ascii_lowercase();
                if c.contains("delete") || c.contains("f8") {
                    Risk::Medium
                } else {
                    Risk::Low
                }
            }
            Action::Drag { .. } => Risk::Medium,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    None,
    Safe,
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(&self) -> &'static str {
        match self {
            Risk::None => "none",
            Risk::Safe => "safe",
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}
