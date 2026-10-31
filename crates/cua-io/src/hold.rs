//! What is physically held down, and how to put it back.
//!
//! Injected input is fire-and-forget. An injection call returns success even
//! when the event never reaches its target, and a press whose release was
//! dropped leaves the machine behaving as if the key were taped down: every
//! later click becomes a shift-click, every typed letter comes out uppercase,
//! and nothing in the process can see why. The audit that opened this issue
//! found five such paths in [`crate::input`] and no recovery at all.
//!
//! Two layers close that gap:
//!
//! 1. the injection paths refuse to return while they still hold something —
//!    see [`crate::input::HeldGuard`];
//! 2. this module asks the operating system what is *actually* pressed, so a
//!    hold leaked by an earlier crash, a killed task or a swallowed release is
//!    detected and cleared before it can corrupt the next action.
//!
//! Windows answers the question: `GetAsyncKeyState` reports real-time physical
//! state per left/right-hand key. Linux has no equivalent query, so there the
//! truth functions report [`Down::Unknown`], [`release_stuck`] stays inert, and
//! recovery rests on layer 1 — which works everywhere.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cua_core::MouseButton;

/// One input that can be held down.
///
/// Named per physical key rather than per logical role because that is the
/// granularity the OS answers at: a stuck right-hand Shift is a different
/// hardware state from a stuck left-hand one, and a repair has to target the
/// side that is actually down.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Held {
    LeftShift,
    RightShift,
    LeftControl,
    RightControl,
    LeftAlt,
    RightAlt,
    LeftMeta,
    RightMeta,
    LeftButton,
    RightButton,
    MiddleButton,
}

/// Every input this module can reason about, in sweep order.
pub const ALL: [Held; 11] = [
    Held::LeftShift,
    Held::RightShift,
    Held::LeftControl,
    Held::RightControl,
    Held::LeftAlt,
    Held::RightAlt,
    Held::LeftMeta,
    Held::RightMeta,
    Held::LeftButton,
    Held::RightButton,
    Held::MiddleButton,
];

impl Held {
    /// The label used in reports and tool output.
    pub fn label(self) -> &'static str {
        match self {
            Held::LeftShift => "shift",
            Held::RightShift => "rshift",
            Held::LeftControl => "ctrl",
            Held::RightControl => "rctrl",
            Held::LeftAlt => "alt",
            Held::RightAlt => "ralt",
            Held::LeftMeta => "win",
            Held::RightMeta => "rwin",
            Held::LeftButton => "left",
            Held::RightButton => "right",
            Held::MiddleButton => "middle",
        }
    }

    /// The physical button a core mouse button maps to.
    pub fn from_mouse_button(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => Held::LeftButton,
            MouseButton::Right => Held::RightButton,
            MouseButton::Middle => Held::MiddleButton,
        }
    }

    /// Whether a key this process injects is one that stays held, and which
    /// physical key it presses.
    ///
    /// `Key::Shift`/`Control`/`Alt`/`Meta` are generic roles, and both enigo and
    /// Windows resolve them to the left-hand key (enigo maps `Shift` to
    /// `VK_SHIFT` with its left scancode), so they classify as the left side.
    /// Anything else is either a self-releasing key click or out of scope.
    pub fn from_enigo_key(key: enigo::Key) -> Option<Self> {
        use enigo::Key;
        match key {
            Key::Shift | Key::LShift => Some(Held::LeftShift),
            Key::Control | Key::LControl => Some(Held::LeftControl),
            Key::Alt => Some(Held::LeftAlt),
            Key::Meta => Some(Held::LeftMeta),
            _ => None,
        }
    }
}

/// What the operating system reports for one input.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Down {
    Yes,
    No,
    /// The platform offers no way to ask.
    Unknown,
}

/// Whether this platform lets us read the physical input state at all.
pub const TRUTH_IS_AVAILABLE: bool = cfg!(windows);

/// Inputs this process is holding *on purpose*: `mouse_down` and `key_down`
/// register them and the matching release clears them. A sweep must never touch
/// these — a drag in progress looks exactly like a leak from the outside.
fn intentional() -> &'static Mutex<BTreeSet<Held>> {
    static REGISTRY: OnceLock<Mutex<BTreeSet<Held>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Record that a hold is caller-owned, so a sweep leaves it alone.
pub fn mark_intentional(held: Held) {
    if let Ok(mut set) = intentional().lock() {
        set.insert(held);
    }
}

/// Clear that record once the caller has released the hold.
pub fn clear_intentional(held: Held) {
    if let Ok(mut set) = intentional().lock() {
        set.remove(&held);
    }
}

/// Currently registered caller-owned holds.
pub fn intentional_holds() -> BTreeSet<Held> {
    intentional()
        .lock()
        .map(|set| set.clone())
        .unwrap_or_default()
}

/// Physical state of one input, as far as the platform can tell.
pub fn is_down(held: Held) -> Down {
    #[cfg(windows)]
    {
        windows_state(held)
    }
    #[cfg(not(windows))]
    {
        let _ = held;
        Down::Unknown
    }
}

/// Every input the OS reports as physically held.
///
/// Empty on platforms that cannot answer, which is what keeps a sweep there
/// from guessing.
pub fn scan() -> BTreeSet<Held> {
    ALL.iter()
        .copied()
        .filter(|held| is_down(*held) == Down::Yes)
        .collect()
}

/// Which reported-down inputs must be released: everything the OS says is held
/// that the caller did not ask us to hold.
///
/// Pure, so the decision is covered by tests on every platform while the
/// injection next to it stays thin enough to audit by eye.
pub fn stuck_plan(os_down: &BTreeSet<Held>, owned: &BTreeSet<Held>) -> Vec<Held> {
    ALL.iter()
        .copied()
        .filter(|h| os_down.contains(h) && !owned.contains(h))
        .collect()
}

/// What a sweep did.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Inputs this pass released.
    pub released: Vec<Held>,
    /// Inputs still reported down afterwards: the release was refused, which on
    /// Windows means a secure desktop or an elevation boundary, not a leak here.
    pub still_down: Vec<Held>,
}

impl Report {
    /// True when nothing had to be done.
    pub fn is_clean(&self) -> bool {
        self.released.is_empty() && self.still_down.is_empty()
    }

    /// Human-readable summary for tool output and logs.
    pub fn summary(&self) -> String {
        let named = |list: &Vec<Held>| {
            list.iter()
                .map(|h| h.label())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if self.is_clean() {
            return "nothing was held".to_string();
        }
        let mut parts = Vec::new();
        if !self.released.is_empty() {
            parts.push(format!("released {}", named(&self.released)));
        }
        if !self.still_down.is_empty() {
            parts.push(format!("still held {}", named(&self.still_down)));
        }
        parts.join("; ")
    }
}

/// How long an input must be continuously down before a sweep treats it as a
/// leak rather than someone typing.
///
/// A human tap clears 60 ms easily; a leaked programmatic hold stays until
/// something releases it. Sampling over this window is what makes it safe to
/// sweep the whole keyboard on a machine the user is also using.
const LEAK_WINDOW: Duration = Duration::from_millis(60);

/// Release every leaked hold and report what happened.
///
/// Free when nothing is stuck: one read per input and no waiting. When
/// something is down, it is sampled again after [`LEAK_WINDOW`] so a key the
/// user is physically pressing right now is left alone and only a hold that
/// persists is forced up. Never fails — a sweep that cannot fix the machine
/// reports that instead of aborting the action that asked for it.
pub fn release_stuck() -> Report {
    let mut report = Report::default();
    if !TRUTH_IS_AVAILABLE {
        return report;
    }

    let first = scan();
    if first.is_empty() {
        return report;
    }
    let candidates = stuck_plan(&first, &intentional_holds());
    if candidates.is_empty() {
        return report;
    }

    let deadline = Instant::now() + LEAK_WINDOW;
    let mut persisting: Vec<Held> = Vec::new();
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let now = scan();
        for held in &candidates {
            if now.contains(held) && !persisting.contains(held) {
                persisting.push(*held);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    for held in persisting {
        force_release(held);
        if is_down(held) == Down::Yes {
            report.still_down.push(held);
        } else {
            report.released.push(held);
        }
    }
    if !report.still_down.is_empty() {
        tracing::warn!(still_down = ?report.still_down, "held inputs refused release");
    }
    report
}

/// Put one input up through the lowest-level channel available, bypassing the
/// path the original injection used.
///
/// On Windows this posts a release for the physical key itself, which is what
/// repairs a hold some other code left behind. Other platforms have no such
/// channel, so this does nothing and the caller trusts the injection result —
/// the behaviour that predates this module.
pub fn force_release(held: Held) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            keybd_event, mouse_event, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC_EX,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTUP,
        };

        /// Release a physical key with the scancode Windows itself maps for it,
        /// including the extended prefix that tells left and right hand apart.
        fn release_key(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) {
            use windows::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW;
            let code = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC_EX) };
            let mut flags = KEYEVENTF_KEYUP;
            if code & 0xE000 != 0 {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            unsafe { keybd_event(vk.0 as u8, code as u8, flags, 0) };
        }

        match held {
            Held::LeftShift => release_key(vk::LEFT_SHIFT),
            Held::RightShift => release_key(vk::RIGHT_SHIFT),
            Held::LeftControl => release_key(vk::LEFT_CONTROL),
            Held::RightControl => release_key(vk::RIGHT_CONTROL),
            Held::LeftAlt => release_key(vk::LEFT_MENU),
            Held::RightAlt => release_key(vk::RIGHT_MENU),
            Held::LeftMeta => release_key(vk::LEFT_WIN),
            Held::RightMeta => release_key(vk::RIGHT_WIN),
            Held::LeftButton => unsafe { mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0) },
            Held::RightButton => unsafe { mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0) },
            Held::MiddleButton => unsafe { mouse_event(MOUSEEVENTF_MIDDLEUP, 0, 0, 0, 0) },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = held;
    }
}

#[cfg(windows)]
mod vk {
    //! The virtual-key codes for the physical inputs [`Held`] names.
    #![allow(non_snake_case)]
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VIRTUAL_KEY, VK_LBUTTON, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MBUTTON, VK_RBUTTON,
        VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
    };

    pub const LEFT_SHIFT: VIRTUAL_KEY = VK_LSHIFT;
    pub const RIGHT_SHIFT: VIRTUAL_KEY = VK_RSHIFT;
    pub const LEFT_CONTROL: VIRTUAL_KEY = VK_LCONTROL;
    pub const RIGHT_CONTROL: VIRTUAL_KEY = VK_RCONTROL;
    pub const LEFT_MENU: VIRTUAL_KEY = VK_LMENU;
    pub const RIGHT_MENU: VIRTUAL_KEY = VK_RMENU;
    pub const LEFT_WIN: VIRTUAL_KEY = VK_LWIN;
    pub const RIGHT_WIN: VIRTUAL_KEY = VK_RWIN;
    pub const LEFT_BUTTON: VIRTUAL_KEY = VK_LBUTTON;
    pub const RIGHT_BUTTON: VIRTUAL_KEY = VK_RBUTTON;
    pub const MIDDLE_BUTTON: VIRTUAL_KEY = VK_MBUTTON;
}

/// The virtual key a held input refers to, used for state queries.
#[cfg(windows)]
fn held_vk(held: Held) -> windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY {
    #[cfg(windows)]
    {
        let _ = held;
    }
    match held {
        Held::LeftShift => vk::LEFT_SHIFT,
        Held::RightShift => vk::RIGHT_SHIFT,
        Held::LeftControl => vk::LEFT_CONTROL,
        Held::RightControl => vk::RIGHT_CONTROL,
        Held::LeftAlt => vk::LEFT_MENU,
        Held::RightAlt => vk::RIGHT_MENU,
        Held::LeftMeta => vk::LEFT_WIN,
        Held::RightMeta => vk::RIGHT_WIN,
        Held::LeftButton => vk::LEFT_BUTTON,
        Held::RightButton => vk::RIGHT_BUTTON,
        Held::MiddleButton => vk::MIDDLE_BUTTON,
    }
}

#[cfg(windows)]
fn windows_state(held: Held) -> Down {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // The high-order bit is the real-time physical state; the low bit is a
    // "pressed since last call" flag and means nothing for a hold.
    let state = unsafe { GetAsyncKeyState(held_vk(held).0 as i32) };
    if state & 0x8000u16 as i16 != 0 {
        Down::Yes
    } else {
        Down::No
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[Held]) -> BTreeSet<Held> {
        items.iter().copied().collect()
    }

    #[test]
    fn every_input_has_a_distinct_label() {
        let labels: Vec<&str> = ALL.iter().map(|h| h.label()).collect();
        let unique: BTreeSet<&str> = labels.iter().copied().collect();
        assert_eq!(labels.len(), ALL.len());
        assert_eq!(unique.len(), ALL.len(), "labels must identify one key each");
    }

    #[test]
    fn generic_modifier_roles_are_the_left_hand_keys() {
        // The claim this module makes about the OS is only correct while enigo
        // and Windows resolve a generic role to the left-hand physical key.
        assert_eq!(
            Held::from_enigo_key(enigo::Key::Shift),
            Some(Held::LeftShift)
        );
        assert_eq!(
            Held::from_enigo_key(enigo::Key::Control),
            Some(Held::LeftControl)
        );
        assert_eq!(Held::from_enigo_key(enigo::Key::Alt), Some(Held::LeftAlt));
        assert_eq!(Held::from_enigo_key(enigo::Key::Meta), Some(Held::LeftMeta));
        assert_eq!(Held::from_enigo_key(enigo::Key::Return), None);
        assert_eq!(Held::from_enigo_key(enigo::Key::Unicode('a')), None);
    }

    #[test]
    fn mouse_buttons_map_to_their_physical_identity() {
        assert_eq!(Held::from_mouse_button(MouseButton::Left), Held::LeftButton);
        assert_eq!(
            Held::from_mouse_button(MouseButton::Right),
            Held::RightButton
        );
        assert_eq!(
            Held::from_mouse_button(MouseButton::Middle),
            Held::MiddleButton
        );
    }

    #[test]
    fn a_hold_we_own_is_never_reaped() {
        // A drag in progress reports "left button down" just like a leak does;
        // the registry is the only thing that tells them apart.
        let down = set(&[Held::LeftButton]);
        assert_eq!(stuck_plan(&down, &set(&[Held::LeftButton])), vec![]);
        assert_eq!(stuck_plan(&down, &set(&[])), vec![Held::LeftButton]);
    }

    #[test]
    fn a_leaked_modifier_is_planned_in_sweep_order() {
        let plan = stuck_plan(&set(&[Held::RightAlt, Held::LeftShift]), &set(&[]));
        assert_eq!(plan, vec![Held::LeftShift, Held::RightAlt]);
    }

    #[test]
    fn clearing_one_hold_does_not_clear_another() {
        mark_intentional(Held::LeftShift);
        mark_intentional(Held::LeftControl);
        clear_intentional(Held::LeftShift);
        let plan = stuck_plan(
            &set(&[Held::LeftShift, Held::LeftControl]),
            &intentional_holds(),
        );
        assert_eq!(plan, vec![Held::LeftShift]);
        clear_intentional(Held::LeftControl);
        assert!(intentional_holds().is_empty());
    }

    #[test]
    fn a_clean_report_says_so() {
        assert!(Report::default().is_clean());
        assert_eq!(Report::default().summary(), "nothing was held");
        let report = Report {
            released: vec![Held::LeftShift],
            still_down: vec![Held::LeftButton, Held::RightControl],
        };
        assert!(!report.is_clean());
        assert_eq!(report.summary(), "released shift; still held left, rctrl");
    }

    #[test]
    fn a_sweep_without_platform_truth_does_nothing_and_reports_nothing() {
        // On Linux the query has no answer, so a sweep must neither claim to
        // have released anything nor disturb the machine.
        if !TRUTH_IS_AVAILABLE {
            assert!(scan().is_empty());
            assert!(release_stuck().is_clean());
            assert_eq!(is_down(Held::LeftShift), Down::Unknown);
        }
    }
}
