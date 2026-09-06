//! Emergency stop.
//!
//! An elevated automation server reachable over the network needs a halt that
//! does not depend on the network, the model, or the MCP client being
//! responsive. Parking the physical mouse in the top-left corner is that halt:
//! it needs no keyboard focus, no window, and no cooperation from whatever is
//! currently running.
//!
//! The latch matters as much as the trip. If the stop released the instant the
//! cursor moved, nudging the mouse away would immediately re-arm the agent -
//! so it stays engaged for a cooldown after the corner is vacated, which is
//! long enough to actually take control back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How close to (0,0) counts as "parked".
const CORNER_PX: i32 = 10;
/// How long it must stay parked before the stop engages.
const HOLD: Duration = Duration::from_millis(500);
/// How long the stop stays engaged after the corner is vacated.
const COOLDOWN: Duration = Duration::from_secs(30);

static ENGAGED: AtomicBool = AtomicBool::new(false);

pub fn engaged() -> bool {
    ENGAGED.load(Ordering::Relaxed)
}

/// The message tools return when the stop is engaged. Deliberately explicit
/// about how to release it - a halted agent should not have to guess.
pub fn refusal() -> String {
    "EMERGENCY STOP engaged: the mouse is parked in the top-left corner. \
     All input is refused. Move the cursor away and wait 30s to resume."
        .to_string()
}

/// The stop's decision logic, extracted from the thread that drives it.
///
/// A kill switch whose behaviour can only be exercised by physically moving a
/// mouse is a kill switch nobody tests. Everything that decides *when* the stop
/// engages and releases lives here, as a pure state machine over ticks.
#[derive(Debug, Default)]
pub struct EstopState {
    parked_ms: u64,
    engaged: bool,
    release_at_ms: u64,
}

impl EstopState {
    /// Advance one tick. Returns whether the stop is engaged afterwards.
    pub fn step(&mut self, now_ms: u64, tick_ms: u64, in_corner: bool) -> bool {
        if in_corner {
            self.parked_ms = self.parked_ms.saturating_add(tick_ms);
            if self.parked_ms >= HOLD.as_millis() as u64 {
                self.engaged = true;
                // Refreshed every tick the cursor stays parked, so the cooldown
                // counts from when it *leaves*, not from when it arrived.
                self.release_at_ms = now_ms.saturating_add(COOLDOWN.as_millis() as u64);
            }
        } else {
            self.parked_ms = 0;
            if self.engaged && now_ms >= self.release_at_ms {
                self.engaged = false;
            }
        }
        self.engaged
    }
}

#[cfg(windows)]
pub fn spawn_watcher() {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    std::thread::Builder::new()
        .name("wincrust-estop".into())
        .spawn(|| {
            let tick = Duration::from_millis(100);
            let tick_ms = tick.as_millis() as u64;
            let mut now_ms: u64 = 0;
            let mut state = EstopState::default();
            let mut was = false;
            loop {
                std::thread::sleep(tick);
                now_ms += tick_ms;

                let mut pt = POINT::default();
                let in_corner = unsafe { GetCursorPos(&mut pt) }.is_ok()
                    && pt.x.abs() <= CORNER_PX
                    && pt.y.abs() <= CORNER_PX;

                let engaged = state.step(now_ms, tick_ms, in_corner);
                if engaged != was {
                    if engaged {
                        tracing::warn!("EMERGENCY STOP engaged (cursor parked at origin)");
                    } else {
                        tracing::warn!("emergency stop released");
                    }
                    was = engaged;
                }
                ENGAGED.store(engaged, Ordering::Relaxed);
            }
        })
        .expect("spawn estop watcher");
}

#[cfg(target_os = "macos")]
pub fn spawn_watcher() {
    std::thread::Builder::new()
        .name("wincrust-estop".into())
        .spawn(|| {
            let start = std::time::Instant::now();
            let mut state = EstopState::default();
            let mut was = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let event = objc2_core_graphics::CGEvent::new(None);
                // Failure to read the cursor disables input rather than removing
                // the user's emergency stop without notice. This is the one place
                // macOS deliberately differs from Windows, which treats an
                // unreadable cursor as "not in the corner" and keeps running.
                let Some(event) = event else {
                    if !was {
                        tracing::warn!("EMERGENCY STOP engaged (cursor position unreadable)");
                        was = true;
                    }
                    ENGAGED.store(true, Ordering::Relaxed);
                    continue;
                };
                let p = objc2_core_graphics::CGEvent::location(Some(&event));
                let corner = p.x.abs() <= f64::from(CORNER_PX) && p.y.abs() <= f64::from(CORNER_PX);
                let engaged = state.step(start.elapsed().as_millis() as u64, 100, corner);
                // A safety latch that engages without saying so leaves an
                // operator staring at a server that has silently stopped.
                if engaged != was {
                    if engaged {
                        tracing::warn!("EMERGENCY STOP engaged (cursor parked at origin)");
                    } else {
                        tracing::warn!("emergency stop released");
                    }
                    was = engaged;
                }
                ENGAGED.store(engaged, Ordering::Relaxed);
            }
        })
        .expect("spawn estop watcher");
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn spawn_watcher() {}

/// Apps `launch` is permitted to start.
///
/// Three failure-closed properties. An absent file permits nothing; a file we
/// cannot label permits nothing; and the label is applied to the *same open
/// file object* the contents are then read from.
///
/// That last one is not pedantry. Labelling by path and then re-opening by path
/// leaves a window in which a Medium-integrity process deletes the file and
/// drops in a replacement - the label protects a file that is no longer there,
/// and the unlabelled replacement is trusted anyway. Holding one handle across
/// both operations closes it.
pub fn load_allowlist() -> Vec<String> {
    if cfg!(target_os = "macos") {
        return Vec::new();
    }
    let path = allowlist_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        // Best effort: a labelled directory stops a lower-integrity process
        // replacing the file wholesale rather than editing it.
        let _ = protect_dir(dir);
    }
    if !path.exists() {
        return Vec::new();
    }
    match read_protected(&path) {
        Ok(s) => parse_allowlist(&s),
        Err(e) => {
            tracing::error!(
                "refusing to load the launch allowlist at {} ({e}). `launch` will permit nothing.",
                path.display()
            );
            Vec::new()
        }
    }
}

/// Entries are lower-cased for case-insensitive matching, blank lines and
/// `#` comments dropped.
pub fn parse_allowlist(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn allowlist_path() -> std::path::PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("wincrust")
        .join("launch-allowlist.txt")
}

/// SDDL for "no write up" at High integrity, as a SACL.
#[cfg(windows)]
unsafe fn high_integrity_sacl() -> anyhow::Result<(
    *mut windows::Win32::Security::ACL,
    windows::Win32::Security::PSECURITY_DESCRIPTOR,
)> {
    use windows::core::{BOOL, PCWSTR};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{GetSecurityDescriptorSacl, ACL, PSECURITY_DESCRIPTOR};

    // ML = mandatory label, NW = no write up, HI = high integrity.
    let sddl: Vec<u16> = "S:(ML;;NW;;;HI)\0".encode_utf16().collect();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
        PCWSTR(sddl.as_ptr()),
        SDDL_REVISION_1,
        &mut psd,
        None,
    )?;
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let (mut present, mut defaulted) = (BOOL(0), BOOL(0));
    GetSecurityDescriptorSacl(psd, &mut present, &mut sacl, &mut defaulted)?;
    Ok((sacl, psd))
}

/// Open once, stamp the label on that handle, read from that same handle.
#[cfg(windows)]
fn read_protected(path: &std::path::Path) -> anyhow::Result<String> {
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::Authorization::{SetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::LABEL_SECURITY_INFORMATION;

    // WRITE_OWNER is what a mandatory label actually needs, and neither
    // GENERIC_READ nor GENERIC_WRITE implies it - opening read+write yields
    // ACCESS_DENIED from SetSecurityInfo. Ask for it explicitly.
    const FILE_GENERIC_READ: u32 = 0x0012_0089;
    const WRITE_OWNER: u32 = 0x0008_0000;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_GENERIC_READ | WRITE_OWNER)
        .open(path)?;
    unsafe {
        let (sacl, _psd) = high_integrity_sacl()?;
        let rc = SetSecurityInfo(
            HANDLE(f.as_raw_handle()),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            Some(sacl),
        );
        if rc.is_err() {
            anyhow::bail!(
                "cannot apply a High mandatory label ({rc:?}). A process cannot set a label \
                 above its own integrity level - if this server is not running elevated, that \
                 is the cause, and `launch` is correctly refusing to trust an allowlist it \
                 cannot protect."
            );
        }
    }
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

#[cfg(windows)]
fn protect_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::LABEL_SECURITY_INFORMATION;

    unsafe {
        let (sacl, _psd) = high_integrity_sacl()?;
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        let rc = SetNamedSecurityInfoW(
            PCWSTR(wide.as_mut_ptr()),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            Some(sacl),
        );
        if rc.is_err() {
            anyhow::bail!("SetNamedSecurityInfoW on {}: {rc:?}", dir.display());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn read_protected(_p: &std::path::Path) -> anyhow::Result<String> {
    anyhow::bail!("mandatory labels are Windows-only")
}

#[cfg(not(windows))]
fn protect_dir(_p: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("mandatory labels are Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_ignores_comments_and_blanks_and_folds_case() {
        let got = parse_allowlist("# a comment\n\n  Notepad  \nCALC\n\t# indented comment\n");
        assert_eq!(got, vec!["notepad", "calc"]);
    }

    #[test]
    fn empty_allowlist_permits_nothing() {
        assert!(parse_allowlist("").is_empty());
        assert!(parse_allowlist("# only comments\n\n").is_empty());
    }

    const TICK: u64 = 100;

    /// Engages only after the hold, not on the first tick in the corner - a
    /// cursor passing through the origin must not halt an agent.
    #[test]
    fn estop_requires_the_full_hold() {
        let mut s = EstopState::default();
        let mut t = 0;
        for _ in 0..4 {
            t += TICK;
            assert!(!s.step(t, TICK, true), "engaged early at {t}ms");
        }
        t += TICK;
        assert!(s.step(t, TICK, true), "not engaged after 500ms parked");
    }

    #[test]
    fn estop_resets_the_hold_when_the_cursor_leaves() {
        let mut s = EstopState::default();
        let mut t = 0;
        for _ in 0..4 {
            t += TICK;
            s.step(t, TICK, true);
        }
        t += TICK;
        assert!(!s.step(t, TICK, false));
        for _ in 0..4 {
            t += TICK;
            assert!(!s.step(t, TICK, true), "hold not reset");
        }
    }

    /// The latch is the point: if the stop released the moment the cursor moved,
    /// nudging the mouse would instantly re-arm the agent.
    #[test]
    fn estop_stays_latched_through_the_cooldown_then_releases() {
        let mut s = EstopState::default();
        let mut t = 0;
        for _ in 0..5 {
            t += TICK;
            s.step(t, TICK, true);
        }
        assert!(s.step(t, TICK, true));

        let left_at = t;
        t += TICK;
        assert!(s.step(t, TICK, false), "released immediately on leaving");

        t = left_at + COOLDOWN.as_millis() as u64 - TICK;
        assert!(
            s.step(t, TICK, false),
            "released before the cooldown elapsed"
        );

        t = left_at + COOLDOWN.as_millis() as u64 + TICK;
        assert!(!s.step(t, TICK, false), "still latched after the cooldown");
    }

    /// Re-parking during the cooldown must extend it, not let it lapse.
    #[test]
    fn estop_cooldown_restarts_if_the_cursor_returns() {
        let mut s = EstopState::default();
        let mut t = 0;
        for _ in 0..5 {
            t += TICK;
            s.step(t, TICK, true);
        }
        t += TICK;
        s.step(t, TICK, false);

        t += COOLDOWN.as_millis() as u64 / 2;
        for _ in 0..5 {
            t += TICK;
            s.step(t, TICK, true);
        }
        let re_left = t;
        t = re_left + COOLDOWN.as_millis() as u64 - TICK;
        assert!(s.step(t, TICK, false), "cooldown did not restart");
    }
}
