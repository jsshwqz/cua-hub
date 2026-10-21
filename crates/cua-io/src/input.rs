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
    INSTANCE
        .get()
        .expect("just initialised")
        .lock()
        .map_err(|e| Error::Input(format!("enigo lock: {e}")))
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
            let btn = map_button(*button);
            if *double {
                enigo.button(btn, Direction::Click).map_err(input_err)?;
                std::thread::sleep(Duration::from_millis(60));
            }
            enigo.button(btn, Direction::Click).map_err(input_err)?;
        }
        Action::Drag { from, to } => {
            let (x1, y1) = phys(*from, scale);
            let (x2, y2) = phys(*to, scale);
            enigo
                .move_mouse(x1, y1, Coordinate::Abs)
                .map_err(input_err)?;
            enigo
                .button(Button::Left, Direction::Press)
                .map_err(input_err)?;
            std::thread::sleep(Duration::from_millis(80));
            enigo
                .move_mouse(x2, y2, Coordinate::Abs)
                .map_err(input_err)?;
            std::thread::sleep(Duration::from_millis(80));
            enigo
                .button(Button::Left, Direction::Release)
                .map_err(input_err)?;
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
    let mut enigo = enigo()?;
    let direction = if down {
        Direction::Press
    } else {
        Direction::Release
    };
    enigo
        .button(map_button(button), direction)
        .map_err(input_err)
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
    // Release in reverse order so chords unwind cleanly.
    if !down {
        keys.reverse();
    }
    for key in keys {
        enigo.key(key, direction).map_err(input_err)?;
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
        enigo.key(Key::Shift, Direction::Press).map_err(input_err)?;
        enigo
            .key(Key::Unicode(base), Direction::Click)
            .map_err(input_err)?;
        enigo
            .key(Key::Shift, Direction::Release)
            .map_err(input_err)?;
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

fn input_err(e: impl std::fmt::Display) -> Error {
    Error::Input(e.to_string())
}

/// Press a chord like `ctrl+c`, `alt+Tab`, `Return`.
fn key_combo(enigo: &mut Enigo, combo: &str) -> Result<()> {
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

    let mut pressed = Vec::new();
    for m in mods {
        if let Some(k) = parse_modifier(m) {
            enigo.key(k, Direction::Press).map_err(input_err)?;
            pressed.push(k);
        } else {
            return Err(Error::InvalidAction(format!("unknown modifier: {m}")));
        }
    }

    enigo.key(key, Direction::Click).map_err(input_err)?;
    std::thread::sleep(Duration::from_millis(20));
    for k in pressed.into_iter().rev() {
        enigo.key(k, Direction::Release).map_err(input_err)?;
    }
    Ok(())
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
    use super::shifted_base;

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
}
