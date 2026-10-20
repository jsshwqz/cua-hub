//! 执行核心：把 cua-types 的动作真正落到屏幕上。
//!
//! - 截屏：x11rb 直连 X11 取根窗口图像（Linux/X11，Phase 1 范围）
//! - 键鼠：enigo（X11 后端；Windows/macOS 后端为现成实现，后续接入）
//! - 安全：每个动作先过 guard::Policy，再执行，全程写 audit 审计日志

pub mod audit;
pub mod capture;
pub mod guard;
pub mod input;
pub mod runtime;

pub use guard::{AutoConfirmer, Confirmer, Policy, RunMode, StdinConfirmer};
pub use runtime::{ExecOutcome, Runtime};
