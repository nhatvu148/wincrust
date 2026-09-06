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
use std::time::Duration;

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
                                let frame = (|| -> Result<Frame> {
                                    let image = image.as_ref().ok_or_else(|| {
                                        anyhow!(error
                                            .as_ref()
                                            .map(|e| e.to_string())
                                            .unwrap_or_else(|| "empty screenshot".into()))
                                    })?;
                                    let rep = NSBitmapImageRep::initWithCGImage(
                                        NSBitmapImageRep::alloc(),
                                        image,
                                    );
                                    let png = rep
                                        .representationUsingType_properties(
                                            NSBitmapImageFileType::PNG,
                                            &NSDictionary::new(),
                                        )
                                        .ok_or_else(|| anyhow!("PNG encoding failed"))?;
                                    let rgb = image::load_from_memory(&png.to_vec())?.to_rgb8();
                                    ensure!(
                                        rgb.width() as usize == width
                                            && rgb.height() as usize == height,
                                        "unexpected capture dimensions"
                                    );
                                    Ok(Frame {
                                        w: rgb.width(),
                                        h: rgb.height(),
                                        origin,
                                        rgb: rgb.into_raw(),
                                    })
                                })();
                                let _ = reply.send(Message::Frame(frame));
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
    let n = match rx.recv_timeout(Duration::from_secs(10))? {
        Message::Count(n) => n,
        Message::Frame(e) => return e,
    };
    ensure!(
        n > 0 && n <= 32,
        "no usable displays or display count exceeds limit"
    );
    let mut frames = Vec::new();
    for _ in 0..n {
        if let Message::Frame(frame) = rx.recv_timeout(Duration::from_secs(10))? {
            frames.push(frame?);
        }
    }
    compose(frames)
}

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
