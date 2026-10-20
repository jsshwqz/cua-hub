//! 键鼠注入（enigo）与按键组合解析，如 "ctrl+l"、"Return"、"alt+f4"。

use anyhow::{bail, Result};
use enigo::{Direction, Enigo, Key, Keyboard};

pub fn parse_key(name: &str) -> Result<Key> {
    let lower = name.to_ascii_lowercase();
    let key = match lower.as_str() {
        "ctrl" | "control" => Key::Control,
        "alt" => Key::Alt,
        "shift" => Key::Shift,
        "meta" | "super" | "win" => Key::Meta,
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "capslock" => Key::CapsLock,
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
                _ => bail!("unknown key: {name}"),
            }
        }
    };
    Ok(key)
}

/// 执行组合键：修饰键依次按下 → 主键点击 → 修饰键逆序释放。
pub fn press_combo(enigo: &mut Enigo, combo: &str) -> Result<()> {
    let parts: Vec<&str> = combo
        .split('+')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let (main, mods) = match parts.split_last() {
        Some((m, rest)) => (*m, rest),
        None => bail!("empty key combo"),
    };
    for m in mods {
        enigo.key(parse_key(m)?, Direction::Press)?;
    }
    let result = enigo.key(parse_key(main)?, Direction::Click);
    for m in mods.iter().rev() {
        let _ = enigo.key(parse_key(m)?, Direction::Release);
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_keys() {
        assert!(matches!(parse_key("Return"), Ok(Key::Return)));
        assert!(matches!(parse_key("ctrl"), Ok(Key::Control)));
        assert!(matches!(parse_key("a"), Ok(Key::Unicode('a'))));
        assert!(parse_key("nosuchkey").is_err());
    }
}
