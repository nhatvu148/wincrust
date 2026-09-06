use crate::keys::*;
use anyhow::{anyhow, ensure, Result};
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGEvent, CGEventFlags, CGScrollEventUnit};

pub struct Key {
    code: u16,
    flags: CGEventFlags,
}

/// This first backend supports US/ABC alphanumeric shortcuts. Reject other
/// layouts explicitly instead of silently sending a different shortcut.
fn us_layout() -> bool {
    use objc2_core_foundation::{CFRetained, CFString, CFType};
    use std::ffi::c_void;
    use std::ptr::NonNull;
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut CFType;
        fn TISGetInputSourceProperty(source: *const CFType, key: &CFString) -> *const c_void;
        static kTISPropertyInputSourceID: &'static CFString;
    }
    unsafe {
        let Some(p) = NonNull::new(TISCopyCurrentKeyboardLayoutInputSource()) else {
            return false;
        };
        let source = CFRetained::from_raw(p);
        let value = TISGetInputSourceProperty(&*source, kTISPropertyInputSourceID).cast::<CFType>();
        let Some(value) = value.as_ref().and_then(|v| v.downcast_ref::<CFString>()) else {
            return false;
        };
        matches!(
            value.to_string().as_str(),
            "com.apple.keylayout.US" | "com.apple.keylayout.ABC"
        )
    }
}

fn code(v: u16) -> Option<u16> {
    Some(match v {
        VK_RETURN => 36,
        VK_TAB => 48,
        VK_SPACE => 49,
        VK_BACK => 51,
        VK_ESCAPE => 53,
        VK_DELETE => 117,
        VK_HOME => 115,
        VK_END => 119,
        VK_PRIOR => 116,
        VK_NEXT => 121,
        VK_LEFT => 123,
        VK_RIGHT => 124,
        VK_DOWN => 125,
        VK_UP => 126,
        VK_OEM_PLUS => 24,
        VK_OEM_MINUS => 27,
        0x41 => 0,
        0x42 => 11,
        0x43 => 8,
        0x44 => 2,
        0x45 => 14,
        0x46 => 3,
        0x47 => 5,
        0x48 => 4,
        0x49 => 34,
        0x4a => 38,
        0x4b => 40,
        0x4c => 37,
        0x4d => 46,
        0x4e => 45,
        0x4f => 31,
        0x50 => 35,
        0x51 => 12,
        0x52 => 15,
        0x53 => 1,
        0x54 => 17,
        0x55 => 32,
        0x56 => 9,
        0x57 => 13,
        0x58 => 7,
        0x59 => 16,
        0x5a => 6,
        0x30 => 29,
        0x31 => 18,
        0x32 => 19,
        0x33 => 20,
        0x34 => 21,
        0x35 => 23,
        0x36 => 22,
        0x37 => 26,
        0x38 => 28,
        0x39 => 25,
        v if (VK_F1..VK_F1 + 20).contains(&v) => [
            122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80,
            90,
        ][(v - VK_F1) as usize],
        _ => return None,
    })
}

pub fn prepare(spec: &str) -> Result<Vec<Key>> {
    let chords = parse(spec)?;
    if chords
        .iter()
        .any(|c| (0x30..=0x5a).contains(&c.key) || c.key == VK_OEM_PLUS || c.key == VK_OEM_MINUS)
    {
        ensure!(us_layout(), "Alphanumeric shortcuts currently require the US or ABC keyboard layout. Unicode type_keys is layout independent.");
    }
    chords
        .into_iter()
        .map(|c| {
            let mut flags = CGEventFlags::empty();
            if c.ctrl {
                flags |= CGEventFlags::MaskControl;
            }
            if c.shift || c.key == VK_OEM_PLUS {
                flags |= CGEventFlags::MaskShift;
            }
            if c.alt {
                flags |= CGEventFlags::MaskAlternate;
            }
            if c.win {
                flags |= CGEventFlags::MaskCommand;
            }
            Ok(Key {
                code: code(c.key).ok_or_else(|| anyhow!("key unsupported on macOS"))?,
                flags,
            })
        })
        .collect()
}

/// The same contract as `input::text_units` on Windows, checked before focus
/// moves anywhere. An empty string is an error rather than a no-op success:
/// silently reporting "ok" for input that was never sent is the one answer a
/// caller cannot act on.
pub fn validate_text(text: &str) -> Result<()> {
    ensure!(!text.is_empty(), "no text to send");
    ensure!(
        text.encode_utf16().count() <= crate::input::MAX_TEXT_UNITS,
        "text exceeds input limit"
    );
    ensure!(!text.contains('\0'), "NUL is not valid keyboard text");
    Ok(())
}

pub fn send_prepared(pid: i32, keys: &[Key]) -> Result<()> {
    ensure!(!crate::guard::engaged(), "{}", crate::guard::refusal());
    // Allocate every event before dispatching any, so allocation errors cannot
    // leave a half-sent chord. Flags are attached to events, not held globally.
    let mut events = Vec::new();
    for key in keys {
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(None, key.code, down)
                .ok_or_else(|| anyhow!("cannot create keyboard event"))?;
            CGEvent::set_flags(Some(&event), key.flags);
            events.push(event);
        }
    }
    for event in events {
        CGEvent::post_to_pid(pid, Some(&event));
    }
    Ok(())
}

pub fn send_text(pid: i32, text: &str) -> Result<()> {
    validate_text(text)?;
    ensure!(!crate::guard::engaged(), "{}", crate::guard::refusal());
    let mut events = Vec::new();
    // One scalar per pair keeps surrogate pairs together and avoids the OS's
    // per-event text length limit without touching the user's clipboard.
    for ch in text.chars() {
        let mut buffer = [0; 2];
        let units = ch.encode_utf16(&mut buffer);
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(None, 0, down)
                .ok_or_else(|| anyhow!("cannot create text event"))?;
            unsafe {
                CGEvent::keyboard_set_unicode_string(
                    Some(&event),
                    units.len() as _,
                    units.as_ptr(),
                );
            }
            events.push(event);
        }
    }
    for event in events {
        CGEvent::post_to_pid(pid, Some(&event));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_keys_are_not_windows_codes() {
        assert_eq!(code(VK_RETURN), Some(36));
        assert_eq!(code(b'S' as u16), Some(1));
        assert_eq!(code(VK_F1 + 23), None);
    }
    #[test]
    fn text_limits_count_surrogates() {
        assert!(validate_text(&"🦀".repeat(1024)).is_ok());
        assert!(validate_text(&"🦀".repeat(1025)).is_err());
        assert!(validate_text("a\0b").is_err());
    }
    /// Windows rejects this in `text_units`; taking focus and then reporting
    /// success for zero keystrokes would be a different answer on each platform.
    #[test]
    fn empty_text_is_refused_like_windows() {
        assert!(validate_text("").is_err());
    }
}

/// One notch of a scroll wheel, in lines. Matches what a physical detent does,
/// so "scroll down 3" means three notches rather than an opaque pixel count.
const LINES_PER_NOTCH: i32 = 3;

/// A parsed scroll request: how far to move, in lines, on each axis.
pub struct Scroll {
    pub vertical: i32,
    pub horizontal: i32,
}

/// Parse `up`, `down`, `left`, `right`, optionally followed by a notch count.
///
/// Deliberately named directions rather than a signed number: the sign
/// convention for a scroll wheel is genuinely ambiguous - "scroll down" moves
/// the content up - and a caller that gets it backwards discovers this by
/// scrolling the wrong way through a document.
pub fn parse_scroll(spec: &str) -> Result<Scroll> {
    let mut parts = spec.split_whitespace();
    let direction = parts
        .next()
        .ok_or_else(|| anyhow!("scroll needs a direction: up, down, left or right"))?
        .to_ascii_lowercase();
    let notches: i32 = match parts.next() {
        None => 1,
        Some(n) => n
            .parse()
            .map_err(|_| anyhow!("scroll count {n:?} is not a number"))?,
    };
    ensure!(
        parts.next().is_none(),
        "scroll takes a direction and an optional count, e.g. \"down 3\""
    );
    ensure!(
        (1..=100).contains(&notches),
        "scroll count must be between 1 and 100"
    );
    let lines = notches * LINES_PER_NOTCH;
    // Positive wheel1 scrolls the view up, which is what "scroll up" means.
    Ok(match direction.as_str() {
        "up" => Scroll {
            vertical: lines,
            horizontal: 0,
        },
        "down" => Scroll {
            vertical: -lines,
            horizontal: 0,
        },
        "left" => Scroll {
            vertical: 0,
            horizontal: lines,
        },
        "right" => Scroll {
            vertical: 0,
            horizontal: -lines,
        },
        other => {
            return Err(anyhow!(
                "unknown scroll direction {other:?}: use up, down, left or right"
            ))
        }
    })
}

/// Send a scroll at a point, to one application.
///
/// The point matters: a scroll wheel event is dispatched to whatever view sits
/// under it, so scrolling a specific pane means placing the event over that
/// pane rather than wherever the user's mouse happens to be.
pub fn send_scroll(pid: i32, at: (i32, i32), scroll: &Scroll) -> Result<()> {
    ensure!(!crate::guard::engaged(), "{}", crate::guard::refusal());
    let event = CGEvent::new_scroll_wheel_event2(
        None,
        CGScrollEventUnit::Line,
        2,
        scroll.vertical,
        scroll.horizontal,
        0,
    )
    .ok_or_else(|| anyhow!("cannot create scroll event"))?;
    CGEvent::set_location(Some(&event), CGPoint::new(f64::from(at.0), f64::from(at.1)));
    CGEvent::post_to_pid(pid, Some(&event));
    Ok(())
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    #[test]
    fn a_bare_direction_is_one_notch() {
        let s = parse_scroll("down").unwrap();
        assert_eq!((s.vertical, s.horizontal), (-LINES_PER_NOTCH, 0));
        assert_eq!(parse_scroll("up").unwrap().vertical, LINES_PER_NOTCH);
    }

    /// "down" must move the content the way a user means it, not the way the
    /// wheel axis is signed.
    #[test]
    fn down_and_up_have_opposite_signs() {
        assert!(parse_scroll("down").unwrap().vertical < 0);
        assert!(parse_scroll("up").unwrap().vertical > 0);
        assert!(parse_scroll("right").unwrap().horizontal < 0);
        assert!(parse_scroll("left").unwrap().horizontal > 0);
    }

    #[test]
    fn a_count_multiplies_notches() {
        assert_eq!(
            parse_scroll("down 4").unwrap().vertical,
            -4 * LINES_PER_NOTCH
        );
    }

    #[test]
    fn it_refuses_what_it_cannot_mean() {
        for bad in [
            "", "sideways", "down 0", "down 101", "down -2", "down two", "down 3 4",
        ] {
            assert!(parse_scroll(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn direction_is_case_insensitive() {
        assert_eq!(parse_scroll("DOWN").unwrap().vertical, -LINES_PER_NOTCH);
    }
}
