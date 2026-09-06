use crate::capture::Frame;
use anyhow::{anyhow, ensure, Result};
use block2::RcBlock;
use objc2::{rc::autoreleasepool, AllocAnyThread};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
use objc2_core_graphics::{CGImage, CGPreflightScreenCaptureAccess};
use objc2_foundation::{NSArray, NSDictionary, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

/// What a screenshot needs in order to find a window the caller already has an
/// ID for.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowTarget {
    pub pid: i32,
    pub title: String,
    pub bounds: crate::uia::Bounds,
}

/// Window IDs handed out on macOS are this process's own invention: they name
/// an `AXUIElement`, which ScreenCaptureKit has never heard of and cannot be
/// converted into a `CGWindowID` through any public API.
///
/// The only public bridge between the two is the identity a person can see -
/// owning process, title, and frame - so the accessibility engine records that
/// here whenever it enumerates, and capture matches a shareable window against
/// it. Keeping the registry rather than plumbing a resolved target through
/// `observe` and `find_text` is deliberate: a caller can only hold one of these
/// IDs because `windows` gave it one, so this is a projection of what has
/// already been returned, not new state.
static KNOWN: Mutex<BTreeMap<isize, WindowTarget>> = Mutex::new(BTreeMap::new());

/// Record what the engine just enumerated.
///
/// `fresh` is what answered this round; `tracked` is every window the engine
/// still considers valid, which is a superset - an application that was
/// momentarily unresponsive keeps its windows rather than having their handles
/// invalidated. Entries for tracked-but-unanswered windows are therefore kept
/// at their last known identity instead of being dropped, so a transient hang
/// does not make `observe` and `find_text` reject a handle that `act` still
/// accepts. Anything no longer tracked is forgotten.
pub fn publish(fresh: Vec<(isize, WindowTarget)>, tracked: &[isize]) {
    let Ok(mut known) = KNOWN.lock() else {
        return;
    };
    known.retain(|id, _| tracked.contains(id));
    known.extend(fresh);
}

fn known(id: isize) -> Result<WindowTarget> {
    KNOWN
        .lock()
        .map_err(|_| anyhow!("window registry lock poisoned"))?
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("unknown window ID {id}; call windows again"))
}

/// Whether a shareable window sits exactly where the caller's window does.
///
/// The frame carries the identification. It is compared with a tolerance
/// because ScreenCaptureKit reports its own rounding of the same points the
/// accessibility API reports, and exact equality would refuse a good window
/// over half a pixel.
///
/// Title is deliberately *not* required to match, because the two APIs do not
/// agree on it: a Chrome window ScreenCaptureKit calls "New Tab" is
/// "New Tab - Google Chrome" to accessibility. Requiring equality refused every
/// Chrome window while quietly working for TextEdit and Safari, which happen to
/// agree. Title is used only to break a tie between windows of one application
/// sharing a rectangle - see `pick`.
fn same_place(target: &WindowTarget, frame: (i32, i32, i32, i32)) -> bool {
    let b = target.bounds;
    let near = |a: i32, c: i32| (a - c).abs() <= 2;
    near(b.x, frame.0) && near(b.y, frame.1) && near(b.w, frame.2) && near(b.h, frame.3)
}

/// Whether two titles plausibly name the same window across the two APIs.
///
/// One is routinely a prefix of the other, so containment is the most that can
/// be asked of them.
fn title_agrees(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.contains(b) || b.contains(a))
}

/// Choose among same-application windows that occupy the caller's rectangle.
///
/// Returns `None` rather than guessing when the choice is not forced:
/// capturing the wrong window answers a question about one application with
/// the contents of another, which is worse than answering nothing.
fn pick<T>(target: &WindowTarget, mut candidates: Vec<(T, String)>) -> Option<T> {
    if candidates.len() > 1 {
        let narrowed: Vec<_> = candidates
            .drain(..)
            .filter(|(_, title)| title_agrees(&target.title, title))
            .collect();
        candidates = narrowed;
    }
    match candidates.len() {
        1 => Some(candidates.remove(0).0),
        _ => None,
    }
}

enum Message {
    Count(usize),
    Frame(Result<Frame>),
}

pub fn grab() -> Result<Frame> {
    ensure!(
        objc2::available!(macos = 14.0),
        "Screen capture requires macOS 14 or later"
    );
    ensure!(CGPreflightScreenCaptureAccess(), "Screen Recording permission missing. Grant the host application or installed Wincrust executable access in System Settings > Privacy & Security > Screen Recording, then restart it.");
    let (tx, rx) = std::sync::mpsc::channel();
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            autoreleasepool(|_| unsafe {
                let Some(content) = content.as_ref() else {
                    let message = error
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "no shareable desktop".into());
                    let _ = tx.send(Message::Frame(Err(anyhow!(message))));
                    return;
                };
                let displays = content.displays();
                let _ = tx.send(Message::Count(displays.len()));
                for display in displays {
                    let rect = display.frame();
                    let width = rect.size.width.round().max(1.0) as usize;
                    let height = rect.size.height.round().max(1.0) as usize;
                    let origin = (rect.origin.x.round() as i32, rect.origin.y.round() as i32);
                    let filter = SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        &display,
                        &NSArray::new(),
                    );
                    let config = SCStreamConfiguration::new();
                    // Normalize each display to one pixel per desktop point. This
                    // keeps OCR/click coordinates consistent across mixed scales.
                    config.setWidth(width);
                    config.setHeight(height);
                    config.setShowsCursor(false);
                    let reply = tx.clone();
                    let image_done =
                        RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                            autoreleasepool(|_| {
                                let _ = reply.send(Message::Frame(to_frame(
                                    image, error, width, height, origin,
                                )));
                            });
                        });
                    SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                        &filter,
                        &config,
                        Some(&image_done),
                    );
                }
            });
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&completion);
    }
    let n = match rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow!("{}", CAPTURE_STALLED))?
    {
        Message::Count(n) => n,
        Message::Frame(e) => return e,
    };
    ensure!(
        n > 0 && n <= 32,
        "no usable displays or display count exceeds limit"
    );
    let mut frames = Vec::new();
    for _ in 0..n {
        if let Message::Frame(frame) = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow!("{}", CAPTURE_STALLED))?
        {
            frames.push(frame?);
        }
    }
    compose(frames)
}

/// Turn a ScreenCaptureKit callback's image into a `Frame` in desktop points.
///
/// Shared by the display and window paths so a capture cannot come back in one
/// coordinate space from one of them and a different space from the other.
fn to_frame(
    image: *mut CGImage,
    error: *mut NSError,
    width: usize,
    height: usize,
    origin: (i32, i32),
) -> Result<Frame> {
    let image = unsafe { image.as_ref() }.ok_or_else(|| {
        anyhow!(unsafe { error.as_ref() }
            .map(|e| e.to_string())
            .unwrap_or_else(|| "empty screenshot".into()))
    })?;
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), image);
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or_else(|| anyhow!("PNG encoding failed"))?;
    let rgb = image::load_from_memory(&png.to_vec())?.to_rgb8();
    ensure!(
        rgb.width() as usize == width && rgb.height() as usize == height,
        "unexpected capture dimensions"
    );
    Ok(Frame {
        w: rgb.width(),
        h: rgb.height(),
        origin,
        rgb: rgb.into_raw(),
    })
}

/// Capture one window's own content, rather than the desktop it sits on.
///
/// This is the private path: ScreenCaptureKit renders the window itself, so an
/// overlapping window contributes nothing and nothing else on screen is read
/// into this process at all. `find_text` on a window therefore surveys that
/// window rather than everything the user happens to have open, which matters
/// on a shared screen far more than the pixel count suggests.
pub fn capture_window(id: isize) -> Result<Frame> {
    ensure!(
        objc2::available!(macos = 14.0),
        "Screen capture requires macOS 14 or later"
    );
    ensure!(CGPreflightScreenCaptureAccess(), "Screen Recording permission missing. Grant the host application or installed Wincrust executable access in System Settings > Privacy & Security > Screen Recording, then restart it.");
    let target = known(id)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            autoreleasepool(|_| unsafe {
                let Some(content) = content.as_ref() else {
                    let message = error
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "no shareable content".into());
                    let _ = tx.send(Message::Frame(Err(anyhow!(message))));
                    return;
                };
                let mut hits = Vec::new();
                for window in content.windows() {
                    let owner = window
                        .owningApplication()
                        .map(|a| a.processID())
                        .unwrap_or(-1);
                    if owner != target.pid {
                        continue;
                    }
                    let r = window.frame();
                    let frame = (
                        r.origin.x.round() as i32,
                        r.origin.y.round() as i32,
                        r.size.width.round() as i32,
                        r.size.height.round() as i32,
                    );
                    if same_place(&target, frame) {
                        let title = window.title().map(|t| t.to_string()).unwrap_or_default();
                        hits.push(((window, frame), title));
                    }
                }
                let found = pick(&target, hits);
                let Some((window, frame)) = found else {
                    let _ = tx.send(Message::Frame(Err(anyhow!(
                        "no single shareable window matches window {id} ({:?} at {:?}); it may have moved or closed - call windows again",
                        target.title,
                        target.bounds
                    ))));
                    return;
                };
                let width = (frame.2.max(1)) as usize;
                let height = (frame.3.max(1)) as usize;
                let filter = SCContentFilter::initWithDesktopIndependentWindow(
                    SCContentFilter::alloc(),
                    &window,
                );
                let config = SCStreamConfiguration::new();
                // One pixel per desktop point, and an origin at the window's own
                // corner, so OCR hits come back in the same screen coordinates
                // a desktop capture would have produced.
                config.setWidth(width);
                config.setHeight(height);
                config.setShowsCursor(false);
                let reply = tx.clone();
                let image_done = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                    autoreleasepool(|_| {
                        let _ = reply.send(Message::Frame(to_frame(
                            image,
                            error,
                            width,
                            height,
                            (frame.0, frame.1),
                        )));
                    });
                });
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &config,
                    Some(&image_done),
                );
            });
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&completion);
    }
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Message::Frame(frame)) => frame,
        Ok(Message::Count(_)) => Err(anyhow!("unexpected display count during window capture")),
        Err(_) => Err(anyhow!("{}", CAPTURE_STALLED)),
    }
}

/// ScreenCaptureKit answers a healthy request in tens of milliseconds, so a
/// timeout here is never slowness - it is a callback that will never arrive.
///
/// Observed cause: a wincrust process killed while a capture was outstanding
/// leaves the capture daemon holding a stream for a dead client, and every
/// later wincrust process then blocks. Ending the strays clears it; nothing
/// else does, including restarting `replayd`. Diagnosing this from
/// "timed out waiting on channel" took an hour and nearly ended in a reboot.
const CAPTURE_STALLED: &str = "screen capture did not respond within 10s. ScreenCaptureKit is almost certainly wedged rather than slow: a wincrust process killed mid-capture leaves the capture daemon holding its stream. Check for stray processes with `pgrep -fl \"wincrust serve\"` and end them, then retry.";

fn compose(frames: Vec<Frame>) -> Result<Frame> {
    ensure!(!frames.is_empty(), "no captured displays");
    let x = frames.iter().map(|f| f.origin.0).min().unwrap();
    let y = frames.iter().map(|f| f.origin.1).min().unwrap();
    let right = frames
        .iter()
        .map(|f| i64::from(f.origin.0) + i64::from(f.w))
        .max()
        .unwrap();
    let bottom = frames
        .iter()
        .map(|f| i64::from(f.origin.1) + i64::from(f.h))
        .max()
        .unwrap();
    let w = u32::try_from(right - i64::from(x))?;
    let h = u32::try_from(bottom - i64::from(y))?;
    ensure!(
        u64::from(w) * u64::from(h) <= 64_000_000,
        "desktop exceeds 64 megapixel capture limit"
    );
    let mut rgb = vec![0; w as usize * h as usize * 3];
    for frame in frames {
        let ox = (frame.origin.0 - x) as usize;
        let oy = (frame.origin.1 - y) as usize;
        for row in 0..frame.h as usize {
            let dest = ((oy + row) * w as usize + ox) * 3;
            let source = row * frame.w as usize * 3;
            let len = frame.w as usize * 3;
            rgb[dest..dest + len].copy_from_slice(&frame.rgb[source..source + len]);
        }
    }
    Ok(Frame {
        w,
        h,
        origin: (x, y),
        rgb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn displays_left_and_above_keep_origin_and_rows() {
        let f = compose(vec![
            Frame {
                w: 1,
                h: 2,
                origin: (-1, -1),
                rgb: vec![1, 2, 3, 4, 5, 6],
            },
            Frame {
                w: 1,
                h: 1,
                origin: (0, 0),
                rgb: vec![7, 8, 9],
            },
        ])
        .unwrap();
        assert_eq!((f.w, f.h, f.origin), (2, 2, (-1, -1)));
        assert_eq!(f.rgb, vec![1, 2, 3, 0, 0, 0, 4, 5, 6, 7, 8, 9]);
    }
}

#[cfg(test)]
mod match_tests {
    use super::*;
    use crate::uia::Bounds;

    fn target(title: &str) -> WindowTarget {
        WindowTarget {
            pid: 1,
            title: title.into(),
            bounds: Bounds {
                x: 60,
                y: 39,
                w: 1996,
                h: 1290,
            },
        }
    }

    #[test]
    fn a_frame_matches_through_rounding() {
        assert!(same_place(&target("x"), (60, 39, 1996, 1290)));
        assert!(same_place(&target("x"), (61, 38, 1997, 1291)));
        assert!(!same_place(&target("x"), (70, 39, 1996, 1290)));
    }

    /// The case that refused every Chrome window: ScreenCaptureKit says
    /// "New Tab", accessibility says "New Tab - Google Chrome".
    #[test]
    fn titles_need_only_agree_not_match() {
        assert!(title_agrees("New Tab - Google Chrome", "New Tab"));
        assert!(title_agrees("scratch.txt", "scratch.txt"));
        assert!(!title_agrees("docA.txt", "docB.txt"));
        assert!(
            !title_agrees("", "New Tab"),
            "an unknown title agrees with nothing"
        );
    }

    #[test]
    fn one_candidate_is_taken_whatever_it_is_called() {
        assert_eq!(
            pick(
                &target("New Tab - Google Chrome"),
                vec![(7, "New Tab".to_string())]
            ),
            Some(7)
        );
        assert_eq!(pick(&target("anything"), vec![(7, String::new())]), Some(7));
    }

    #[test]
    fn a_tie_is_broken_by_title_or_refused() {
        let two = vec![(1, "docA.txt".to_string()), (2, "docB.txt".to_string())];
        assert_eq!(pick(&target("docB.txt"), two), Some(2));
        let same = vec![(1, "same".to_string()), (2, "same".to_string())];
        assert_eq!(
            pick(&target("same"), same),
            None,
            "indistinguishable must refuse"
        );
        assert_eq!(pick::<i32>(&target("x"), vec![]), None);
    }
}
