//! 安全围栏：动作执行前的统一决策点。
//!
//! - DryRun：全部只记录不执行（默认模式）
//! - Supervised：Safe 直接放行，Input/Risky 逐个确认
//! - Autonomous：全部放行（仅应在沙箱 VM 内使用）

use cua_types::{Action, SafetyLevel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    DryRun,
    Supervised,
    Autonomous,
}

/// 确认者：Supervised 模式下由它决定 Input/Risky 动作是否放行。
pub trait Confirmer {
    fn confirm(&mut self, action: &Action) -> bool;
}

/// 无条件放行（Autonomous 模式与自动化测试用）。
pub struct AutoConfirmer;
impl Confirmer for AutoConfirmer {
    fn confirm(&mut self, _action: &Action) -> bool {
        true
    }
}

/// 无条件拒绝（保守测试用）。
pub struct DenyConfirmer;
impl Confirmer for DenyConfirmer {
    fn confirm(&mut self, _action: &Action) -> bool {
        false
    }
}

/// 从终端逐个询问 y/N（CLI 的 Supervised 模式）。
pub struct StdinConfirmer;
impl Confirmer for StdinConfirmer {
    fn confirm(&mut self, action: &Action) -> bool {
        use std::io::Write;
        eprint!("执行动作 {} ? [y/N] ", action.describe());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub mode: RunMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Execute,
    Skip(String),
}

impl Policy {
    pub fn decide<C: Confirmer + ?Sized>(&self, action: &Action, confirmer: &mut C) -> Decision {
        match self.mode {
            RunMode::DryRun => Decision::Skip("dry-run".to_string()),
            RunMode::Autonomous => Decision::Execute,
            RunMode::Supervised => match action.safety() {
                SafetyLevel::Safe => Decision::Execute,
                SafetyLevel::Input | SafetyLevel::Risky => {
                    if confirmer.confirm(action) {
                        Decision::Execute
                    } else {
                        Decision::Skip("denied by confirmer".to_string())
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_skips_everything() {
        let p = Policy {
            mode: RunMode::DryRun,
        };
        let mut c = AutoConfirmer;
        assert!(matches!(
            p.decide(&Action::Screenshot, &mut c),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn supervised_safe_passes_input_needs_confirm() {
        let p = Policy {
            mode: RunMode::Supervised,
        };
        let mut yes = AutoConfirmer;
        let mut no = DenyConfirmer;
        assert_eq!(p.decide(&Action::Screenshot, &mut no), Decision::Execute);
        assert_eq!(
            p.decide(&Action::TypeText { text: "x".into() }, &mut yes),
            Decision::Execute
        );
        assert!(matches!(
            p.decide(&Action::TypeText { text: "x".into() }, &mut no),
            Decision::Skip(_)
        ));
    }
}
