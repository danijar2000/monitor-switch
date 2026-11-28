//! The hotkey format, shared by the hotkey program and `setup`, which
//! checks a hotkey before saving it: "ctrl+enter", "ctrl+alt+cmd+m".

// Carbon modifier masks.
pub const CMD: u32 = 0x0100;
pub const SHIFT: u32 = 0x0200;
pub const ALT: u32 = 0x0800;
pub const CTRL: u32 = 0x1000;

/// ANSI virtual key codes for the keys a hotkey may end with.
pub fn key_code(name: &str) -> Option<u32> {
    const KEYS: &[(&str, u32)] = &[
        ("a", 0), ("s", 1), ("d", 2), ("f", 3), ("h", 4), ("g", 5), ("z", 6), ("x", 7),
        ("c", 8), ("v", 9), ("b", 11), ("q", 12), ("w", 13), ("e", 14), ("r", 15),
        ("y", 16), ("t", 17), ("1", 18), ("2", 19), ("3", 20), ("4", 21), ("6", 22),
        ("5", 23), ("9", 25), ("7", 26), ("8", 28), ("0", 29), ("o", 31), ("u", 32),
        ("i", 34), ("p", 35), ("l", 37), ("j", 38), ("k", 40), ("n", 45), ("m", 46),
        ("f1", 122), ("f2", 120), ("f3", 99), ("f4", 118), ("f5", 96), ("f6", 97),
        ("f7", 98), ("f8", 100), ("f9", 101), ("f10", 109), ("f11", 103), ("f12", 111),
        ("enter", 36), ("return", 36), ("tab", 48), ("space", 49), ("esc", 53),
    ];
    KEYS.iter().find(|(key, _)| *key == name).map(|(_, code)| *code)
}

/// "ctrl+alt+cmd+m" -> (key code, modifier mask)
pub fn parse(hotkey: &str) -> Result<(u32, u32), String> {
    let mut modifiers = 0;
    let mut key = None;
    for part in hotkey.to_lowercase().split('+').map(str::trim) {
        match part {
            "cmd" | "command" => modifiers |= CMD,
            "shift" => modifiers |= SHIFT,
            "alt" | "opt" | "option" => modifiers |= ALT,
            "ctrl" | "control" => modifiers |= CTRL,
            name => key = Some(key_code(name).ok_or(format!("unknown key: {name}"))?),
        }
    }
    let key = key.ok_or("the hotkey names no key")?;
    if modifiers == 0 {
        return Err("the hotkey needs at least one modifier".into());
    }
    // macOS 15 refuses hotkeys whose only modifiers are Option and Shift.
    if modifiers & (CTRL | CMD) == 0 && modifiers & ALT != 0 {
        return Err("macOS does not allow Option or Option+Shift alone: add ctrl or cmd".into());
    }
    Ok((key, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        assert_eq!(parse("ctrl+enter"), Ok((36, CTRL)));
        assert_eq!(parse("Ctrl + Alt + Cmd + M"), Ok((46, CTRL | ALT | CMD)));
        assert_eq!(parse("shift+f12"), Ok((111, SHIFT)));
    }

    #[test]
    fn rejects_unusable_hotkeys() {
        assert!(parse("enter").is_err());
        assert!(parse("ctrl+shift").is_err());
        assert!(parse("ctrl+hyper").is_err());
        assert!(parse("alt+m").is_err());
        assert!(parse("alt+shift+m").is_err());
        assert!(parse("ctrl+alt+m").is_ok());
    }
}
