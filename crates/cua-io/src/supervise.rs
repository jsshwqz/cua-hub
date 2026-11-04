//! Supervision mode for cua-hub: one interactive control bar + human takeover.
//!
//! 监督模式把"运行态显示"与"人工接管"合成一个概念：
//!
//! - **开**：屏幕上有一条交互式控制条（可点按钮、可拖动、位置持久化、对截屏
//!   隐形），低级钩子同时上岗——真人物理输入立即暂停 agent。
//! - **关**：纯后台——无窗口、钩子不拦输入，agent 不受任何干扰。
//!
//! 控制条**不绑定单次任务生命周期**：它是进程级单例（serve 模式下跨任务常驻，
//! 任务结束后进入"待机"态），带 ⏻（监督开/关）与 ✕（优雅结束）两个按钮，
//! `Ctrl+Alt+O` 全局热键随时显隐/开关兜底。
//!
//! Agent 正在执行动作的瞬间，控制条临时恢复点击穿透（`WS_EX_TRANSPARENT`），
//! 保证模型选中的坐标落在真实目标上而不是被控制条挡住；动作结束立即恢复可交互。
//!
//! 非 Windows 平台编译为 no-op。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::takeover::TakeoverState;

/// Everything the runner, the bar window and the hook thread share.
pub struct Supervision {
    /// Takeover state machine (`RUNNING`/`PAUSED`/`STOPPED`).
    pub state: Arc<TakeoverState>,
    /// 监督开/关：关 = 纯后台（钩子不拦、条隐藏、agent 不被打断）。
    pub enabled: Arc<AtomicBool>,
    /// 控制条显/隐（⏻ 关闭、`Ctrl+Alt+O`、任务结束后的待机都走这里）。
    pub visible: Arc<AtomicBool>,
    /// ✕ 按钮按下：runner 优雅收尾（写审计）后进程自然退出。
    pub quit: Arc<AtomicBool>,
    /// agent 正在执行输入动作：控制条临时点击穿透，动作结束恢复交互。
    pub action: Arc<AtomicBool>,
    #[cfg(windows)]
    bar: Option<crate::overlay::WindowsBar>,
}

static HUB: Mutex<Option<Arc<Supervision>>> = Mutex::new(None);

/// Returns the process-wide supervision hub, creating it on first call.
///
/// `None` when supervision was not requested (`enabled == false`), the env
/// kill-switch is set, or the platform has no interactive desktop. Subsequent
/// calls reuse the existing bar — it is deliberately NOT bound to one task.
pub fn ensure(enabled: bool, task: &str, max_steps: usize) -> Option<Arc<Supervision>> {
    if let Ok(hub) = HUB.lock() {
        if let Some(existing) = hub.as_ref() {
            let existing = existing.clone();
            drop(hub);
            existing.begin_task(task, max_steps);
            return Some(existing);
        }
    }

    if !enabled || !supervise_env_enabled() {
        return None;
    }

    #[cfg(windows)]
    {
        let sup = create(task, max_steps);
        if let Ok(mut hub) = HUB.lock() {
            *hub = Some(sup.clone());
        }
        Some(sup)
    }
    #[cfg(not(windows))]
    {
        let _ = (task, max_steps);
        None
    }
}

/// Process-wide access for the hotkey pump: `Ctrl+Alt+O` toggles supervision
/// even when no task is running. No-op when supervision was never created.
pub fn toggle_supervision() {
    if let Ok(hub) = HUB.lock() {
        if let Some(sup) = hub.as_ref() {
            if sup.enabled.load(Ordering::Relaxed) {
                sup.disable();
            } else {
                sup.enable();
            }
        }
    }
}

/// Whether takeover hooks should act on genuine human input. Hooks call this
/// so the ⏻ button can turn supervision into pure background mid-run.
pub fn supervision_enabled() -> bool {
    HUB.lock()
        .ok()
        .and_then(|hub| hub.clone())
        .map(|sup| sup.enabled.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// The bar's ⏻ button: supervision off (pure background). The bar hides and
/// the hooks stand down; `Ctrl+Alt+O` is the way back.
pub fn turn_off() {
    if let Ok(hub) = HUB.lock() {
        if let Some(sup) = hub.as_ref() {
            sup.disable();
        }
    }
}

/// The bar's ✕ button: graceful end. The takeover state flips to `STOPPED`
/// and the quit flag rises; the runner finishes the current step, writes its
/// audit trail and returns — the process then exits on its own.
pub fn request_quit() {
    if let Ok(hub) = HUB.lock() {
        if let Some(sup) = hub.as_ref() {
            sup.quit.store(true, Ordering::Relaxed);
            sup.state.stop();
        }
    }
}

#[cfg(windows)]
fn create(task: &str, max_steps: usize) -> Arc<Supervision> {
    let state = Arc::new(TakeoverState::new());
    let enabled = Arc::new(AtomicBool::new(true));
    let visible = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let action = Arc::new(AtomicBool::new(false));
    // Hooks first so the very first human keystroke is already observed when
    // the bar appears; the bar thread then owns the window for the process.
    crate::takeover::spawn_takeover_shared(state.clone());
    let bar = crate::overlay::WindowsBar::start(
        task,
        max_steps,
        state.clone(),
        visible.clone(),
        action.clone(),
    );
    Arc::new(Supervision {
        state,
        enabled,
        visible,
        quit,
        action,
        bar,
    })
}

impl Supervision {
    /// Per-task reset: a fresh run always starts visible, enabled and running.
    /// This is what lets the bar outlive tasks — each task reuses the same one.
    pub fn begin_task(&self, task: &str, max_steps: usize) {
        self.quit.store(false, Ordering::Relaxed);
        self.enabled.store(true, Ordering::Relaxed);
        self.visible.store(true, Ordering::Relaxed);
        self.action.store(false, Ordering::Relaxed);
        self.state.reset_running();
        #[cfg(windows)]
        if let Some(bar) = &self.bar {
            bar.begin_task(task, max_steps);
        }
        #[cfg(not(windows))]
        let _ = (task, max_steps);
    }

    /// Task over: the bar stays up in standby (it is not bound to the task),
    /// unless ✕ was pressed — then it hides while the process winds down.
    pub fn end_task(&self, summary: &str) {
        #[cfg(windows)]
        if let Some(bar) = &self.bar {
            if self.quit.load(Ordering::Relaxed) {
                self.visible.store(false, Ordering::Relaxed);
            }
            bar.end_task(summary);
        }
        #[cfg(not(windows))]
        let _ = summary;
    }

    /// Feeds the current step into the bar display.
    pub fn update(&self, step: usize, max_steps: usize, status: &str) {
        #[cfg(windows)]
        if let Some(bar) = &self.bar {
            bar.update(step, max_steps, status);
        }
        #[cfg(not(windows))]
        let _ = (step, max_steps, status);
    }

    /// Blocks while the human is in control; false when stopped/quit — the
    /// runner should wind the task down and write its audit trail.
    pub fn wait_gate(&self) -> bool {
        if self.quit.load(Ordering::Relaxed) {
            return false;
        }
        loop {
            match self.state.status() {
                crate::takeover::RUNNING => return true,
                crate::takeover::STOPPED => return false,
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    /// Agent input actions must land on real targets even if they fall under
    /// the bar: flip click-through on while executing, back off afterwards.
    pub fn set_action_executing(&self, executing: bool) {
        self.action.store(executing, Ordering::Relaxed);
    }

    /// ⏻ off: pure background. Hooks stand down, a paused agent resumes, the
    /// bar hides. `Ctrl+Alt+O` (or the next supervised task) brings it back.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        self.visible.store(false, Ordering::Relaxed);
        // A paused agent must not stay parked just because the bar went away.
        self.state.resume_running();
    }

    /// Back on (hotkey fallback / next supervised task).
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
        self.visible.store(true, Ordering::Relaxed);
    }
}

/// Parse `CUA_HUB_SUPERVISE`. Defaults to `true` on desktop environments.
/// Setting `0`/`false`/`off`/`no` acts as an absolute kill-switch.
pub fn supervise_env_enabled() -> bool {
    parse_supervise_env(std::env::var("CUA_HUB_SUPERVISE").ok().as_deref())
}

/// Pure helper to parse the environment variable value string.
pub fn parse_supervise_env(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => {
            let s = v.trim().to_ascii_lowercase();
            !matches!(s.as_str(), "0" | "false" | "off" | "no")
        }
        None => true,
    }
}

/// Where the bar position persists across process restarts.
pub fn position_path() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|base| PathBuf::from(base).join("cua-hub").join("bar_position.txt"))
}

/// Parse the persisted `"x,y"` line. Pure, testable.
pub fn parse_position(raw: &str) -> Option<(i32, i32)> {
    let s = raw.trim();
    let (x, y) = s.split_once(',')?;
    let x: i32 = x.trim().parse().ok()?;
    let y: i32 = y.trim().parse().ok()?;
    Some((x, y))
}

/// Format the persisted `"x,y"` line. Pure, testable.
pub fn format_position(x: i32, y: i32) -> String {
    format!("{x},{y}")
}

/// Keep the bar on screen even if the monitor layout shrank since the last
/// run. Pure, testable: clamps `(x, y)` so the whole bar stays visible.
pub fn clamp_position(
    x: i32,
    y: i32,
    bar_w: i32,
    bar_h: i32,
    screen_w: i32,
    screen_h: i32,
) -> (i32, i32) {
    let max_x = (screen_w - bar_w).max(0);
    let max_y = (screen_h - bar_h).max(0);
    (x.clamp(0, max_x), y.clamp(0, max_y))
}

/// Loads the position persisted by a previous run, if any.
pub fn load_saved_position() -> Option<(i32, i32)> {
    let path = position_path()?;
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| parse_position(&text))
}

/// Persists the bar position so the next run restores it (③ 位置持久化).
pub fn persist_position(x: i32, y: i32) {
    let Some(path) = position_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format_position(x, y));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_supervise_env_defaults_to_enabled() {
        assert!(parse_supervise_env(None), "default is enabled");
        assert!(!parse_supervise_env(Some("0")));
        assert!(!parse_supervise_env(Some("false")));
        assert!(!parse_supervise_env(Some("off")));
        assert!(!parse_supervise_env(Some("no")));
        assert!(parse_supervise_env(Some("1")));
        assert!(parse_supervise_env(Some(" true ")));
        assert!(parse_supervise_env(Some("ON")));
    }

    #[test]
    fn position_round_trips() {
        assert_eq!(parse_position("120,48"), Some((120, 48)));
        assert_eq!(parse_position(" 7 , 9 "), Some((7, 9)));
        assert_eq!(parse_position("nope"), None);
        assert_eq!(parse_position("1"), None);
        assert_eq!(format_position(12, 34), "12,34");
        let parsed = parse_position(&format_position(1920, 1080));
        assert_eq!(parsed, Some((1920, 1080)));
    }

    #[test]
    fn clamped_position_keeps_bar_on_screen() {
        // Screen shrank since last run: bar must stay fully visible.
        assert_eq!(
            clamp_position(5000, 5000, 380, 46, 1920, 1080),
            (1540, 1034)
        );
        assert_eq!(clamp_position(-50, -50, 380, 46, 1920, 1080), (0, 0));
        assert_eq!(clamp_position(100, 100, 380, 46, 1920, 1080), (100, 100));
        // Degenerate tiny screen: never negative.
        assert_eq!(clamp_position(10, 10, 380, 46, 100, 20), (0, 0));
    }
}
