//! winit keys → X11 keysyms, which the Mac's Screen Sharing agent reads.
//!
//! A printable key goes as the text that the local layout committed, so
//! AltGr and dead keys type what the user sees here. The agent looks each
//! character up in the Mac's own layout, and presses the key and the
//! modifiers that type it there. When that layout has no key for the
//! character, the agent posts the character itself. So the Swiss German `@`
//! (AltGr+2 here, ⌥G on the Mac) arrives as `@`. A shortcut goes by the key at
//! the base level of the layout, so Super+C sends `c` with Command held.
//!
//! The agent maps the modifier keysyms by its own table: Super is Command,
//! Meta is Option, and Alt is Command too. So the Alt keys go as Meta. AltGr
//! only selects the character here, and never reaches the Mac.

use std::collections::HashMap;

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey};

pub const XK_SHIFT_L: u32 = 0xffe1;
pub const XK_SHIFT_R: u32 = 0xffe2;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_CONTROL_R: u32 = 0xffe4;
pub const XK_META_L: u32 = 0xffe7;
pub const XK_META_R: u32 = 0xffe8;
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
        KeyCode::ShiftLeft => XK_SHIFT_L,
        KeyCode::ShiftRight => XK_SHIFT_R,
        KeyCode::ControlLeft => XK_CONTROL_L,
        KeyCode::ControlRight => XK_CONTROL_R,
        KeyCode::AltLeft => XK_META_L,
        KeyCode::AltRight => XK_META_R,
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

/// The keysym of a key that names one character or a named key.
fn key_keysym(key: &Key) -> Option<u32> {
    match key {
        Key::Named(n) => named_keysym(*n),
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            chars.next().is_none().then(|| char_keysym(c))
        }
        _ => None,
    }
}

/// One key event of the window, as the mapping reads it.
pub struct KeyInput<'a> {
    pub code: KeyCode,
    pub down: bool,
    /// The key with the held modifiers and a pending dead key applied.
    pub logical: &'a Key,
    /// The key at the base level of the layout, for shortcuts.
    pub base: &'a Key,
    /// The text that the key commits, such as `ê` after a dead `^`.
    pub text: Option<&'a str>,
    pub mods: ModifiersState,
}

/// What a key event sends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Out {
    Down(u32),
    Up(u32),
    /// The local key combination: give the compositor's shortcuts back, or
    /// take them again.
    ToggleShortcuts,
}

enum Press {
    /// A key that stays down on the Mac until its release here.
    Hold(u32),
    /// Committed text of more than one character, typed at once.
    Type(Vec<u32>),
}

/// The local key combination that toggles the compositor's shortcuts:
/// Ctrl+Alt+Shift+Z, as Moonlight toggles its capture.
fn toggles_shortcuts(k: &KeyInput) -> bool {
    let m = k.mods;
    m.control_key() && m.alt_key() && m.shift_key() && matches!(k.base, Key::Character(s) if s.eq_ignore_ascii_case("z"))
}

fn press(k: &KeyInput, swap_ctrl_cmd: bool) -> Option<Press> {
    if *k.logical == Key::Named(NamedKey::AltGraph) {
        return None;
    }
    if let Some(ks) = modifier_keysym(k.code) {
        return Some(Press::Hold(match (swap_ctrl_cmd, ks) {
            (true, XK_CONTROL_L) => XK_SUPER_L,
            (true, XK_CONTROL_R) => XK_SUPER_R,
            (true, XK_SUPER_L) => XK_CONTROL_L,
            (true, XK_SUPER_R) => XK_CONTROL_R,
            _ => ks,
        }));
    }
    if let Some(ks) = keypad_keysym(k.code) {
        return Some(Press::Hold(ks));
    }
    let m = k.mods;
    if m.control_key() || m.alt_key() || m.super_key() {
        return key_keysym(k.base).map(Press::Hold);
    }
    // Control characters, such as the "\r" of Enter, go by the named key.
    if let Some(text) = k.text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control)) {
        let mut keys: Vec<u32> = text.chars().map(char_keysym).collect();
        return Some(match keys.len() {
            1 => Press::Hold(keys.remove(0)),
            _ => Press::Type(keys),
        });
    }
    key_keysym(k.logical).map(Press::Hold)
}

/// The keys held on the Mac, and how each key event changes them.
pub struct Keyboard {
    swap_ctrl_cmd: bool,
    /// Keysym sent for each held physical key, so releases match presses
    /// even if modifiers changed in between.
    held: HashMap<KeyCode, u32>,
}

impl Keyboard {
    pub fn new(swap_ctrl_cmd: bool) -> Self {
        Self { swap_ctrl_cmd, held: HashMap::new() }
    }

    pub fn key(&mut self, k: &KeyInput) -> Vec<Out> {
        if !k.down {
            return self.held.remove(&k.code).map(Out::Up).into_iter().collect();
        }
        // The Mac sees the modifiers of the combination go up, and never its Z.
        if toggles_shortcuts(k) {
            let mut out = self.release_all();
            out.push(Out::ToggleShortcuts);
            return out;
        }
        match press(k, self.swap_ctrl_cmd) {
            None => Vec::new(),
            Some(Press::Hold(ks)) => {
                let mut out = Vec::new();
                if let Some(prev) = self.held.insert(k.code, ks)
                    && prev != ks
                {
                    out.push(Out::Up(prev));
                }
                out.push(Out::Down(ks));
                out
            }
            Some(Press::Type(keys)) => keys.into_iter().flat_map(|ks| [Out::Down(ks), Out::Up(ks)]).collect(),
        }
    }

    /// Releases every key held on the Mac, e.g. when the focus goes while a
    /// shortcut is down and its release would never arrive.
    pub fn release_all(&mut self) -> Vec<Out> {
        self.held.drain().map(|(_, ks)| Out::Up(ks)).collect()
    }

    /// Forgets the held keys without a release, for a Mac that forgot them
    /// with its session.
    pub fn forget(&mut self) {
        self.held.clear();
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
    use KeyCode::*;

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    /// A key event of the Swiss German layout on Linux, as winit gives it.
    struct Ev {
        code: KeyCode,
        logical: Key,
        base: Key,
        text: Option<String>,
    }

    impl Ev {
        /// A key that commits `text`.
        fn text(code: KeyCode, base: &str, text: &str) -> Self {
            Self { code, logical: ch(text), base: ch(base), text: Some(text.into()) }
        }

        /// A dead key, which commits nothing.
        fn dead(code: KeyCode, base: &str, accent: char) -> Self {
            Self { code, logical: Key::Dead(Some(accent)), base: ch(base), text: None }
        }

        fn named(code: KeyCode, key: NamedKey, text: Option<&str>) -> Self {
            Self { code, logical: Key::Named(key), base: Key::Named(key), text: text.map(Into::into) }
        }

        fn modifier(code: KeyCode, key: NamedKey) -> Self {
            Self::named(code, key, None)
        }
    }

    fn altgr() -> Ev {
        Ev::modifier(AltRight, NamedKey::AltGraph)
    }

    fn super_left() -> Ev {
        Ev::modifier(SuperLeft, NamedKey::Super)
    }

    /// Presses each key of `chord` in turn, holding the ones before it, then
    /// releases them in reverse order, and returns what the Mac gets.
    fn chord(kb: &mut Keyboard, chord: &[Ev]) -> Vec<Out> {
        let mut mods = ModifiersState::empty();
        let mut out = Vec::new();
        let mut send = |ev: &Ev, down: bool, mods: ModifiersState| {
            let text = ev.text.as_deref();
            out.extend(kb.key(&KeyInput { code: ev.code, down, logical: &ev.logical, base: &ev.base, text, mods }));
        };
        for ev in chord {
            send(ev, true, mods);
            mods |= match ev.code {
                ShiftLeft | ShiftRight => ModifiersState::SHIFT,
                ControlLeft | ControlRight => ModifiersState::CONTROL,
                AltLeft => ModifiersState::ALT,
                SuperLeft | SuperRight => ModifiersState::SUPER,
                _ => ModifiersState::empty(),
            };
        }
        for ev in chord.iter().rev() {
            send(ev, false, mods);
        }
        out
    }

    fn typed(chords: Vec<Vec<Ev>>) -> Vec<Out> {
        let mut kb = Keyboard::new(false);
        chords.iter().flat_map(|c| chord(&mut kb, c)).collect()
    }

    /// A press and release of the key that types `c`, with no modifier.
    fn tap(c: char) -> [Out; 2] {
        [Out::Down(char_keysym(c)), Out::Up(char_keysym(c))]
    }

    #[test]
    fn altgr_2_types_an_at_sign_without_a_modifier_on_the_mac() {
        assert_eq!(typed(vec![vec![altgr(), Ev::text(Digit2, "2", "@")]]), tap('@'));
    }

    #[test]
    fn altgr_combinations_type_their_swiss_german_characters() {
        let keys = [
            (Digit3, "3", '#'),
            (Digit7, "7", '|'),
            (IntlBackslash, "<", '\\'),
            (BracketLeft, "ü", '['),
            (BracketRight, "¨", ']'),
            (Quote, "ä", '{'),
            (Backslash, "$", '}'),
        ];
        for (code, base, c) in keys {
            assert_eq!(typed(vec![vec![altgr(), Ev::text(code, base, &c.to_string())]]), tap(c), "{c}");
        }
    }

    #[test]
    fn dead_circumflex_then_e_types_e_with_circumflex() {
        let typed = typed(vec![vec![Ev::dead(Equal, "^", '^')], vec![Ev::text(KeyE, "e", "ê")]]);
        assert_eq!(typed, [Out::Down(0xea), Out::Up(0xea)]);
    }

    #[test]
    fn dead_circumflex_then_space_types_a_circumflex() {
        let space = Ev::named(Space, NamedKey::Space, Some("^"));
        assert_eq!(typed(vec![vec![Ev::dead(Equal, "^", '^')], vec![space]]), [Out::Down(0x5e), Out::Up(0x5e)]);
    }

    #[test]
    fn each_dead_key_types_its_accent_on_a_vowel_and_alone_after_a_space() {
        /// The level that selects a dead key of the Swiss German layout.
        #[derive(PartialEq)]
        enum Level {
            Base,
            Shift,
            AltGr,
        }
        let dead_keys = [
            (Level::Base, Equal, "^", '^', 'â'),
            (Level::Shift, Equal, "^", '`', 'à'),
            (Level::AltGr, Minus, "'", '´', 'á'),
            (Level::Base, BracketRight, "¨", '¨', 'ä'),
            (Level::AltGr, Equal, "^", '~', 'ã'),
        ];
        for (level, code, base, accent, on_a) in dead_keys {
            let dead = || {
                let mut keys = match level {
                    Level::Base => vec![],
                    Level::Shift => vec![Ev::modifier(ShiftLeft, NamedKey::Shift)],
                    Level::AltGr => vec![altgr()],
                };
                keys.push(Ev::dead(code, base, accent));
                keys
            };
            let a = Ev::text(KeyA, "a", &on_a.to_string());
            let space = Ev::named(Space, NamedKey::Space, Some(&accent.to_string()));
            let shift: &[Out] = if level == Level::Shift { &[Out::Down(XK_SHIFT_L), Out::Up(XK_SHIFT_L)] } else { &[] };
            let want = [shift, &tap(on_a), shift, &tap(accent)].concat();
            assert_eq!(typed(vec![dead(), vec![a], dead(), vec![space]]), want, "{accent}");
        }
    }

    #[test]
    fn umlauts_type_themselves() {
        let shift_oe = vec![Ev::modifier(ShiftLeft, NamedKey::Shift), Ev::text(Semicolon, "ö", "Ö")];
        assert_eq!(
            typed(vec![vec![Ev::text(Quote, "ä", "ä")], shift_oe]),
            [Out::Down(0xe4), Out::Up(0xe4), Out::Down(XK_SHIFT_L), Out::Down(0xd6), Out::Up(0xd6), Out::Up(XK_SHIFT_L)]
        );
    }

    #[test]
    fn committed_text_of_two_characters_is_typed_at_once() {
        let (a, b) = (0x0100_01ce, 'b' as u32);
        assert_eq!(typed(vec![vec![Ev::text(KeyA, "a", "ǎb")]]), [Out::Down(a), Out::Up(a), Out::Down(b), Out::Up(b)]);
    }

    #[test]
    fn enter_goes_as_return_and_not_as_its_text() {
        let enter = Ev::named(Enter, NamedKey::Enter, Some("\r"));
        assert_eq!(typed(vec![vec![enter]]), [Out::Down(0xff0d), Out::Up(0xff0d)]);
    }

    #[test]
    fn super_left_is_the_mac_command_key() {
        assert_eq!(typed(vec![vec![super_left()]]), [Out::Down(XK_SUPER_L), Out::Up(XK_SUPER_L)]);
    }

    #[test]
    fn super_c_tab_and_space_send_their_key_with_command_held() {
        let keys = [
            (Ev::text(KeyC, "c", "c"), 'c' as u32),
            (Ev::named(Tab, NamedKey::Tab, Some("\t")), 0xff09),
            (Ev::named(Space, NamedKey::Space, Some(" ")), 0x20),
        ];
        for (k, ks) in keys {
            assert_eq!(
                typed(vec![vec![super_left(), k]]),
                [Out::Down(XK_SUPER_L), Out::Down(ks), Out::Up(ks), Out::Up(XK_SUPER_L)],
                "{ks:#x}"
            );
        }
    }

    #[test]
    fn alt_left_is_the_mac_option_key() {
        assert_eq!(typed(vec![vec![Ev::modifier(AltLeft, NamedKey::Alt)]]), [Out::Down(XK_META_L), Out::Up(XK_META_L)]);
    }

    #[test]
    fn swap_ctrl_cmd_sends_ctrl_as_command() {
        let mut kb = Keyboard::new(true);
        let ctrl = Ev::modifier(ControlLeft, NamedKey::Control);
        assert_eq!(chord(&mut kb, &[ctrl]), [Out::Down(XK_SUPER_L), Out::Up(XK_SUPER_L)]);
    }

    #[test]
    fn ctrl_alt_shift_z_toggles_the_shortcuts_and_never_reaches_the_mac() {
        let combo = vec![
            Ev::modifier(ControlLeft, NamedKey::Control),
            Ev::modifier(AltLeft, NamedKey::Alt),
            Ev::modifier(ShiftLeft, NamedKey::Shift),
            Ev::text(KeyZ, "z", "Z"),
        ];
        let out = typed(vec![combo]);
        assert_eq!(out[..3], [Out::Down(XK_CONTROL_L), Out::Down(XK_META_L), Out::Down(XK_SHIFT_L)]);
        let mut released: Vec<u32> = out[3..6].iter().map(|o| if let Out::Up(ks) = o { *ks } else { 0 }).collect();
        released.sort();
        assert_eq!(released, [XK_SHIFT_L, XK_CONTROL_L, XK_META_L], "the modifiers go up at once");
        assert_eq!(out[6..], [Out::ToggleShortcuts], "and nothing after it, no Z and no second release");
    }

    #[test]
    fn a_release_matches_its_press_after_the_modifiers_change() {
        let mut kb = Keyboard::new(false);
        let (a, alt_a) = (ch("a"), ch("æ"));
        let input = |down, logical, mods| KeyInput { code: KeyA, down, logical, base: &a, text: None, mods };
        assert_eq!(kb.key(&input(true, &a, ModifiersState::empty())), [Out::Down('a' as u32)]);
        assert_eq!(kb.key(&input(false, &alt_a, ModifiersState::SHIFT)), [Out::Up('a' as u32)]);
    }

    #[test]
    fn maps_key_names() {
        assert_eq!(by_name("f"), Some(0x66));
        assert_eq!(by_name("cmd"), Some(XK_SUPER_L));
        assert_eq!(by_name("ctrl"), Some(XK_CONTROL_L));
        assert_eq!(by_name("opt"), Some(XK_META_L));
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
    fn maps_characters_to_keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
    }
}
