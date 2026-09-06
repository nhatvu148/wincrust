//! Parsing key specifications into virtual-key chords.
//!
//! `act` could set a text field's value long before it could press Enter, and
//! a console prompt is a text field *plus* Enter. Field use hit that wall
//! twice in one session and fell back to `SendKeys` both times, which is the
//! outcome this crate exists to avoid: a synthetic keystroke from PowerShell
//! carries none of the identity guarantees a resolved element does.
//!
//! Virtual-key codes are written as plain constants rather than pulled from
//! the `windows` crate, so the parser - which is where the bugs actually live -
//! compiles and tests on any host.

use anyhow::{anyhow, Result};

pub const VK_BACK: u16 = 0x08;
pub const VK_TAB: u16 = 0x09;
pub const VK_RETURN: u16 = 0x0D;
#[cfg(any(windows, test))]
pub const VK_SHIFT: u16 = 0x10;
#[cfg(any(windows, test))]
pub const VK_CONTROL: u16 = 0x11;
#[cfg(any(windows, test))]
pub const VK_MENU: u16 = 0x12; // Alt
pub const VK_ESCAPE: u16 = 0x1B;
pub const VK_SPACE: u16 = 0x20;
pub const VK_PRIOR: u16 = 0x21; // Page Up
pub const VK_NEXT: u16 = 0x22; // Page Down
pub const VK_END: u16 = 0x23;
pub const VK_HOME: u16 = 0x24;
pub const VK_LEFT: u16 = 0x25;
pub const VK_UP: u16 = 0x26;
pub const VK_RIGHT: u16 = 0x27;
pub const VK_DOWN: u16 = 0x28;
pub const VK_DELETE: u16 = 0x2E;
#[cfg(any(windows, test))]
pub const VK_LWIN: u16 = 0x5B;
pub const VK_OEM_PLUS: u16 = 0xBB;
pub const VK_OEM_MINUS: u16 = 0xBD;
pub const VK_F1: u16 = 0x70;

/// One keystroke: zero or more held modifiers plus the key they modify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub key: u16,
}

impl Chord {
    /// The modifiers to press before the key and release after, outermost
    /// first. Order matters on release: releasing a modifier before the key it
    /// modifies can deliver a different keystroke to the application.
    #[cfg(any(windows, test))]
    pub fn modifiers(&self) -> Vec<u16> {
        let mut m = Vec::new();
        if self.ctrl {
            m.push(VK_CONTROL);
        }
        if self.alt {
            m.push(VK_MENU);
        }
        if self.shift {
            m.push(VK_SHIFT);
        }
        if self.win {
            m.push(VK_LWIN);
        }
        m
    }
}

fn named(k: &str) -> Option<u16> {
    Some(match k.to_ascii_lowercase().as_str() {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "space" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        // Ctrl++ and Ctrl+- are real shortcuts (zoom), so the keys they name
        // have to exist before the parser can claim to support them.
        "plus" | "+" => VK_OEM_PLUS,
        "minus" | "-" => VK_OEM_MINUS,
        other => {
            // F1-F24 are contiguous from VK_F1.
            let n: u8 = other.strip_prefix('f')?.parse().ok()?;
            if (1..=24).contains(&n) {
                VK_F1 + u16::from(n) - 1
            } else {
                return None;
            }
        }
    })
}

/// Parses one chord, e.g. `Enter`, `Ctrl+S`, `Ctrl+Shift+P`, `F5`, `a`.
pub fn parse_chord(spec: &str) -> Result<Chord> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("empty key specification"));
    }
    let mut c = Chord {
        ctrl: false,
        shift: false,
        alt: false,
        win: false,
        key: 0,
    };
    // Only a DOUBLED trailing '+' is the plus key: "Ctrl++" means ctrl and
    // plus, while "Ctrl+" is a dangling separator and almost certainly a typo.
    // Reading the second as ctrl-plus would turn a mistake into a keystroke.
    // A bare "+" is the plus key on its own.
    let (mods_str, key_str) = if spec == "+" {
        ("", "+")
    } else if let Some(head) = spec.strip_suffix("++") {
        (head, "+")
    } else {
        match spec.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", spec),
        }
    };
    // An entirely empty modifier section means there were none. An empty
    // segment *within* one is still an error, so "Ctrl++Shift" is refused.
    let mods: Vec<&str> = if mods_str.trim().is_empty() {
        Vec::new()
    } else {
        mods_str.split('+').collect()
    };
    for m in &mods {
        match m.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => c.ctrl = true,
            "shift" => c.shift = true,
            "alt" | "option" | "opt" => c.alt = true,
            "win" | "meta" | "cmd" | "command" | "super" => c.win = true,
            "" => return Err(anyhow!("empty modifier in {spec:?}")),
            other => return Err(anyhow!("unknown modifier {other:?} in {spec:?}")),
        }
    }
    let k = key_str.trim();
    if k.is_empty() {
        return Err(anyhow!("{spec:?} has modifiers but no key"));
    }
    c.key = match named(k) {
        Some(v) => v,
        None => {
            let mut ch = k.chars();
            let (first, rest) = (ch.next(), ch.next());
            match (first, rest) {
                // Single ASCII alphanumerics map onto their own VK code.
                (Some(x), None) if x.is_ascii_alphanumeric() => x.to_ascii_uppercase() as u16,
                // A lone non-alphanumeric character is the common case here:
                // someone trying to type a path. It has no layout-independent
                // virtual-key code - ':' is Shift+VK_OEM_1 on US and elsewhere
                // on JIS or AZERTY - so guessing one would type the wrong
                // character into a path rather than fail. Point at the action
                // that sends characters instead of key positions.
                (Some(x), None) if !x.is_ascii_alphanumeric() => {
                    return Err(anyhow!(
                        "{k:?} has no virtual-key code that is the same on every keyboard layout, \
                         so `key` will not guess one. Use action 'type_keys' to send it as text."
                    ))
                }
                _ => return Err(anyhow!("unknown key {k:?} in {spec:?}")),
            }
        }
    };
    Ok(c)
}

/// Parses a whitespace-separated sequence, e.g. `Ctrl+A Delete` or
/// `Home Shift+End Ctrl+C`. Order is preserved.
pub fn parse(spec: &str) -> Result<Vec<Chord>> {
    let chords: Result<Vec<Chord>> = spec.split_whitespace().map(parse_chord).collect();
    let chords = chords?;
    if chords.is_empty() {
        return Err(anyhow!("no keys in {spec:?}"));
    }
    // A cap, so one call cannot become an unbounded stream of synthetic input.
    if chords.len() > 32 {
        return Err(anyhow!(
            "{} keystrokes in one call; 32 is the limit",
            chords.len()
        ));
    }
    Ok(chords)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(s: &str) -> u16 {
        parse_chord(s).unwrap().key
    }

    #[test]
    fn the_case_that_forced_a_sendkeys_fallback() {
        // A console prompt is a text field plus Enter. `act type` could do the
        // first half and nothing could do the second.
        let c = parse_chord("Enter").unwrap();
        assert_eq!(c.key, VK_RETURN);
        assert!(!c.ctrl && !c.shift && !c.alt && !c.win);
    }

    #[test]
    fn modifiers_combine() {
        let c = parse_chord("Ctrl+Shift+P").unwrap();
        assert!(c.ctrl && c.shift);
        assert!(!c.alt && !c.win);
        assert_eq!(c.key, b'P' as u16);
    }

    #[test]
    fn names_are_case_insensitive_and_aliased() {
        for s in ["Enter", "enter", "RETURN", "Return"] {
            assert_eq!(key_of(s), VK_RETURN, "{s}");
        }
        assert_eq!(key_of("esc"), key_of("Escape"));
        assert_eq!(key_of("pgdn"), key_of("PageDown"));
        assert_eq!(key_of("del"), key_of("Delete"));
    }

    #[test]
    fn function_keys_are_contiguous() {
        assert_eq!(key_of("F1"), VK_F1);
        assert_eq!(key_of("F5"), VK_F1 + 4);
        assert_eq!(key_of("F24"), VK_F1 + 23);
        assert!(parse_chord("F25").is_err());
        assert!(parse_chord("F0").is_err());
    }

    #[test]
    fn single_characters_map_to_their_own_code() {
        assert_eq!(key_of("a"), b'A' as u16);
        assert_eq!(key_of("A"), b'A' as u16);
        assert_eq!(key_of("7"), b'7' as u16);
    }

    #[test]
    fn a_trailing_plus_is_the_plus_key_not_a_dangling_separator() {
        // "Ctrl++" is how you say ctrl-plus, and a naive split leaves an empty
        // final segment that reads as an empty modifier instead.
        //
        // The previous version of this test asserted `is_err() || ok`, which
        // is true of every possible outcome - a guard against mis-parsing that
        // could not detect mis-parsing. It now asserts the actual behaviour.
        let c = parse_chord("Ctrl++").expect("Ctrl++ must parse");
        assert!(c.ctrl);
        assert_eq!(c.key, VK_OEM_PLUS);

        let bare = parse_chord("+").expect("+ alone must parse");
        assert!(!bare.ctrl && !bare.shift && !bare.alt && !bare.win);
        assert_eq!(bare.key, VK_OEM_PLUS);

        assert_eq!(parse_chord("Ctrl+-").unwrap().key, VK_OEM_MINUS);
        assert_eq!(key_of("plus"), VK_OEM_PLUS);

        // A SINGLE trailing '+' stays an error: "Ctrl+" is a typo far more
        // often than it is an intent, and reading it as ctrl-plus would turn
        // a mistake into a keystroke.
        assert!(parse_chord("Ctrl+").is_err());
    }

    #[test]
    fn an_empty_segment_inside_the_modifiers_is_still_refused() {
        // Tolerating the trailing '+' must not tolerate "Ctrl++Shift".
        assert!(parse_chord("Ctrl++Shift").is_err());
    }

    #[test]
    fn sequences_preserve_order() {
        let seq = parse("Home Shift+End Ctrl+C").unwrap();
        assert_eq!(seq.len(), 3);
        assert_eq!(seq[0].key, VK_HOME);
        assert!(seq[1].shift && seq[1].key == VK_END);
        assert!(seq[2].ctrl && seq[2].key == b'C' as u16);
    }

    /// The two characters a path needs. `key` still refuses them - that is
    /// deliberate, not a gap - but the refusal now names the action that can
    /// send them, because "unknown key" left the field renaming a script to
    /// nine pure letters to get around it.
    #[test]
    fn path_punctuation_is_refused_but_points_somewhere() {
        for bad in [":", "\\", "/", "."] {
            let e = parse_chord(bad)
                .expect_err("{bad:?} must not parse")
                .to_string();
            assert!(e.contains("type_keys"), "{bad:?} said: {e}");
        }
        // Still refused, and still not silently mapped to some other key.
        assert!(parse_chord("Ctrl+:").is_err());
    }

    #[test]
    fn nonsense_is_refused_rather_than_guessed() {
        for bad in ["", "   ", "Ctrl+", "Ctrl+Nope", "Hyper+A", "notakey"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn a_run_of_keystrokes_is_capped() {
        let long = "a ".repeat(40);
        assert!(parse(&long).is_err());
    }

    #[test]
    fn modifiers_are_reported_for_press_and_release() {
        let c = parse_chord("Ctrl+Alt+Shift+Win+S").unwrap();
        let m = c.modifiers();
        assert_eq!(m.len(), 4);
        assert_eq!(m[0], VK_CONTROL, "ctrl must be outermost");
    }
}
