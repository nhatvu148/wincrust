//! Synthetic mouse input.
//!
//! Everything else in this crate acts through UI Automation control patterns:
//! `Invoke()` reaches a control without stealing focus, moving the cursor, or
//! requiring the window to be on top. This module is the exception, and it
//! exists only because OCR returns a rectangle rather than a control - there is
//! no pattern to invoke on a word read off the screen.
//!
//! It is therefore strictly worse than the pattern path and should only run
//! when that path has already failed:
//!
//!   - it moves the user's real cursor
//!   - it clicks whatever is topmost at that point, which may not be the thing
//!     the OCR read if a window moved in between
//!   - it needs the target visible and unobscured
//!
//! Callers opt in explicitly. Nothing here is reachable by default.

use anyhow::{anyhow, Result};

/// Map a screen coordinate into `SendInput`'s normalised 0..65535 space.
///
/// Deliberately a free function rather than a closure inside the unsafe block:
/// it is the only arithmetic here that can be wrong in a way nothing catches,
/// and it is the single place multi-monitor geometry is handled anywhere in
/// this crate. `origin` is negative when a monitor sits left of or above the
/// primary, which is exactly the case no hardware here can exercise - so it is
/// tested instead.
#[cfg(any(windows, test))]
pub(crate) fn to_normalized(v: i32, origin: i32, span: i32) -> i32 {
    (((v - origin) as f64) * 65535.0 / ((span - 1).max(1) as f64)).round() as i32
}

/// Click once at a screen coordinate.
///
/// `SendInput` with `MOUSEEVENTF_ABSOLUTE` addresses the *virtual desktop* in a
/// normalised 0..65535 space, not pixels - so the conversion has to divide by
/// the virtual screen's size and offset by its origin, which is negative when a
/// monitor sits left of or above the primary.
#[cfg(windows)]
pub fn click_at(x: i32, y: i32) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    unsafe {
        let (vx, vy, vw, vh) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        if vw <= 0 || vh <= 0 {
            return Err(anyhow!(
                "virtual screen is {vw}x{vh} - no desktop to click on (session 0?)"
            ));
        }
        if x < vx || y < vy || x >= vx + vw || y >= vy + vh {
            return Err(anyhow!(
                "({x},{y}) is outside the virtual desktop ({vx},{vy} {vw}x{vh})"
            ));
        }

        let (nx, ny) = (to_normalized(x, vx, vw), to_normalized(y, vy, vh));

        let mk = |flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: nx,
                    dy: ny,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // Move, press, release as one batch: sending them separately lets other
        // input interleave between the move and the press, which is how a click
        // lands somewhere the caller never asked for.
        let events = [
            mk(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
            mk(MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
            mk(MOUSEEVENTF_LEFTUP | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
        ];
        let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != events.len() {
            return Err(anyhow!(
                "SendInput accepted {sent} of {} events - blocked by UIPI?",
                events.len()
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn click_at(_x: i32, _y: i32) -> Result<()> {
    Err(anyhow!("synthetic input requires Windows"))
}

/// Sends a sequence of chords as synthetic keyboard input.
///
/// Keyboard input goes to whatever has focus - there is no per-element
/// keyboard equivalent of `InvokePattern`. So the caller must focus the
/// resolved element first, and `act` says so in its result rather than
/// pretending the keystroke was delivered to a control by contract the way a
/// pattern call is.
///
/// Modifiers are released in reverse order: releasing Ctrl before the key it
/// modifies can deliver a different keystroke to the application.
#[cfg(windows)]
pub fn send_keys(chords: &[crate::keys::Chord]) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY,
    };

    let mut events: Vec<INPUT> = Vec::with_capacity(chords.len() * 6);
    let mk = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    for c in chords {
        let mods = c.modifiers();
        for m in &mods {
            events.push(mk(*m, false));
        }
        events.push(mk(c.key, false));
        events.push(mk(c.key, true));
        for m in mods.iter().rev() {
            events.push(mk(*m, true));
        }
    }
    // One batch, so nothing can interleave between a modifier press and the
    // key it modifies.
    let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        anyhow::bail!(
            "SendInput accepted {sent} of {} keyboard events",
            events.len()
        );
    }
    Ok(())
}

/// A run of synthetic text is capped, for the same reason a run of chords is:
/// one call must not become an unbounded stream of input the user cannot stop.
/// Generous enough for a command line or a long path, which is the case this
/// exists to serve.
pub const MAX_TEXT_UNITS: usize = 2048;

/// The UTF-16 code units a string will be sent as, or an error if there are
/// too many.
///
/// Split out and pure because this is where the bug would actually be: a
/// character outside the BMP is *two* code units, and sending only the first
/// delivers a lone surrogate rather than the character. Iterating `chars()`
/// here instead of `encode_utf16()` is the mistake this function exists to
/// make untestable-by-inspection into tested.
#[cfg(any(windows, test))]
pub(crate) fn text_units(s: &str) -> Result<Vec<u16>> {
    if s.is_empty() {
        return Err(anyhow!("no text to send"));
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() > MAX_TEXT_UNITS {
        return Err(anyhow!(
            "{} UTF-16 code units in one call; {MAX_TEXT_UNITS} is the limit",
            units.len()
        ));
    }
    Ok(units)
}

/// Sends a string as synthetic keystrokes, character by character.
///
/// Unlike `send_keys`, this carries no virtual-key code at all: `KEYEVENTF_UNICODE`
/// puts the UTF-16 code unit in `wScan` and leaves `wVk` zero, and Windows
/// delivers that character directly. That is the whole point - a VK code names a
/// *position on a keyboard*, so it is layout-dependent, and `:` is Shift+VK_OEM_1
/// on a US layout and something else on JIS or AZERTY. Typing a file path through
/// VK codes therefore produces the wrong path on a keyboard we did not anticipate,
/// which is worse than refusing. Unicode has no such failure mode.
///
/// It shares every caveat of `send_keys`: it goes wherever focus is, not to a
/// control by contract, and UIPI silently drops it into a window of higher
/// integrity than this process.
#[cfg(windows)]
pub fn send_text(s: &str) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VIRTUAL_KEY,
    };

    let units = text_units(s)?;
    let mut events: Vec<INPUT> = Vec::with_capacity(units.len() * 2);
    let mk = |unit: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: if up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // Surrogate pairs need both halves adjacent and in order, so the units are
    // emitted exactly as `encode_utf16` produced them.
    for u in units {
        events.push(mk(u, false));
        events.push(mk(u, true));
    }
    let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        anyhow::bail!(
            "SendInput accepted {sent} of {} keyboard events - blocked by UIPI? A window running \
             elevated cannot be typed into from this process.",
            events.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{text_units, to_normalized, MAX_TEXT_UNITS};

    #[test]
    fn maps_the_span_to_the_full_range() {
        assert_eq!(to_normalized(0, 0, 1920), 0);
        assert_eq!(to_normalized(1919, 0, 1920), 65535);
    }

    /// The case no display here can produce: a monitor left of the primary puts
    /// the virtual-screen origin at a negative x, and a point on it is negative
    /// too. Both must still land inside 0..65535.
    #[test]
    fn handles_a_negative_virtual_origin() {
        // Two 1920-wide monitors, secondary on the left: origin -1920, span 3840.
        assert_eq!(to_normalized(-1920, -1920, 3840), 0);
        assert_eq!(to_normalized(1919, -1920, 3840), 65535);
        // The primary's left edge sits halfway across the virtual desktop.
        let mid = to_normalized(0, -1920, 3840);
        assert!((32750..=32790).contains(&mid), "midpoint was {mid}");
    }

    #[test]
    fn handles_a_negative_origin_on_the_y_axis() {
        // A monitor above the primary.
        assert_eq!(to_normalized(-1080, -1080, 2160), 0);
        assert_eq!(to_normalized(1079, -1080, 2160), 65535);
    }

    /// A degenerate span must not divide by zero.
    #[test]
    fn survives_a_one_pixel_span() {
        assert_eq!(to_normalized(0, 0, 1), 0);
        assert_eq!(to_normalized(0, 0, 0), 0);
    }

    /// The characters that forced a workaround in the field. A path could not
    /// be typed at all, because `key` maps only alphanumerics onto VK codes -
    /// so a benchmark launcher had to be renamed to nine pure letters and put
    /// on PATH. Through the unicode path they are unremarkable.
    #[test]
    fn punctuation_that_no_vk_code_could_carry() {
        for s in [":", "\\", r"C:\Users\nhatv\.local\bin\uv.exe"] {
            let u = text_units(s).unwrap_or_else(|e| panic!("{s:?} must encode: {e}"));
            assert_eq!(u.len(), s.encode_utf16().count(), "{s:?}");
        }
        // The colon and backslash specifically, since those are the two the
        // parser refuses.
        assert_eq!(text_units(":").unwrap(), vec![b':' as u16]);
        assert_eq!(text_units("\\").unwrap(), vec![b'\\' as u16]);
    }

    /// A character outside the BMP is two UTF-16 code units. Iterating
    /// `chars()` would send one event and deliver a lone surrogate - a
    /// mangled character rather than the one asked for.
    #[test]
    fn an_astral_character_becomes_a_surrogate_pair() {
        let u = text_units("\u{1F600}").unwrap();
        assert_eq!(u.len(), 2, "one char, but two code units");
        assert!((0xD800..0xDC00).contains(&u[0]), "high surrogate first");
        assert!((0xDC00..0xE000).contains(&u[1]), "low surrogate second");

        // And it must stay adjacent to its neighbours' units, in order.
        let mixed = text_units("a\u{1F600}b").unwrap();
        assert_eq!(mixed.len(), 4);
        assert_eq!(mixed[0], b'a' as u16);
        assert_eq!(mixed[3], b'b' as u16);
    }

    #[test]
    fn a_run_of_text_is_capped() {
        let long = "a".repeat(MAX_TEXT_UNITS + 1);
        assert!(text_units(&long).is_err());
        assert!(text_units(&"a".repeat(MAX_TEXT_UNITS)).is_ok());
    }

    #[test]
    fn empty_text_is_refused_rather_than_sent_as_nothing() {
        assert!(text_units("").is_err());
    }

    #[test]
    fn is_monotonic_across_the_span() {
        let mut last = i32::MIN;
        for x in (-1920..1920).step_by(97) {
            let n = to_normalized(x, -1920, 3840);
            assert!(n > last, "not monotonic at {x}");
            last = n;
        }
    }
}
