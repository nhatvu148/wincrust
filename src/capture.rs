//! Screen capture.
//!
//! GDI `BitBlt` rather than DXGI duplication or Windows.Graphics.Capture: it is
//! a few dozen lines, has no device-lost handling, no WinRT dependency, and
//! works identically across sessions and remote desktops. Capture is not the
//! bottleneck here - a UIA tree walk costs an order of magnitude more - so the
//! simpler API wins until measurement says otherwise.

use anyhow::{anyhow, Result};
use image::ImageEncoder;
use schemars::JsonSchema;
use serde::Serialize;
use std::sync::Mutex;

#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub struct Frame {
    pub w: u32,
    pub h: u32,
    /// Screen coordinate of this frame's top-left. NOT always (0,0): a monitor
    /// placed left of or above the primary gives the virtual screen a negative
    /// origin, and a region reported without it points at the wrong place.
    pub origin: (i32, i32),
    /// Row-major RGB, top-down.
    pub rgb: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Observation {
    pub width: u32,
    pub height: u32,
    /// Scale applied before encoding; 1.0 means native.
    pub scale: f64,
    pub png_bytes: usize,
    /// Roughly what the image will cost in context: (w*h)/750.
    pub approx_tokens: u32,
    /// diff mode only: fraction of pixels that changed since the last capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_fraction: Option<f64>,
    /// diff mode only: the changed region in SCREEN coordinates (x and y may be
    /// negative on a multi-monitor desktop), so it can be compared directly
    /// against an entity's `bounds` or `click_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_region: Option<(i32, i32, u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub elapsed_ms: f64,
}

/// Previous frame, kept so `diff` can answer "nothing moved" without sending an
/// image at all - by far the cheapest useful answer during a wait loop.
static LAST: Mutex<Option<Frame>> = Mutex::new(None);

/// Is the interactive desktop reachable, or is the session locked?
///
/// `OpenInputDesktop` fails when the workstation is locked or a secure desktop
/// is up. Worth checking before a capture, because the alternative is worse
/// than an error: a locked session captures the lock-screen wallpaper, which is
/// a perfectly valid image containing no application at all. OCR then returns
/// zero lines and UIA returns nothing useful, and a caller has no way to tell
/// that from "the app has no text" - which is exactly the confusion this cost
/// an hour of debugging.
#[cfg(windows)]
pub fn session_is_locked() -> bool {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    };
    unsafe {
        match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
            Ok(h) => {
                let _ = CloseDesktop(h);
                false
            }
            Err(_) => true,
        }
    }
}

/// Capture one window through `PrintWindow(PW_RENDERFULLCONTENT)`.
///
/// `BitBlt` from the screen DC reads the desktop surface, and hardware
/// accelerated content - OpenGL and Direct3D viewports - is composited by DWM
/// rather than drawn into it. The result is not an error: it is a flat
/// clear-colour rectangle where the 3D content should be, which reads exactly
/// like an empty viewport. Measured on a CAD viewport showing a cone, every
/// canvas pixel came back 207,221,238 while a translucent toolbar strip over
/// the same canvas carried the model's own colour - proof the content reached
/// the compositor and not the read.
///
/// `PW_RENDERFULLCONTENT` asks the window to render itself, that content
/// included. It is the cheap candidate and it is worth having, but it is NOT a
/// general fix and must not be described as one:
///
/// **Measured on the viewport above, it changed nothing.** Both paths returned
/// the same flat 207,221,238 at the same screen points. An application that
/// renders through its own swap chain and never services a `WM_PRINT` for that
/// region is not obliged to produce anything here, and this one does not.
///
/// It does help applications that draw through a path `PrintWindow` can drive,
/// which is why it stays. The honest framing is: try it, do not rely on it. A
/// general fix means capturing the presentation surface itself - DXGI desktop
/// duplication or Windows.Graphics.Capture - which is a different and much
/// larger piece of work.
#[cfg(windows)]
pub fn capture_window(hwnd: isize) -> Result<Frame> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};

    // Stable since 8.1: render the full content, accelerated surfaces included.
    const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

    unsafe {
        let h = HWND(hwnd as *mut _);
        if !IsWindow(Some(h)).as_bool() {
            return Err(anyhow!("window {hwnd} no longer exists"));
        }
        // Same check grab() makes, and for the same reason: a locked session
        // yields a blank or nonsense frame, and "PrintWindow failed" would
        // send the reader looking at the window rather than at the lock.
        if session_is_locked() {
            return Err(anyhow!(
                "the session is locked - a capture here returns the lock screen, not the desktop. \
                 Unlock the machine, or expect UIA and OCR to see nothing."
            ));
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r)?;
        let (w, ht) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || ht <= 0 {
            return Err(anyhow!("window {hwnd} has no area ({w}x{ht})"));
        }

        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(anyhow!("GetDC failed"));
        }
        // Checked rather than assumed: GDI handle exhaustion otherwise
        // surfaces later as "GetDIBits returned no scanlines", which names the
        // wrong call entirely.
        let mem = CreateCompatibleDC(Some(screen));
        if mem.is_invalid() {
            ReleaseDC(None, screen);
            return Err(anyhow!(
                "CreateCompatibleDC failed - GDI handles may be exhausted"
            ));
        }
        let bmp = CreateCompatibleBitmap(screen, w, ht);
        if bmp.is_invalid() {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return Err(anyhow!(
                "CreateCompatibleBitmap failed for {w}x{ht} - GDI handles or memory may be exhausted"
            ));
        }
        let old = SelectObject(mem, bmp.into());

        let ok = PrintWindow(h, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -ht,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (w as usize) * (ht as usize) * 4];
        let rows = GetDIBits(
            mem,
            bmp,
            0,
            ht as u32,
            Some(bgra.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        if !ok.as_bool() {
            return Err(anyhow!(
                "PrintWindow failed for {hwnd} - the window may not support rendering on demand"
            ));
        }
        if rows == 0 {
            return Err(anyhow!("GetDIBits returned no scanlines"));
        }

        let mut rgb = Vec::with_capacity((w as usize) * (ht as usize) * 3);
        let mut i = 0usize;
        while i + 3 < bgra.len() {
            rgb.push(bgra[i + 2]);
            rgb.push(bgra[i + 1]);
            rgb.push(bgra[i]);
            i += 4;
        }
        Ok(Frame {
            w: w as u32,
            h: ht as u32,
            // Window pixels still need screen coordinates, so the origin
            // travels with the frame exactly as for a screen capture and any
            // click target derived from it stays correct.
            origin: (r.left, r.top),
            rgb,
        })
    }
}

#[cfg(not(windows))]
pub fn capture_window(hwnd: isize) -> Result<Frame> {
    #[cfg(target_os = "macos")]
    return crate::macos::capture::capture_window(hwnd);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = hwnd;
        anyhow::bail!("window capture requires Windows or macOS")
    }
}

#[cfg(windows)]
pub fn grab() -> Result<Frame> {
    unsafe {
        let (x, y, w, h) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        if w <= 0 || h <= 0 {
            return Err(anyhow!(
                "virtual screen is {w}x{h} - almost certainly running in session 0, which has no desktop"
            ));
        }
        if session_is_locked() {
            return Err(anyhow!(
                "the session is locked - a capture here returns the lock screen, not the desktop. \
                 Unlock the machine, or expect UIA and OCR to see nothing."
            ));
        }

        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(anyhow!("GetDC failed"));
        }
        let mem = CreateCompatibleDC(Some(screen));
        if mem.is_invalid() {
            ReleaseDC(None, screen);
            return Err(anyhow!(
                "CreateCompatibleDC failed - GDI handles may be exhausted"
            ));
        }
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if bmp.is_invalid() {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return Err(anyhow!(
                "CreateCompatibleBitmap failed for {w}x{h} - GDI handles or memory may be exhausted"
            ));
        }
        let old = SelectObject(mem, bmp.into());

        let blit = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY);

        // Negative height requests a top-down DIB, saving a row flip later.
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
        let rows = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(bgra.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        blit?;
        if rows == 0 {
            return Err(anyhow!("GetDIBits returned no scanlines"));
        }

        // Indexed rather than chunks_exact(4): newer clippy denies a constant
        // chunk size, and the suggested replacements are not stable across the
        // toolchain range this has to build on.
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for i in (0..bgra.len()).step_by(4) {
            rgb.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i]]);
        }
        Ok(Frame {
            w: w as u32,
            h: h as u32,
            origin: (x, y),
            rgb,
        })
    }
}

#[cfg(target_os = "macos")]
pub fn grab() -> Result<Frame> {
    crate::macos::capture::grab()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn grab() -> Result<Frame> {
    Err(anyhow!("capture requires Windows"))
}

/// Bounding box of everything that changed, plus what fraction of the frame it
/// covers. One box, not a region list: a caller wants "look here", and merging
/// scattered boxes into their hull is both cheaper and easier to act on.
fn changed_box(a: &Frame, b: &Frame, tol: u8) -> Option<(u32, u32, u32, u32, f64, u64)> {
    if a.w != b.w || a.h != b.h {
        return Some((0, 0, b.w, b.h, 1.0, (b.w as u64) * (b.h as u64)));
    }
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut changed = 0u64;
    for y in 0..b.h {
        let row = (y as usize) * (b.w as usize) * 3;
        for x in 0..b.w {
            let i = row + (x as usize) * 3;
            let d = (a.rgb[i] as i16 - b.rgb[i] as i16).unsigned_abs()
                + (a.rgb[i + 1] as i16 - b.rgb[i + 1] as i16).unsigned_abs()
                + (a.rgb[i + 2] as i16 - b.rgb[i + 2] as i16).unsigned_abs();
            if d > tol as u16 {
                changed += 1;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if changed == 0 {
        return None;
    }
    let frac = changed as f64 / ((b.w as f64) * (b.h as f64));
    Some((x0, y0, x1 - x0 + 1, y1 - y0 + 1, frac, changed))
}

fn crop(f: &Frame, x: u32, y: u32, w: u32, h: u32) -> Frame {
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for row in y..(y + h).min(f.h) {
        let s = (row as usize) * (f.w as usize) * 3 + (x as usize) * 3;
        let e = s + (w.min(f.w - x) as usize) * 3;
        out.extend_from_slice(&f.rgb[s..e]);
    }
    Frame {
        w,
        h,
        origin: (f.origin.0 + x as i32, f.origin.1 + y as i32),
        rgb: out,
    }
}

fn encode_png(f: &Frame, max_width: u32) -> Result<(Vec<u8>, u32, u32, f64)> {
    let img = image::RgbImage::from_raw(f.w, f.h, f.rgb.clone())
        .ok_or_else(|| anyhow!("frame buffer size does not match {}x{}", f.w, f.h))?;
    let (img, scale) = if max_width > 0 && f.w > max_width {
        let s = max_width as f64 / f.w as f64;
        let nh = ((f.h as f64) * s).round().max(1.0) as u32;
        (
            image::imageops::resize(&img, max_width, nh, image::imageops::FilterType::Triangle),
            s,
        )
    } else {
        (img, 1.0)
    };
    let (w, h) = (img.width(), img.height());
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        img.as_raw(),
        w,
        h,
        image::ExtendedColorType::Rgb8,
    )?;
    Ok((png, w, h, scale))
}

/// `image` mode: whole screen. `diff` mode: only what moved, or nothing at all.
///
/// With `hwnd`, one window is rendered on demand instead of read off the
/// desktop - the only way to see an OpenGL or Direct3D viewport, which DWM
/// composites and a screen read therefore misses entirely.
pub fn observe_bytes(
    diff: bool,
    max_width: u32,
    hwnd: Option<isize>,
) -> Result<(Observation, Vec<u8>)> {
    let t0 = std::time::Instant::now();
    let frame = match hwnd {
        Some(h) => capture_window(h)?,
        None => grab()?,
    };

    let (target, changed_fraction, changed_region, note) =
        if diff {
            let mut last = LAST.lock().map_err(|_| anyhow!("frame lock poisoned"))?;
            match last.as_ref().and_then(|p| changed_box(p, &frame, 12)) {
                None if last.is_some() => {
                    *last = Some(Frame {
                        w: frame.w,
                        h: frame.h,
                        origin: frame.origin,
                        rgb: frame.rgb.clone(),
                    });
                    return Ok((
                        Observation {
                            width: 0,
                            height: 0,
                            scale: 1.0,
                            png_bytes: 0,
                            approx_tokens: 0,
                            changed_fraction: Some(0.0),
                            changed_region: None,
                            note: Some("no change since last observation".into()),
                            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
                        },
                        Vec::new(),
                    ));
                }
                Some((x, y, w, h, frac, changed_px)) => {
                    *last = Some(Frame {
                        w: frame.w,
                        h: frame.h,
                        origin: frame.origin,
                        rgb: frame.rgb.clone(),
                    });
                    // A single hull around scattered changes is mostly unchanged
                    // pixels - a caret in one corner and a clock in the other yields
                    // a near-fullscreen box for a handful of moved pixels. When the
                    // box is that unrepresentative, the coordinates ARE the answer;
                    // sending 259KB to show 200 changed pixels helps nobody.
                    let hull = (w as u64) * (h as u64);
                    if frac < 0.005 && hull > changed_px.saturating_mul(25) {
                        return Ok((
                            Observation {
                                width: 0,
                                height: 0,
                                scale: 1.0,
                                png_bytes: 0,
                                approx_tokens: 0,
                                changed_fraction: Some(frac),
                                changed_region: Some((
                                    frame.origin.0 + x as i32,
                                    frame.origin.1 + y as i32,
                                    w,
                                    h,
                                )),
                                note: Some(format!(
                            "{changed_px} scattered pixels changed ({:.3}%) - image withheld, \
                             the region hull is {}x larger than the change itself",
                            frac * 100.0, hull / changed_px.max(1))),
                                elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
                            },
                            Vec::new(),
                        ));
                    }
                    (
                        crop(&frame, x, y, w, h),
                        Some(frac),
                        Some((frame.origin.0 + x as i32, frame.origin.1 + y as i32, w, h)),
                        None,
                    )
                }
                None => {
                    *last = Some(Frame {
                        w: frame.w,
                        h: frame.h,
                        origin: frame.origin,
                        rgb: frame.rgb.clone(),
                    });
                    (
                        frame,
                        None,
                        None,
                        Some("first observation - full frame".into()),
                    )
                }
            }
        } else {
            (frame, None, None, None)
        };

    let (png, w, h, scale) = encode_png(&target, max_width)?;
    let obs = Observation {
        width: w,
        height: h,
        scale,
        png_bytes: png.len(),
        approx_tokens: (w * h) / 750,
        changed_fraction,
        changed_region,
        note,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
    };
    Ok((obs, png))
}

/// Magnified PNG for recognition, and **the scale actually applied**.
///
/// Returning the effective scale is the point. A caller has to divide the
/// recogniser's coordinates back out, and if it divides by a number this
/// function did not use, every coordinate is silently wrong with no error
/// anywhere - which is what happened when a fractional scale skipped the resize
/// while the caller still divided by it. Handing back the real value makes the
/// two sides impossible to disagree.
///
/// Deliberately does not route through `encode_png`, whose `max_width` only
/// ever shrinks (`f.w > max_width`) - passing a larger width there is a silent
/// no-op, which is exactly how an "upscale" knob came to do nothing at all.
pub fn encode_png_scaled(f: &Frame, scale: f32, prep: Prep) -> Result<(Vec<u8>, f32)> {
    use image::ImageEncoder;
    let img = image::RgbImage::from_raw(f.w, f.h, f.rgb.clone())
        .ok_or_else(|| anyhow!("frame buffer size does not match {}x{}", f.w, f.h))?;
    // Sharpen and threshold BEFORE magnifying: applied afterwards they act on
    // interpolated pixels the recogniser never needed to see.
    let img = preprocess(img, prep);
    if scale <= 1.0 {
        let (w, h) = (img.width(), img.height());
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(
            img.as_raw(),
            w,
            h,
            image::ExtendedColorType::Rgb8,
        )?;
        // 1.0, not the requested value: nothing was resized.
        return Ok((png, 1.0));
    }
    let (w, h) = (
        ((f.w as f32) * scale).round().max(1.0) as u32,
        ((f.h as f32) * scale).round().max(1.0) as u32,
    );
    // Lanczos3 rather than Triangle: upscaling for a recogniser wants sharp
    // glyph edges, and bilinear softens exactly the strokes it needs to read.
    let big = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        big.as_raw(),
        w,
        h,
        image::ExtendedColorType::Rgb8,
    )?;
    // The real ratio, not the request: rounding to whole pixels means a 1.5x
    // ask on an odd-width frame is not exactly 1.5x.
    Ok((png, w as f32 / f.w as f32))
}

/// What to do to the pixels before handing them to a recogniser.
///
/// MEASURED, AND NONE OF IT HELPS. On the Abaqus fixture at 1.5x and 2x:
///
///   preprocess  menu bar  model tree
///   none        6/9       15/15
///   gray        6/9       15/15
///   contrast    6/9       15/15
///   otsu        5/9       15/15   <- worse
///   sharpen     6/9       15/15
///
/// That is not the result classic OCR advice predicts, and the reason is that
/// the advice predates the engine. Windows.Media.Ocr is a modern recogniser
/// trained on natural, anti-aliased imagery; binarisation discards exactly the
/// grey levels it reads, which is why Otsu measurably loses a word.
///
/// Kept behind a CLI flag, defaulting to None, so the next person to wonder
/// whether thresholding would help can re-run the comparison on their own
/// application rather than re-deriving it - a dark theme or a different
/// typeface might yet behave differently. Not exposed over MCP: a knob that
/// measurably does nothing has no business on a tool surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Prep {
    None,
    /// Luminance only. Removes subpixel-antialiasing colour fringes.
    Gray,
    /// Grayscale, then rescale so the darkest pixel is 0 and the lightest 255.
    Contrast,
    /// Grayscale, then a global Otsu threshold to pure black and white.
    Otsu,
    /// Unsharp mask. Sharpens glyph edges without discarding grey levels.
    Sharpen,
}

impl std::str::FromStr for Prep {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "none" => Ok(Prep::None),
            "gray" => Ok(Prep::Gray),
            "contrast" => Ok(Prep::Contrast),
            "otsu" => Ok(Prep::Otsu),
            "sharpen" => Ok(Prep::Sharpen),
            o => Err(format!(
                "unknown preprocess '{o}' (none|gray|contrast|otsu|sharpen)"
            )),
        }
    }
}

fn to_gray(img: &image::RgbImage) -> image::GrayImage {
    image::imageops::grayscale(img)
}

/// Otsu: the threshold that minimises intra-class variance. Standard for
/// document binarisation; measurably wrong for anti-aliased UI text.
fn otsu_threshold(g: &image::GrayImage) -> u8 {
    let mut hist = [0u32; 256];
    for p in g.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u32 = hist.iter().sum();
    let sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, c)| i as f64 * *c as f64)
        .sum();
    let (mut sum_b, mut w_b, mut best, mut best_t) = (0.0f64, 0u32, -1.0f64, 0u8);
    for (t, &count) in hist.iter().enumerate() {
        w_b += count;
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += t as f64 * count as f64;
        let m_b = sum_b / w_b as f64;
        let m_f = (sum - sum_b) / w_f as f64;
        let var = w_b as f64 * w_f as f64 * (m_b - m_f) * (m_b - m_f);
        if var > best {
            best = var;
            best_t = t as u8;
        }
    }
    best_t
}

fn preprocess(img: image::RgbImage, prep: Prep) -> image::RgbImage {
    let gray_to_rgb = |g: image::GrayImage| -> image::RgbImage {
        image::RgbImage::from_fn(g.width(), g.height(), |x, y| {
            let v = g.get_pixel(x, y).0[0];
            image::Rgb([v, v, v])
        })
    };
    match prep {
        Prep::None => img,
        Prep::Gray => gray_to_rgb(to_gray(&img)),
        Prep::Contrast => {
            let g = to_gray(&img);
            let (lo, hi) = g
                .pixels()
                .fold((255u8, 0u8), |(lo, hi), p| (lo.min(p.0[0]), hi.max(p.0[0])));
            let span = hi.saturating_sub(lo).max(1) as f32;
            gray_to_rgb(image::GrayImage::from_fn(g.width(), g.height(), |x, y| {
                let v = g.get_pixel(x, y).0[0];
                image::Luma([((v.saturating_sub(lo) as f32 / span) * 255.0) as u8])
            }))
        }
        Prep::Otsu => {
            let g = to_gray(&img);
            let th = otsu_threshold(&g);
            gray_to_rgb(image::GrayImage::from_fn(g.width(), g.height(), |x, y| {
                image::Luma([if g.get_pixel(x, y).0[0] > th { 255 } else { 0 }])
            }))
        }
        Prep::Sharpen => image::imageops::unsharpen(&img, 1.0, 4),
    }
}

/// Load a PNG as a frame, so accuracy can be measured against a fixed image
/// rather than a live desktop that changes between runs.
pub fn frame_from_png(path: &std::path::Path) -> Result<Frame> {
    let img = image::open(path)
        .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?
        .to_rgb8();
    Ok(Frame {
        w: img.width(),
        h: img.height(),
        // A file has no place on screen; coordinates come back image-relative.
        origin: (0, 0),
        rgb: img.into_raw(),
    })
}

/// Compare two captures. `None` when nothing moved.
///
/// Separate from the stateful `diff` mode, which owns a single previous frame
/// globally: this compares two frames a caller holds, which is what verifying
/// one specific action requires.
pub fn compare_frames(a: &Frame, b: &Frame) -> Option<ChangedRegion> {
    changed_box(a, b, 12).map(|(x, y, w, h, fraction, pixels)| ChangedRegion {
        x: b.origin.0 + x as i32,
        y: b.origin.1 + y as i32,
        w,
        h,
        fraction,
        pixels,
    })
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChangedRegion {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Share of the frame that differs, 0.0..1.0.
    pub fraction: f64,
    pub pixels: u64,
}

/// Crop exposed for the OCR region-of-interest path.
#[cfg(windows)]
pub fn crop_frame(f: &Frame, x: u32, y: u32, w: u32, h: u32) -> Frame {
    crop(f, x, y, w, h)
}

/// CLI convenience wrapper: same thing, but writes the PNG to disk.
pub fn observe(
    diff: bool,
    max_width: u32,
    out_path: Option<&str>,
    hwnd: Option<isize>,
) -> Result<Observation> {
    let (obs, png) = observe_bytes(diff, max_width, hwnd)?;
    if let (Some(p), false) = (out_path, png.is_empty()) {
        std::fs::write(p, &png)?;
    }
    Ok(obs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: u8) -> Frame {
        frame_at(w, h, fill, (0, 0))
    }

    fn frame_at(w: u32, h: u32, fill: u8, origin: (i32, i32)) -> Frame {
        Frame {
            w,
            h,
            origin,
            rgb: vec![fill; (w as usize) * (h as usize) * 3],
        }
    }

    fn set(f: &mut Frame, x: u32, y: u32, v: u8) {
        let i = ((y as usize) * (f.w as usize) + x as usize) * 3;
        f.rgb[i] = v;
        f.rgb[i + 1] = v;
        f.rgb[i + 2] = v;
    }

    /// The cheapest and most common answer during a wait loop.
    #[test]
    fn identical_frames_report_no_change() {
        assert!(changed_box(&frame(8, 8, 10), &frame(8, 8, 10), 12).is_none());
    }

    #[test]
    fn sub_tolerance_change_is_not_a_change() {
        let a = frame(8, 8, 10);
        let mut b = frame(8, 8, 10);
        set(&mut b, 3, 3, 13); // 3 per channel = 9 total, under tol 12
        assert!(changed_box(&a, &b, 12).is_none());
    }

    #[test]
    fn one_changed_pixel_yields_a_tight_box() {
        let a = frame(8, 8, 0);
        let mut b = frame(8, 8, 0);
        set(&mut b, 5, 2, 255);
        let (x, y, w, h, _, n) = changed_box(&a, &b, 12).unwrap();
        assert_eq!((x, y, w, h), (5, 2, 1, 1));
        assert_eq!(n, 1);
    }

    /// The case that motivates withholding the image: a handful of scattered
    /// pixels produce a hull covering nearly the whole frame.
    #[test]
    fn scattered_changes_produce_a_hull_far_larger_than_the_change() {
        let a = frame(100, 100, 0);
        let mut b = frame(100, 100, 0);
        set(&mut b, 1, 1, 255);
        set(&mut b, 98, 98, 255);
        let (x, y, w, h, frac, n) = changed_box(&a, &b, 12).unwrap();
        assert_eq!((x, y, w, h), (1, 1, 98, 98));
        assert_eq!(n, 2);
        assert!(frac < 0.001, "frac was {frac}");
        let hull = (w as u64) * (h as u64);
        assert!(
            hull > n * 25,
            "hull {hull} should dwarf {n} changed pixels - this is the withhold trigger"
        );
    }

    #[test]
    fn a_resize_counts_as_a_full_change() {
        let (x, y, w, h, frac, _) = changed_box(&frame(8, 8, 0), &frame(16, 16, 0), 12).unwrap();
        assert_eq!((x, y, w, h), (0, 0, 16, 16));
        assert_eq!(frac, 1.0);
    }

    #[test]
    fn crop_extracts_the_requested_region() {
        let mut f = frame(10, 10, 0);
        set(&mut f, 4, 4, 200);
        let c = crop(&f, 4, 4, 2, 2);
        assert_eq!((c.w, c.h), (2, 2));
        assert_eq!(c.rgb[0], 200);
        assert_eq!(c.rgb.len(), 2 * 2 * 3);
    }

    /// A monitor left of the primary gives a negative virtual-screen origin;
    /// a crop taken from it must still describe where it came from on screen.
    #[test]
    fn crop_carries_the_screen_origin() {
        let f = frame_at(100, 100, 0, (-1920, -200));
        let c = crop(&f, 10, 20, 5, 5);
        assert_eq!(c.origin, (-1910, -180));
    }

    #[test]
    fn crop_clamps_at_the_frame_edge() {
        let f = frame(10, 10, 0);
        let c = crop(&f, 8, 8, 4, 4);
        assert!(c.rgb.len() <= 4 * 4 * 3);
    }
}
