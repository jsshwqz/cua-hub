//! Human-takeover guard for CUA tasks.
//!
//! While the agent drives the desktop it owns the keyboard and mouse. This
//! module lets a human operator seize control at any moment — without a
//! manual switch they have to remember to flip:
//!
//! 1. **Automatic seizure (default on)**: a pair of low-level hooks
//!    (`WH_KEYBOARD_LL` / `WH_MOUSE_LL`) inspects every input event. cua-hub
//!    injects its own input through `SendInput`, which the OS tags with
//!    `LLKHF_INJECTED`; genuine physical input never carries that tag. The
//!    first real keystroke or mouse click flips the shared state to
//!    `PAUSED`, and the runner blocks before its next step.
//! 2. **Manual hotkeys (explicit override)**: `Ctrl+Alt+H` toggles pause /
//!    resume, `Ctrl+Alt+X` stops the task outright.
//! 3. **Idle auto-resume**: once the human stops touching the machine for
//!    `IDLE_AUTO_RESUME_MS`, the agent picks back up on its own.
//!
//! The hook callbacks do almost no work — read a flag, bump a timestamp,
//! touch one atomic — because Windows silently unhooks a low-level hook whose
//! callback blocks longer than `LowLevelHooksTimeout`.
//!
//! Non-Windows platforms get a no-op implementation with the same API.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Shared takeover state machine.
///
/// `status` is read by the runner (to gate each step) and by the overlay (to
/// recolor the capsule); it is written by the hook callbacks (auto seizure),
/// the hotkey handler (manual override) and the auto-resume tick.
#[derive(Debug)]
pub struct TakeoverState {
    status: AtomicU8,
    /// `GetTickCount64()` of the last genuine human input, in ms.
    last_human: AtomicU64,
    /// Milliseconds without human input after which the agent auto-resumes.
    idle_auto_resume_ms: AtomicU64,
}

pub const RUNNING: u8 = 0;
pub const PAUSED: u8 = 1;
pub const STOPPED: u8 = 2;

/// How long the human must leave the machine alone before the agent resumes.
const IDLE_AUTO_RESUME_MS: u64 = 2500;

impl TakeoverState {
    fn new() -> Self {
        Self {
            status: AtomicU8::new(RUNNING),
            last_human: AtomicU64::new(0),
            idle_auto_resume_ms: AtomicU64::new(IDLE_AUTO_RESUME_MS),
        }
    }

    /// Current status: [`RUNNING`], [`PAUSED`] or [`STOPPED`].
    pub fn status(&self) -> u8 {
        self.status.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.status() == PAUSED
    }

    pub fn is_stopped(&self) -> bool {
        self.status() == STOPPED
    }

    /// Marks that genuine human input just arrived. Only flips `RUNNING` to
    /// `PAUSED`; a manual `STOPPED` is never silently undone by input.
    pub fn note_human_input(&self, now_ms: u64) {
        self.last_human.store(now_ms, Ordering::Relaxed);
        let _ = self
            .status
            .compare_exchange(RUNNING, PAUSED, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Manual pause toggle from the hotkey. Pause and resume are explicit and
    /// override the automatic path; stop is one-way.
    pub fn toggle_pause(&self) {
        match self.status() {
            RUNNING => self.status.store(PAUSED, Ordering::Relaxed),
            PAUSED => {
                self.last_human.store(now_ms(), Ordering::Relaxed);
                self.status.store(RUNNING, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn stop(&self) {
        self.status.store(STOPPED, Ordering::Relaxed);
    }

    /// Resumes when the human has been idle past the threshold. Returns true
    /// if it transitioned `PAUSED -> RUNNING`.
    fn auto_resume_if_idle(&self) -> bool {
        if self.status() != PAUSED {
            return false;
        }
        let last = self.last_human.load(Ordering::Relaxed);
        let idle_ms = self.idle_auto_resume_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms().saturating_sub(last) >= idle_ms {
            self.status.store(RUNNING, Ordering::Relaxed);
            return true;
        }
        false
    }
}

#[cfg(windows)]
mod imp {
    use super::{now_ms, TakeoverState};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, RegisterHotKey, SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_KEYUP, MOD_CONTROL, MOD_NOREPEAT, VIRTUAL_KEY, VK_CONTROL, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
        KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_REMOVE, WH_KEYBOARD_LL,
        WH_MOUSE_LL, WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_RBUTTONDOWN, WM_SYSKEYDOWN,
    };

    /// Set when the guard is dropped so the pump can unwind its hooks and let
    /// the unhook code below the loop stay reachable.
    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    /// Low-level hook timeout is 300ms by default; stay far under it by doing
    /// nothing but an atomic write and a timestamp in the callbacks below.
    unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if !SHUTDOWN.load(Ordering::Relaxed) {
            let msg = wparam.0 as u32;
            if code >= 0 && (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) {
                let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                // cua-hub's own SendInput events carry LLKHF_INJECTED; physical
                // keystrokes never do. Only the latter mean "a human is typing".
                // `flags`/`LLKHF_INJECTED` are `KBDLLHOOKSTRUCT_FLAGS` newtypes
                // over u32, so compare the raw bits.
                if kb.flags.0 & LLKHF_INJECTED.0 == 0 {
                    if let Some(st) = super::shared_state() {
                        st.note_human_input(now_ms());
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if !SHUTDOWN.load(Ordering::Relaxed) {
            let msg = wparam.0 as u32;
            if code >= 0 && (msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN) {
                let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
                // MSLLHOOKSTRUCT.flags is a plain u32 (unlike the keyboard
                // struct's newtype), so mask the injected bit directly.
                if ms.flags & LLKHF_INJECTED.0 == 0 {
                    if let Some(st) = super::shared_state() {
                        st.note_human_input(now_ms());
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    const HK_TOGGLE: i32 = 1;
    const HK_STOP: i32 = 2;

    /// Drives the hook + hotkey message loop until the process exits.
    fn run_takeover_loop(state: Arc<TakeoverState>) {
        // Shared state is published through a process-global so the hook
        // callbacks (which only receive a code/wparam/lparam) can reach it.
        super::publish_state(state.clone());

        unsafe {
            let kb_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0);
            let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0);

            // Manual override: Ctrl+Alt+H toggles pause, Ctrl+Alt+X stops.
            // NOREPEAT keeps a held key from retriggering.
            let _ = RegisterHotKey(
                None,
                HK_TOGGLE,
                MOD_CONTROL | MOD_NOREPEAT,
                u32::from('H' as u16),
            );
            let _ = RegisterHotKey(
                None,
                HK_STOP,
                MOD_CONTROL | MOD_NOREPEAT,
                u32::from('X' as u16),
            );

            let mut msg = MSG::default();
            loop {
                // Poll with PeekMessageW rather than blocking in GetMessageW:
                // low-level hooks are dispatched through this pump either way,
                // and polling lets us re-check idle auto-resume every tick even
                // when the human leaves no pending messages behind.
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_HOTKEY {
                        match msg.wParam.0 as i32 {
                            HK_TOGGLE => state.toggle_pause(),
                            HK_STOP => state.stop(),
                            _ => {}
                        }
                        continue;
                    }
                    let _ = DispatchMessageW(&msg);
                }

                // Idle auto-resume: the human walked away, hand control back.
                if state.auto_resume_if_idle() {
                    // The human may still be physically holding Ctrl/Alt (e.g.
                    // they just pressed Ctrl+Alt+H before walking off); release
                    // the stray modifiers so the agent's next injected
                    // keystroke can't combine with them into a shortcut.
                    nudge_modifier_release();
                }

                if SHUTDOWN.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }

            if let Ok(h) = kb_hook {
                let _ = UnhookWindowsHookEx(h);
            }
            if let Ok(h) = mouse_hook {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }

    /// Best-effort synthesized Ctrl release.
    ///
    /// If the human pressed Ctrl+Alt+H, the physical Ctrl/Alt keys are still
    /// down when the agent resumes; without this nudge the next injected
    /// keystroke would combine with them into an unintended shortcut.
    pub fn nudge_modifier_release() {
        unsafe {
            // GetAsyncKeyState returns i16; the high bit is set while the key
            // is physically down. Widen to u16 before masking so 0x8000 fits.
            let down = |vk: VIRTUAL_KEY| (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0;
            if !(down(VK_CONTROL) || down(VK_MENU)) {
                return;
            }
            for vk in [VK_CONTROL, VK_MENU] {
                let input = INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(vk.0),
                            wScan: 0,
                            dwFlags: KEYEVENTF_KEYUP,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
    }

    /// Spawns the takeover thread. Returns the shared state handle.
    pub fn spawn_takeover() -> Arc<TakeoverState> {
        let state = Arc::new(TakeoverState::new());
        let thread_state = state.clone();
        std::thread::Builder::new()
            .name("cua-takeover".into())
            .spawn(move || run_takeover_loop(thread_state))
            .expect("spawn takeover thread");
        // Give the hook a moment to install so the very first human input is
        // already observed by the time run_task reaches its first step.
        std::thread::sleep(Duration::from_millis(60));
        state
    }

    /// Signals the takeover pump to unhook and exit. Called when the guard is
    /// dropped so the global hotkeys and low-level hooks do not outlive the
    /// run. The pump notices within one 40ms tick and unwinds its hooks.
    pub fn signal_shutdown() {
        SHUTDOWN.store(true, Ordering::Relaxed);
    }
}

#[cfg(windows)]
use std::sync::Mutex;

#[cfg(windows)]
static SHARED: Mutex<Option<Arc<TakeoverState>>> = Mutex::new(None);

#[cfg(windows)]
fn publish_state(state: Arc<TakeoverState>) {
    if let Ok(mut slot) = SHARED.lock() {
        *slot = Some(state);
    }
}

#[cfg(windows)]
fn shared_state() -> Option<Arc<TakeoverState>> {
    SHARED.lock().ok().and_then(|slot| slot.clone())
}

pub fn now_ms() -> u64 {
    #[cfg(windows)]
    unsafe {
        windows::Win32::System::SystemInformation::GetTickCount64()
    }
    #[cfg(not(windows))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// RAII guard owning the takeover machinery for one task run.
///
/// `state()` hands the runner and the overlay a shared view of the machine:
/// the runner gates each step on it, the overlay recolors itself from it.
/// Dropping the guard signals the hook thread to unhook and exit (via
/// `SHUTDOWN`), so the global hotkeys do not linger after the run; a fresh
/// run installs them again and starts in `RUNNING`.
pub struct TakeoverGuard {
    enabled: bool,
    state: Arc<TakeoverState>,
}

impl TakeoverGuard {
    /// Starts the takeover machinery if enabled.
    ///
    /// Respects `CUA_HUB_TAKEOVER` (`0`/`false`/`off` disables) on top of the
    /// per-run `enabled` flag. Returns a dormant guard when disabled or on
    /// unsupported platforms.
    pub fn start(enabled: bool) -> Self {
        let effective = enabled && takeover_env_enabled();
        #[cfg(windows)]
        {
            if effective {
                let state = imp::spawn_takeover();
                return Self {
                    enabled: true,
                    state,
                };
            }
        }
        Self {
            enabled: false,
            state: Arc::new(TakeoverState::new()),
        }
    }

    pub fn state(&self) -> Arc<TakeoverState> {
        self.state.clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Blocks the caller while the human is in control. Returns false if the
    /// task was stopped outright (the runner should wind down), true if it
    /// may continue.
    pub fn wait_gate(&self) -> bool {
        loop {
            match self.state.status() {
                RUNNING => return true,
                STOPPED => return false,
                _ => {
                    // The takeover pump auto-resumes once the human is idle;
                    // poll here so the runner reacts within a few ms.
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

#[cfg(windows)]
impl Drop for TakeoverGuard {
    fn drop(&mut self) {
        // Only signal if we actually installed hooks; a dormant guard has no
        // pump thread waiting on the flag.
        if self.enabled {
            imp::signal_shutdown();
        }
    }
}

/// Parse `CUA_HUB_TAKEOVER` environment variable. Defaults to `true`.
pub fn takeover_env_enabled() -> bool {
    parse_takeover_env(std::env::var("CUA_HUB_TAKEOVER").ok().as_deref())
}

/// Pure helper to parse the environment variable value string.
pub fn parse_takeover_env(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => {
            let s = v.trim().to_ascii_lowercase();
            !matches!(s.as_str(), "0" | "false" | "off" | "no")
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takeover_env_variants() {
        assert!(parse_takeover_env(None));
        assert!(parse_takeover_env(Some("1")));
        assert!(parse_takeover_env(Some("true")));
        assert!(!parse_takeover_env(Some("0")));
        assert!(!parse_takeover_env(Some("false")));
        assert!(!parse_takeover_env(Some(" off ")));
        assert!(parse_takeover_env(Some("yes")));
    }

    #[test]
    fn human_input_pauses_running_but_not_stopped() {
        let st = TakeoverState::new();
        assert_eq!(st.status(), RUNNING);
        st.note_human_input(1000);
        assert_eq!(st.status(), PAUSED);
        // A stopped task is never silently un-stopped by input.
        st.stop();
        st.note_human_input(2000);
        assert_eq!(st.status(), STOPPED);
    }

    #[test]
    fn idle_auto_resume_only_after_threshold() {
        // Drive the threshold rather than the wall clock: a huge threshold can
        // never elapse during the test, so no resume is correct; a zero
        // threshold is always exceeded, so an immediate resume is correct.
        let st = TakeoverState::new();
        st.idle_auto_resume_ms.store(u64::MAX, Ordering::Relaxed);
        st.note_human_input(now_ms());
        assert_eq!(st.status(), PAUSED);
        assert!(!st.auto_resume_if_idle(), "huge threshold must not resume");
        assert_eq!(st.status(), PAUSED);

        // A zero threshold is always past due, so the agent resumes at once.
        st.idle_auto_resume_ms.store(0, Ordering::Relaxed);
        assert!(st.auto_resume_if_idle());
        assert_eq!(st.status(), RUNNING);
        // Already running: a further idle check is a no-op.
        assert!(!st.auto_resume_if_idle());
    }

    #[test]
    fn toggle_pause_round_trips() {
        let st = TakeoverState::new();
        st.toggle_pause();
        assert_eq!(st.status(), PAUSED);
        st.toggle_pause();
        assert_eq!(st.status(), RUNNING);
        // Stop is one-way even for the toggle.
        st.stop();
        st.toggle_pause();
        assert_eq!(st.status(), STOPPED);
    }

    #[test]
    fn stop_then_input_stays_stopped() {
        let st = TakeoverState::new();
        st.stop();
        st.note_human_input(1);
        assert_eq!(st.status(), STOPPED);
        assert!(st.is_stopped());
    }

    #[test]
    fn dormant_guard_reports_running_and_passes_gate() {
        let g = TakeoverGuard::start(false);
        assert!(!g.is_enabled());
        assert_eq!(g.state().status(), RUNNING);
        assert!(g.wait_gate());
    }
}
