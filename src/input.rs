//! winit keys -> `nvim_input` notation (`a`, `<lt>`, `<C-x>`, `<S-Tab>`, ...).

use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

impl From<ModifiersState> for Mods {
    fn from(m: ModifiersState) -> Self {
        Self {
            ctrl: m.control_key(),
            shift: m.shift_key(),
            alt: m.alt_key(),
            logo: m.super_key(),
        }
    }
}

impl Mods {
    fn prefix(self, with_shift: bool) -> String {
        let mut p = String::new();
        if self.ctrl {
            p.push_str("C-");
        }
        if self.shift && with_shift {
            p.push_str("S-");
        }
        if self.alt {
            p.push_str("A-");
        }
        if self.logo {
            p.push_str("D-");
        }
        p
    }

    pub fn mouse(self) -> String {
        let mut m = String::new();
        if self.ctrl {
            m.push('C');
        }
        if self.shift {
            m.push('S');
        }
        if self.alt {
            m.push('A');
        }
        m
    }
}

// TODO: no IME / emoji panel support, winit's Ime events never get here

/// `text` already has layout, Shift, AltGr and dead keys applied.
/// None for Shift, CapsLock, a pending dead key etc.
pub fn translate(key: &Key, text: Option<&str>, mods: Mods) -> Option<String> {
    match key {
        Key::Named(NamedKey::Space) => Some(character(' ', Some(" "), mods)),
        Key::Named(named) => {
            let name = named_key(*named)?;
            Some(format!("<{}{name}>", mods.prefix(true)))
        }
        Key::Character(s) => {
            let c = s.chars().next()?;
            Some(character(c, text, mods))
        }
        // VK_PACKET on Windows: a Unicode char with no physical key behind it
        // (keyboard tools, injected input, and sometimes a space typed while the
        // previous letter is still down). Only the text counts.
        Key::Unidentified(_) => {
            let text = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control))?;
            let altgr = mods.ctrl && mods.alt && !mods.logo;
            (altgr || !(mods.ctrl || mods.alt || mods.logo)).then(|| escape_text(text))
        }
        _ => None,
    }
}

fn character(key_char: char, text: Option<&str>, mods: Mods) -> String {
    let printable = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
    // AltGr shows up as Ctrl+Alt on Windows. If it produced a char (`@`, `#`,
    // `[`, `€` on an Italian layout) send that, not <C-A-...>.
    let altgr = mods.ctrl && mods.alt && !mods.logo;
    if let Some(text) = printable
        && (altgr || !(mods.ctrl || mods.alt || mods.logo))
    {
        return escape_text(text);
    }
    let prefix = mods.prefix(key_char == ' ');
    if prefix.is_empty() {
        return escape_text(key_char.encode_utf8(&mut [0; 4]));
    }
    // Shift is already baked into the char (`A` not `a`), except for space
    let name = match key_char {
        '<' => "lt".to_string(),
        '\\' => "Bslash".to_string(),
        '|' => "Bar".to_string(),
        ' ' => "Space".to_string(),
        c => c.to_string(),
    };
    format!("<{prefix}{name}>")
}

fn escape_text(text: &str) -> String {
    text.replace('<', "<lt>")
}

fn named_key(key: NamedKey) -> Option<&'static str> {
    use NamedKey::*;
    Some(match key {
        Enter => "CR",
        Tab => "Tab",
        Backspace => "BS",
        Escape => "Esc",
        Delete => "Del",
        Insert => "Insert",
        Home => "Home",
        End => "End",
        PageUp => "PageUp",
        PageDown => "PageDown",
        ArrowUp => "Up",
        ArrowDown => "Down",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        F13 => "F13",
        F14 => "F14",
        F15 => "F15",
        F16 => "F16",
        F17 => "F17",
        F18 => "F18",
        F19 => "F19",
        F20 => "F20",
        F21 => "F21",
        F22 => "F22",
        F23 => "F23",
        F24 => "F24",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        ctrl: false,
        shift: false,
        alt: false,
        logo: false,
    };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const SHIFT: Mods = Mods {
        shift: true,
        ..NONE
    };
    const ALT: Mods = Mods { alt: true, ..NONE };
    const ALTGR: Mods = Mods {
        ctrl: true,
        alt: true,
        ..NONE
    };

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    fn t(key: Key, text: Option<&str>, mods: Mods) -> Option<String> {
        translate(&key, text, mods)
    }

    #[test]
    fn plain_text() {
        assert_eq!(t(ch("a"), Some("a"), NONE).as_deref(), Some("a"));
        assert_eq!(t(ch("A"), Some("A"), SHIFT).as_deref(), Some("A"));
        assert_eq!(t(ch("è"), Some("è"), NONE).as_deref(), Some("è"));
        assert_eq!(t(ch("<"), Some("<"), NONE).as_deref(), Some("<lt>"));
        assert_eq!(
            t(Key::Named(NamedKey::Space), Some(" "), NONE).as_deref(),
            Some(" ")
        );
    }

    #[test]
    fn ctrl_and_alt() {
        // Ctrl gives a control char as text on Windows, so use the key
        assert_eq!(t(ch("x"), Some("\u{18}"), CTRL).as_deref(), Some("<C-x>"));
        assert_eq!(t(ch("x"), None, CTRL).as_deref(), Some("<C-x>"));
        assert_eq!(
            t(
                ch("X"),
                None,
                Mods {
                    shift: true,
                    ..CTRL
                }
            )
            .as_deref(),
            Some("<C-X>")
        );
        assert_eq!(t(ch("j"), Some("j"), ALT).as_deref(), Some("<A-j>"));
        assert_eq!(t(ch("<"), None, CTRL).as_deref(), Some("<C-lt>"));
        assert_eq!(
            t(Key::Named(NamedKey::Space), Some(" "), CTRL).as_deref(),
            Some("<C-Space>")
        );
        assert_eq!(
            t(Key::Named(NamedKey::Space), Some(" "), SHIFT).as_deref(),
            Some(" ")
        );
    }

    #[test]
    fn altgr_produces_text() {
        assert_eq!(t(ch("@"), Some("@"), ALTGR).as_deref(), Some("@"));
        assert_eq!(t(ch("["), Some("["), ALTGR).as_deref(), Some("["));
        // no char produced: still a Ctrl+Alt combo
        assert_eq!(t(ch("a"), None, ALTGR).as_deref(), Some("<C-A-a>"));
    }

    #[test]
    fn unidentified_keys_use_their_text() {
        use winit::keyboard::NativeKey;
        // VK_PACKET (0xE7), e.g. a space with no recognized key
        let packet = Key::Unidentified(NativeKey::Windows(0xE7));
        assert_eq!(t(packet.clone(), Some(" "), NONE).as_deref(), Some(" "));
        assert_eq!(t(packet.clone(), Some("<"), NONE).as_deref(), Some("<lt>"));
        assert_eq!(t(packet.clone(), None, NONE), None);
        assert_eq!(t(packet, Some("x"), CTRL), None);
    }

    #[test]
    fn named_keys() {
        assert_eq!(
            t(Key::Named(NamedKey::Enter), Some("\r"), NONE).as_deref(),
            Some("<CR>")
        );
        assert_eq!(
            t(Key::Named(NamedKey::Tab), None, SHIFT).as_deref(),
            Some("<S-Tab>")
        );
        assert_eq!(
            t(Key::Named(NamedKey::ArrowLeft), None, CTRL).as_deref(),
            Some("<C-Left>")
        );
        assert_eq!(
            t(Key::Named(NamedKey::Escape), None, NONE).as_deref(),
            Some("<Esc>")
        );
        assert_eq!(
            t(Key::Named(NamedKey::F5), None, NONE).as_deref(),
            Some("<F5>")
        );
        assert_eq!(t(Key::Named(NamedKey::Shift), None, SHIFT), None);
    }
}
