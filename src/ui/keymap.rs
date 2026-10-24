//! winit keys → X11 keysyms, which the host's RFB key handler expects.
//!
//! Printable keys are sent as the character the local layout produced, so
//! AZERTY/Dvorak etc. type what the user sees. On the Mac, Super is
//! Command and Alt is Option.

use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_CONTROL_R: u32 = 0xffe4;
pub const XK_SUPER_L: u32 = 0xffeb;
pub const XK_SUPER_R: u32 = 0xffec;

/// Keysym for a typed character: Latin-1 maps directly, everything else
/// uses the Unicode keysym range.
pub fn char_keysym(c: char) -> u32 {
    let cp = c as u32;
    match cp {
        0x20..=0x7e | 0xa0..=0xff => cp,
        _ => 0x0100_0000 | cp,
    }
}

fn named_keysym(k: NamedKey) -> Option<u32> {
    use NamedKey::*;
    Some(match k {
        Escape => 0xff1b,
        Tab => 0xff09,
        Backspace => 0xff08,
        Enter => 0xff0d,
        Space => 0x20,
        Delete => 0xffff,
        Insert => 0xff63,
        Home => 0xff50,
        End => 0xff57,
        PageUp => 0xff55,
        PageDown => 0xff56,
        ArrowLeft => 0xff51,
        ArrowUp => 0xff52,
        ArrowRight => 0xff53,
        ArrowDown => 0xff54,
        CapsLock => 0xffe5,
        Help => 0xff6a,
        PrintScreen => 0xff61,
        F1 => 0xffbe,
        F2 => 0xffbf,
        F3 => 0xffc0,
        F4 => 0xffc1,
        F5 => 0xffc2,
        F6 => 0xffc3,
        F7 => 0xffc4,
        F8 => 0xffc5,
        F9 => 0xffc6,
        F10 => 0xffc7,
        F11 => 0xffc8,
        F12 => 0xffc9,
        F13 => 0xffca,
        F14 => 0xffcb,
        F15 => 0xffcc,
        F16 => 0xffcd,
        F17 => 0xffce,
        F18 => 0xffcf,
        F19 => 0xffd0,
        F20 => 0xffd1,
        _ => return None,
    })
}

/// Modifier keys by physical position (left/right matters to the host).
fn modifier_keysym(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::ShiftLeft => 0xffe1,
        KeyCode::ShiftRight => 0xffe2,
        KeyCode::ControlLeft => XK_CONTROL_L,
        KeyCode::ControlRight => XK_CONTROL_R,
        KeyCode::AltLeft => 0xffe9,
        KeyCode::AltRight => 0xffea,
        KeyCode::SuperLeft => XK_SUPER_L,
        KeyCode::SuperRight => XK_SUPER_R,
        _ => return None,
    })
}

/// Keypad keys, sent as their main-keyboard equivalents so they work the
/// same regardless of NumLock handling on either side.
fn keypad_keysym(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::NumpadEnter => 0xff0d,
        _ => return None,
    })
}

/// Maps a key event to a keysym. `logical` should be the key without
/// modifiers applied when a shortcut modifier (Ctrl/Alt/Super) is held, so
/// e.g. Ctrl+C sends `c` rather than a control character.
pub fn keysym(physical: PhysicalKey, logical: &Key, swap_ctrl_cmd: bool) -> Option<u32> {
    if let PhysicalKey::Code(code) = physical {
        if let Some(ks) = modifier_keysym(code) {
            return Some(match (swap_ctrl_cmd, ks) {
                (true, XK_CONTROL_L) => XK_SUPER_L,
                (true, XK_CONTROL_R) => XK_SUPER_R,
                (true, XK_SUPER_L) => XK_CONTROL_L,
                (true, XK_SUPER_R) => XK_CONTROL_R,
                _ => ks,
            });
        }
        if let Some(ks) = keypad_keysym(code) {
            return Some(ks);
        }
    }
    match logical {
        Key::Named(n) => named_keysym(*n),
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            // Multi-character strings (ligatures, compose results) have no
            // single keysym.
            chars.next().is_none().then(|| char_keysym(c))
        }
        _ => None,
    }
}

/// Keysym for a key name of the control FIFO: one character, `plus` for
/// `+` (which joins the names of a shortcut), a modifier (`cmd`, `ctrl`,
/// `alt` or `opt`, `shift`, sent as the left key), or a named key
/// (`return`, `esc`, `tab`, `space`, `backspace`, `delete`, `home`, `end`,
/// `pageup`, `pagedown`, `left`, `right`, `up`, `down`, `f1` to `f20`).
pub fn by_name(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(char_keysym(c));
    }
    match name {
        "cmd" => return modifier_keysym(KeyCode::SuperLeft),
        "ctrl" => return modifier_keysym(KeyCode::ControlLeft),
        "alt" | "opt" => return modifier_keysym(KeyCode::AltLeft),
        "shift" => return modifier_keysym(KeyCode::ShiftLeft),
        "plus" => return Some(char_keysym('+')),
        _ => {}
    }
    use NamedKey::*;
    const F_KEYS: [NamedKey; 20] =
        [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20];
    named_keysym(match name {
        "return" | "enter" => Enter,
        "esc" | "escape" => Escape,
        "tab" => Tab,
        "space" => Space,
        "backspace" => Backspace,
        "delete" => Delete,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "left" => ArrowLeft,
        "right" => ArrowRight,
        "up" => ArrowUp,
        "down" => ArrowDown,
        _ => *F_KEYS.get(name.strip_prefix('f')?.parse::<usize>().ok()?.checked_sub(1)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_key_names() {
        assert_eq!(by_name("f"), Some(0x66));
        assert_eq!(by_name("cmd"), Some(XK_SUPER_L));
        assert_eq!(by_name("ctrl"), Some(XK_CONTROL_L));
        assert_eq!(by_name("opt"), Some(0xffe9));
        assert_eq!(by_name("return"), Some(0xff0d));
        assert_eq!(by_name("esc"), Some(0xff1b));
        assert_eq!(by_name("f1"), Some(0xffbe));
        assert_eq!(by_name("f20"), Some(0xffd1));
        assert_eq!(by_name("plus"), Some(0x2b));
        for bad in ["f0", "f21", "fx", "command", ""] {
            assert_eq!(by_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn maps_keys() {
        let p = PhysicalKey::Code(KeyCode::KeyA);
        assert_eq!(keysym(p, &Key::Character("a".into()), false), Some(0x61));
        assert_eq!(keysym(p, &Key::Character("é".into()), false), Some(0xe9));
        assert_eq!(keysym(p, &Key::Character("€".into()), false), Some(0x0100_20ac));
        assert_eq!(keysym(p, &Key::Named(NamedKey::Enter), false), Some(0xff0d));
        let ctrl = PhysicalKey::Code(KeyCode::ControlLeft);
        assert_eq!(keysym(ctrl, &Key::Named(NamedKey::Control), false), Some(XK_CONTROL_L));
        assert_eq!(keysym(ctrl, &Key::Named(NamedKey::Control), true), Some(XK_SUPER_L));
    }
}
