//! Display scaling.
//!
//! Everything wincrust returns is a coordinate, so the process must opt in to
//! per-monitor DPI awareness before it reads one. Without it Windows reports a
//! virtualised coordinate space to the process: on a 150% display a control at
//! physical x=1500 is reported at x=1000, capture comes back scaled, and the
//! bounds guard still passes because it compares two consistently-wrong numbers
//! against each other.
//!
//! `PER_MONITOR_AWARE_V2` rather than `SYSTEM_AWARE` because a machine can have
//! displays at different scale factors, and system awareness is only correct on
//! whichever one Windows picked at startup.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Display {
    pub index: usize,
    /// Screen coordinates. The left/top of a secondary monitor placed to the
    /// left of the primary is NEGATIVE - hence i32 throughout.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
    pub dpi: u32,
    /// 1.0 at 96 DPI, 1.5 at 150%.
    pub scale: f64,
}

#[cfg(windows)]
pub fn declare_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // Fails harmlessly if a manifest already set it, or on an OS too old to
    // know the V2 context - both leave us no worse off than before.
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            tracing::debug!("per-monitor DPI awareness not applied: {e}");
        }
    }
}

/// macOS has no DPI awareness to declare, but it does have an initialisation
/// this has to happen before too.
///
/// ScreenCaptureKit's desktop-independent *window* filter reaches the window
/// server, and a process that has never touched CoreGraphics aborts inside it
/// with `CGS_REQUIRE_INIT` rather than returning an error. `serve` happened to
/// be safe only because the emergency-stop watcher reads the cursor every
/// 100ms and initialises CoreGraphics as a side effect; the one-shot CLI never
/// starts that watcher, so `observe --hwnd` and `find-text --hwnd` aborted.
///
/// Worse than a crash: the abort leaves the capture daemon holding a stream for
/// a dead client, and every later wincrust process then blocks until the
/// strays are killed. So the cheapest documented CoreGraphics call is made here,
/// at the one point every command passes through, rather than relying on an
/// unrelated safety thread to have run first.
#[cfg(target_os = "macos")]
pub fn declare_awareness() {
    let _ = objc2_core_graphics::CGMainDisplayID();
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn declare_awareness() {}

/// What awareness the process ended up with.
///
/// Reported rather than assumed: `declare_awareness` fails silently by design,
/// and a claim that scaling is handled is worth nothing if nothing checks the
/// call took effect. On an unscaled display this is the only evidence available.
#[cfg(windows)]
pub fn awareness() -> &'static str {
    use windows::Win32::UI::HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
        DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    };
    unsafe {
        let a = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext());
        if a == DPI_AWARENESS_PER_MONITOR_AWARE {
            "per-monitor-aware"
        } else if a == DPI_AWARENESS_SYSTEM_AWARE {
            "system-aware (coordinates wrong off the primary display)"
        } else if a == DPI_AWARENESS_UNAWARE {
            "UNAWARE (coordinates virtualised - every reported position is wrong \
             when display scaling is not 100%)"
        } else {
            "unknown"
        }
    }
}

#[cfg(target_os = "macos")]
pub fn awareness() -> &'static str {
    "desktop-points (capture normalized to one pixel per point)"
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn awareness() -> &'static str {
    "n/a"
}

/// Enumerate displays with their real scale factors.
///
/// Exists to make the DPI and multi-monitor assumptions checkable rather than
/// assumed: if this reports `scale: 1.0` on every machine you test, you have
/// not actually tested scaling.
#[cfg(windows)]
pub fn displays() -> anyhow::Result<Vec<Display>> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };
    // Not re-exported by windows-rs 0.62; the value is stable.
    const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<Display>) };
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool() {
            let r = mi.rcMonitor;
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            out.push(Display {
                index: out.len(),
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
                dpi: dx,
                scale: dx as f64 / 96.0,
            });
        }
        TRUE
    }

    let mut out: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut out as *mut Vec<Display> as isize),
        );
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
pub fn displays() -> anyhow::Result<Vec<Display>> {
    use objc2_core_graphics::{
        CGDisplayBounds, CGDisplayCopyDisplayMode, CGGetActiveDisplayList, CGMainDisplayID,
    };
    let mut ids = [0u32; 32];
    let mut count = 0;
    let status = unsafe { CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut count) };
    anyhow::ensure!(status.0 == 0, "display enumeration failed: {status:?}");
    Ok(ids[..count as usize]
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let b = CGDisplayBounds(*id);
            let scale = CGDisplayCopyDisplayMode(*id)
                .map(|m| {
                    objc2_core_graphics::CGDisplayMode::pixel_width(Some(&m)) as f64
                        / objc2_core_graphics::CGDisplayMode::width(Some(&m)) as f64
                })
                .unwrap_or(1.0);
            Display {
                index,
                x: b.origin.x.round() as i32,
                y: b.origin.y.round() as i32,
                width: b.size.width.round() as i32,
                height: b.size.height.round() as i32,
                primary: *id == CGMainDisplayID(),
                dpi: (72.0 * scale).round() as u32,
                scale,
            }
        })
        .collect())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn displays() -> anyhow::Result<Vec<Display>> {
    anyhow::bail!("displays requires Windows")
}
