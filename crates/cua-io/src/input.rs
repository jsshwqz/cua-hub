//! Input injection on top of `enigo`.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use cua_core::{Action, Error, Point, Result};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

/// One process-wide Enigo instance so held buttons/keys survive across calls
/// (`mouse_down` → later `mouse_up`), which a per-call instance cannot do.
fn enigo() -> Result<MutexGuard<'static, Enigo>> {
    static INSTANCE: OnceLock<Mutex<Enigo>> = OnceLock::new();
    if INSTANCE.get().is_none() {
        let created = Enigo::new(&Settings::default())
            .map_err(|e| Error::Input(format!("enigo init: {e}")))?;
        let _ = INSTANCE.set(Mutex::new(created));
    }
    let m = INSTANCE.get().expect("just initialised");
    m.lock().or_else(|p| Ok(p.into_inner()))
}

/// Execute a mutating action. Points are in model space and are scaled by
/// `scale` (physical = model * scale).
pub fn apply(action: &Action, scale: f32) -> Result<()> {
    let mut enigo = enigo()?;

    match action {
        Action::Move { to } => {
            let (x, y) = phys(*to, scale);
            enigo.move_mouse(x, y, Coordinate::Abs).map_err(input_err)?;
        }
        Action::Click { at, button, double } => {
            let (x, y) = phys(*at, scale);
            enigo.move_mouse(x, y, Coordinate::Abs).map_err(input_err)?;
            // Win32/Qt hit-testing can act on the pointer position from the
            // previous event when the button arrives in the same tick, which
            // shows up as "the click landed where the cursor used to be". Give
            // the move a frame to be processed before pressing.
            std::thread::sleep(Duration::from_millis(25));
            let btn = map_button(*button);
            if *double {
                press_release(&mut enigo, btn, 40)?;
                std::thread::sleep(Duration::from_millis(60));
            }
            press_release(&mut enigo, btn, 40)?;
        }
        Action::Drag { from, to } => {
            let (x1, y1) = phys(*from, scale);
            let (x2, y2) = phys(*to, scale);
            enigo
                .move_mouse(x1, y1, Coordinate::Abs)
                .map_err(input_err)?;
            let mut guard = InputGuard::new(&mut *enigo);
            guard.run_and_release(|g| {
                g.press_button(Button::Left)?;
                std::thread::sleep(Duration::from_millis(80));
                g.device.send_move_mouse(x2, y2, Coordinate::Abs)?;
                std::thread::sleep(Duration::from_millis(80));
                Ok(())
            })?;
        }
        Action::Type { text, interval_ms } => {
            // Qt/FreeCAD and some industrial apps ignore Unicode key events for
            // characters that need Shift (uppercase, (), {}, quotes...). Send
            // those as an explicit Shift press + base key instead.
            let delay = Duration::from_millis((*interval_ms).max(1));
            let mut unicode = String::new();
            for ch in text.chars() {
                if ch.is_ascii() {
                    if !unicode.is_empty() {
                        enigo.text(&unicode).map_err(input_err)?;
                        unicode.clear();
                        std::thread::sleep(delay);
                    }
                    type_ascii_char(&mut enigo, ch)?;
                    std::thread::sleep(delay);
                } else {
                    unicode.push(ch);
                }
            }
            if !unicode.is_empty() {
                enigo.text(&unicode).map_err(input_err)?;
            }
        }
        Action::Key { combo } => key_combo(&mut enigo, combo)?,
        Action::Scroll { at, dx, dy } => {
            let (x, y) = phys(*at, scale);
            enigo.move_mouse(x, y, Coordinate::Abs).map_err(input_err)?;
            if *dy != 0 {
                enigo.scroll(*dy, Axis::Vertical).map_err(input_err)?;
            }
            if *dx != 0 {
                enigo.scroll(*dx, Axis::Horizontal).map_err(input_err)?;
            }
        }
        // Non-input actions are handled by the runner.
        Action::Screenshot
        | Action::Wait { .. }
        | Action::Done { .. }
        | Action::Fail { .. }
        | Action::RunCommand { .. } => {}
    }
    Ok(())
}

/// Current pointer position in physical pixels.
pub fn cursor_position() -> Result<(i32, i32)> {
    let enigo = enigo()?;
    enigo.location().map_err(input_err)
}

/// Press or release a mouse button without moving the pointer.
pub fn mouse_button(button: cua_core::MouseButton, down: bool) -> Result<()> {
    let held = crate::hold::Held::from_mouse_button(button);
    let mut enigo = enigo()?;
    let direction = if down {
        Direction::Press
    } else {
        Direction::Release
    };
    enigo
        .button(map_button(button), direction)
        .map_err(input_err)?;
    if down {
        crate::hold::mark_intentional(held);
    } else {
        crate::hold::clear_intentional(held);
    }
    Ok(())
}

/// Press or release a key or chord, e.g. `"shift"` or `"ctrl+shift"`.
pub fn key_state(combo: &str, down: bool) -> Result<()> {
    let mut enigo = enigo()?;
    let parts: Vec<&str> = combo
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(Error::InvalidAction("empty key combo".into()));
    }
    let direction = if down {
        Direction::Press
    } else {
        Direction::Release
    };
    let mut keys = Vec::new();
    for part in parts {
        let key = parse_modifier(part)
            .or_else(|| parse_key(part))
            .ok_or_else(|| Error::InvalidAction(format!("unknown key: {part}")))?;
        keys.push(key);
    }
    if down {
        let mut guard = InputGuard::new(&mut *enigo);
        let mut pressed: Vec<Option<crate::hold::Held>> = Vec::new();
        for key in keys {
            guard.press_key(key)?;
            pressed.push(crate::hold::Held::from_enigo_key(key));
        }
        // Success: caller wants them held down, disarm guard without releasing
        guard.held.clear();
        // First-layer bookkeeping (#15): mark only keys that were actually
        // pressed. A failed press returned above and the guard already
        // released whatever got through, so nothing gets marked.
        for h in pressed.into_iter().flatten() {
            crate::hold::mark_intentional(h);
        }
    } else {
        // Release in reverse order so chords unwind cleanly.
        keys.reverse();
        let held_keys: Vec<Option<crate::hold::Held>> = keys
            .iter()
            .map(|k| crate::hold::Held::from_enigo_key(*k))
            .collect();
        let mut errors = Vec::new();
        for (key, held) in keys.iter().zip(held_keys.iter()) {
            if let Err(e) = enigo.key(*key, direction) {
                errors.push(e.to_string());
            } else if let Some(h) = held {
                crate::hold::clear_intentional(*h);
            }
        }
        if !errors.is_empty() {
            return Err(Error::Input(format!(
                "failed to release {} key(s): {}",
                errors.len(),
                errors.join("; ")
            )));
        }
        // A key whose release failed keeps its intentional mark: the caller
        // gets the error and retries, and the sweep must not treat it as a
        // leaked hold until the caller gives up on it.
    }
    Ok(())
}

/// Base key for ASCII characters that need Shift on a US layout.
fn shifted_base(ch: char) -> Option<char> {
    Some(match ch {
        'A'..='Z' => ch.to_ascii_lowercase(),
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    })
}

/// Common interface for keyboard, mouse button, and pointer movement injection.
pub trait InputDevice {
    fn send_key(&mut self, key: Key, direction: Direction) -> Result<()>;
    fn send_button(&mut self, button: Button, direction: Direction) -> Result<()>;
    fn send_move_mouse(&mut self, x: i32, y: i32, coordinate: Coordinate) -> Result<()>;
}

impl InputDevice for Enigo {
    fn send_key(&mut self, key: Key, direction: Direction) -> Result<()> {
        Keyboard::key(self, key, direction).map_err(input_err)
    }

    fn send_button(&mut self, button: Button, direction: Direction) -> Result<()> {
        Mouse::button(self, button, direction).map_err(input_err)
    }

    fn send_move_mouse(&mut self, x: i32, y: i32, coordinate: Coordinate) -> Result<()> {
        Mouse::move_mouse(self, x, y, coordinate).map_err(input_err)
    }
}

/// A held key or mouse button tracked for RAII release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldInput {
    Key(Key),
    Button(Button),
}

/// RAII Drop guard ensuring pressed keys and mouse buttons are released upon scope exit,
/// preventing stuck Shift/Ctrl/Alt or mouse buttons on error paths or panics.
pub struct InputGuard<'a, D: InputDevice = Enigo> {
    pub device: &'a mut D,
    pub held: Vec<HeldInput>,
}

impl<'a, D: InputDevice> InputGuard<'a, D> {
    pub fn new(device: &'a mut D) -> Self {
        Self {
            device,
            held: Vec::new(),
        }
    }

    pub fn press_key(&mut self, key: Key) -> Result<()> {
        self.device.send_key(key, Direction::Press)?;
        self.held.push(HeldInput::Key(key));
        Ok(())
    }

    pub fn press_button(&mut self, button: Button) -> Result<()> {
        self.device.send_button(button, Direction::Press)?;
        self.held.push(HeldInput::Button(button));
        Ok(())
    }

    /// Explicitly release all held inputs in reverse order (LIFO).
    /// If one fails, still makes a best-effort attempt to release the rest,
    /// and aggregates/reports any errors encountered.
    pub fn release_all(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        while let Some(item) = self.held.pop() {
            let res = match item {
                HeldInput::Key(k) => self.device.send_key(k, Direction::Release),
                HeldInput::Button(b) => self.device.send_button(b, Direction::Release),
            };
            if let Err(e) = res {
                errors.push(e.to_string());
            }
        }
        if !errors.is_empty() {
            return Err(Error::Input(format!(
                "failed to release {} held input(s): {}",
                errors.len(),
                errors.join("; ")
            )));
        }
        Ok(())
    }

    /// Execute a fallible operation and ensure all held inputs are released afterwards.
    /// If the operation fails, all held inputs are released, and if releasing also fails,
    /// both errors are combined into a descriptive error message.
    pub fn run_and_release<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        let res = f(self);
        let rel_res = self.release_all();
        match (res, rel_res) {
            (Ok(val), Ok(())) => Ok(val),
            (Ok(_), Err(rel_err)) => Err(rel_err),
            (Err(op_err), Ok(())) => Err(op_err),
            (Err(op_err), Err(rel_err)) => Err(Error::Input(format!(
                "{op_err}; additionally failed to release inputs: {rel_err}"
            ))),
        }
    }
}

impl<'a, D: InputDevice> Drop for InputGuard<'a, D> {
    fn drop(&mut self) {
        while let Some(item) = self.held.pop() {
            let _ = match item {
                HeldInput::Key(k) => self.device.send_key(k, Direction::Release),
                HeldInput::Button(b) => self.device.send_button(b, Direction::Release),
            };
        }
    }
}

fn type_ascii_char(enigo: &mut Enigo, ch: char) -> Result<()> {
    match ch {
        '\n' | '\r' => {
            enigo
                .key(Key::Return, Direction::Click)
                .map_err(input_err)?;
            return Ok(());
        }
        '\t' => {
            enigo.key(Key::Tab, Direction::Click).map_err(input_err)?;
            return Ok(());
        }
        _ => {}
    }
    if let Some(base) = shifted_base(ch) {
        let mut guard = InputGuard::new(enigo);
        guard.run_and_release(|g| {
            g.press_key(Key::Shift)?;
            g.device.send_key(Key::Unicode(base), Direction::Click)?;
            Ok(())
        })?;
    } else {
        enigo
            .key(Key::Unicode(ch), Direction::Click)
            .map_err(input_err)?;
    }
    Ok(())
}

fn phys(p: Point, scale: f32) -> (i32, i32) {
    (
        (p.x as f32 * scale).round() as i32,
        (p.y as f32 * scale).round() as i32,
    )
}

fn map_button(b: cua_core::MouseButton) -> Button {
    match b {
        cua_core::MouseButton::Left => Button::Left,
        cua_core::MouseButton::Right => Button::Right,
        cua_core::MouseButton::Middle => Button::Middle,
    }
}

/// Press, hold, release. A zero-duration click is accepted by menus but is
/// dropped or mis-hit by canvas/edit controls that sample the pointer on
/// press, so the hold is explicit.
fn press_release(enigo: &mut Enigo, button: Button, hold_ms: u64) -> Result<()> {
    let mut guard = InputGuard::new(enigo);
    guard.run_and_release(|g| {
        g.press_button(button)?;
        std::thread::sleep(Duration::from_millis(hold_ms));
        Ok(())
    })
}

fn input_err(e: impl std::fmt::Display) -> Error {
    Error::Input(e.to_string())
}

/// Press a chord like `ctrl+c`, `alt+Tab`, `Return`.
fn key_combo(enigo: &mut Enigo, combo: &str) -> Result<()> {
    key_combo_with_device(enigo, combo)
}

fn key_combo_with_device<D: InputDevice>(device: &mut D, combo: &str) -> Result<()> {
    let parts: Vec<&str> = combo
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(Error::InvalidAction(format!("empty key combo: {combo}")));
    }

    let (mods, key_name) = parts.split_at(parts.len() - 1);
    let key = parse_key(key_name[0])
        .ok_or_else(|| Error::InvalidAction(format!("unknown key: {}", key_name[0])))?;

    let mut guard = InputGuard::new(device);
    guard.run_and_release(|g| {
        for m in mods {
            if let Some(k) = parse_modifier(m) {
                g.press_key(k)?;
            } else {
                return Err(Error::InvalidAction(format!("unknown modifier: {m}")));
            }
        }

        g.device.send_key(key, Direction::Click)?;
        std::thread::sleep(Duration::from_millis(20));
        Ok(())
    })
}

fn parse_modifier(name: &str) -> Option<Key> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Key::Control),
        "alt" | "option" | "opt" => Some(Key::Alt),
        "shift" => Some(Key::Shift),
        "meta" | "win" | "cmd" | "super" => Some(Key::Meta),
        _ => None,
    }
}

fn parse_key(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    let k = match lower.as_str() {
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "esc" | "escape" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" => Key::PageUp,
        "pagedown" | "page_down" => Key::PageDown,
        "up" | "arrowup" => Key::UpArrow,
        "down" | "arrowdown" => Key::DownArrow,
        "left" | "arrowleft" => Key::LeftArrow,
        "right" | "arrowright" => Key::RightArrow,
        "capslock" => Key::CapsLock,
        "meta" | "win" | "super" | "cmd" => Key::Meta,
        "ctrl" | "control" => Key::Control,
        "alt" | "option" | "opt" => Key::Alt,
        "shift" => Key::Shift,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => return None,
            }
        }
    };
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_shifted_keys_but_not_plain_ones() {
        assert_eq!(shifted_base('A'), Some('a'));
        assert_eq!(shifted_base('Z'), Some('z'));
        assert_eq!(shifted_base('('), Some('9'));
        assert_eq!(shifted_base(')'), Some('0'));
        assert_eq!(shifted_base('?'), Some('/'));
        assert_eq!(shifted_base('"'), Some('\''));
        assert_eq!(shifted_base('a'), None);
        assert_eq!(shifted_base('1'), None);
        assert_eq!(shifted_base(' '), None);
    }

    #[derive(Default)]
    struct MockDevice {
        events: Vec<(String, Direction)>,
        fail_press: Option<String>,
        fail_release: Option<String>,
        fail_move: bool,
    }

    impl InputDevice for MockDevice {
        fn send_key(&mut self, key: Key, direction: Direction) -> Result<()> {
            let name = format!("{key:?}");
            if direction == Direction::Press && self.fail_press.as_deref() == Some(&name) {
                return Err(Error::Input(format!("simulated press failure on {name}")));
            }
            if direction == Direction::Release && self.fail_release.as_deref() == Some(&name) {
                return Err(Error::Input(format!("simulated release failure on {name}")));
            }
            self.events.push((name, direction));
            Ok(())
        }

        fn send_button(&mut self, button: Button, direction: Direction) -> Result<()> {
            let name = format!("{button:?}");
            if direction == Direction::Press && self.fail_press.as_deref() == Some(&name) {
                return Err(Error::Input(format!("simulated press failure on {name}")));
            }
            if direction == Direction::Release && self.fail_release.as_deref() == Some(&name) {
                return Err(Error::Input(format!("simulated release failure on {name}")));
            }
            self.events.push((name, direction));
            Ok(())
        }

        fn send_move_mouse(&mut self, _x: i32, _y: i32, _coordinate: Coordinate) -> Result<()> {
            if self.fail_move {
                return Err(Error::Input("simulated move failure".into()));
            }
            self.events.push(("mouse_move".into(), Direction::Press));
            Ok(())
        }
    }

    #[test]
    fn test_empty_key_combo_returns_error() {
        let mut mock = MockDevice::default();
        let res = key_combo_with_device(&mut mock, "   ");
        assert!(res.is_err());
    }

    #[test]
    fn test_key_combo_invalid_modifier_unwinds_pressed_keys() {
        let mut mock = MockDevice::default();
        let res = key_combo_with_device(&mut mock, "ctrl+nonexistent_modifier+c");
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("unknown modifier"), "{err_str}");

        assert_eq!(
            mock.events,
            vec![
                (format!("{:?}", Key::Control), Direction::Press),
                (format!("{:?}", Key::Control), Direction::Release),
            ]
        );
    }

    #[test]
    fn test_input_guard_run_and_release_success() {
        let mut mock = MockDevice::default();
        let mut guard = InputGuard::new(&mut mock);
        let res = guard.run_and_release(|g| {
            g.press_key(Key::Shift)?;
            assert_eq!(g.held.len(), 1);
            Ok(42)
        });
        assert_eq!(res.unwrap(), 42);
        assert!(guard.held.is_empty());
        assert_eq!(
            guard.device.events,
            vec![
                (format!("{:?}", Key::Shift), Direction::Press),
                (format!("{:?}", Key::Shift), Direction::Release),
            ]
        );
    }

    #[test]
    fn test_input_guard_run_and_release_error_unwinds() {
        let mut mock = MockDevice::default();
        let mut guard = InputGuard::new(&mut mock);
        let res: Result<()> = guard.run_and_release(|g| {
            g.press_key(Key::Shift)?;
            g.press_button(Button::Left)?;
            assert_eq!(g.held.len(), 2);
            Err(Error::Input("simulated action failure".into()))
        });
        assert!(res.is_err());
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("simulated action failure"), "{err_str}");
        assert!(guard.held.is_empty(), "guard must be released after error");
        assert_eq!(
            guard.device.events,
            vec![
                (format!("{:?}", Key::Shift), Direction::Press),
                (format!("{:?}", Button::Left), Direction::Press),
                (format!("{:?}", Button::Left), Direction::Release),
                (format!("{:?}", Key::Shift), Direction::Release),
            ]
        );
    }

    #[test]
    fn test_input_guard_best_effort_release_and_aggregate_errors() {
        let mut mock = MockDevice {
            fail_release: Some(format!("{:?}", Button::Left)),
            ..Default::default()
        };
        let mut guard = InputGuard::new(&mut mock);
        let res: Result<()> = guard.run_and_release(|g| {
            g.press_key(Key::Shift)?;
            g.press_button(Button::Left)?;
            Err(Error::Input("action failed".into()))
        });
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("action failed"), "{err}");
        assert!(err.contains("failed to release"), "{err}");
        assert!(
            guard
                .device
                .events
                .contains(&(format!("{:?}", Key::Shift), Direction::Release)),
            "Shift release must still have executed despite Button::Left release failure"
        );
    }

    #[test]
    fn test_drop_guard_releases_held_inputs_on_scope_exit() {
        let mut mock = MockDevice::default();
        {
            let mut guard = InputGuard::new(&mut mock);
            guard.press_key(Key::Alt).unwrap();
            guard.press_button(Button::Right).unwrap();
        }
        assert_eq!(
            mock.events,
            vec![
                (format!("{:?}", Key::Alt), Direction::Press),
                (format!("{:?}", Button::Right), Direction::Press),
                (format!("{:?}", Button::Right), Direction::Release),
                (format!("{:?}", Key::Alt), Direction::Release),
            ]
        );
    }
}
