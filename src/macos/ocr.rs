use crate::ocr::{FindArgs, TextMatch, TextResult};
use anyhow::{anyhow, ensure, Result};
use objc2::{rc::autoreleasepool, AllocAnyThread};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest};

pub fn find_text(args: FindArgs<'_>) -> Result<TextResult> {
    let start = std::time::Instant::now();
    let frame = match (args.image, args.hwnd) {
        (Some(p), _) => crate::capture::frame_from_png(p)?,
        // Reading one window rather than the desktop is not only cheaper: on a
        // shared screen a desktop survey OCRs every other application the user
        // has open, and returns their text to the caller.
        (None, Some(hwnd)) => super::capture::capture_window(hwnd)?,
        (None, None) => super::capture::grab()?,
    };
    ensure!(
        args.scale.is_finite() && (0.0..=4.0).contains(&args.scale),
        "scale must be between 0 and 4"
    );
    let requested = if args.scale == 0.0 { 1.0 } else { args.scale };
    ensure!(
        f64::from(frame.w) * f64::from(frame.h) * f64::from(requested.max(1.0)).powi(2)
            <= 64_000_000.0,
        "OCR image exceeds 64 megapixel limit"
    );
    let (png, scale) = crate::capture::encode_png_scaled(&frame, requested, args.prep)?;
    autoreleasepool(|_| {
        let request = VNRecognizeTextRequest::new();
        let available = unsafe { request.supportedRecognitionLanguagesAndReturnError() }
            .map_err(|e| anyhow!(e.to_string()))?
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>();
        if let Some(lang) = args.lang {
            ensure!(
                available.iter().any(|l| l.eq_ignore_ascii_case(lang)),
                "unsupported OCR language {lang}; available: {}",
                available.join(", ")
            );
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&[NSString::from_str(
                lang,
            )]));
        }
        let languages = unsafe { request.recognitionLanguages() }
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let handler = VNImageRequestHandler::initWithData_options(
            VNImageRequestHandler::alloc(),
            &NSData::with_bytes(&png),
            &NSDictionary::new(),
        );
        let requests = NSArray::<VNRequest>::from_slice(&[&request]);
        handler
            .performRequests_error(&requests)
            .map_err(|e| anyhow!(e.to_string()))?;
        let observations = request
            .results()
            .ok_or_else(|| anyhow!("Vision returned no result array"))?;
        let mut matches = Vec::new();
        for observation in observations.iter() {
            if matches.len() >= args.max_matches {
                break;
            }
            let candidates = observation.topCandidates(1);
            let Some(candidate) = candidates.firstObject() else {
                continue;
            };
            let text = candidate.string().to_string();
            let tier = match args.query {
                Some(q) => crate::ocr::matches_text(&text, q),
                None => Some(crate::text::MatchTier::Exact),
            };
            let Some(matched_by) = tier else {
                continue;
            };
            let b = unsafe { observation.boundingBox() };
            let (x, y, w, h) = desktop_rect(b, frame.w, frame.h, frame.origin);
            matches.push(TextMatch {
                text,
                click_at: (x + w / 2, y + h / 2),
                x,
                y,
                w,
                h,
                granularity: "line".into(),
                matched_by,
            });
        }
        Ok(TextResult {
            language: languages,
            available_languages: available,
            matches,
            lines_seen: observations.len(),
            scale,
            prep: format!("{:?}", args.prep).to_lowercase(),
            elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        })
    })
}

fn desktop_rect(
    b: objc2_core_foundation::CGRect,
    w: u32,
    h: u32,
    origin: (i32, i32),
) -> (i32, i32, i32, i32) {
    // Vision's normalized coordinates start at bottom-left; desktop coordinates
    // start at top-left. Normalization also removes OCR magnification.
    (
        origin.0 + (b.origin.x * f64::from(w)).round() as i32,
        origin.1 + ((1.0 - b.origin.y - b.size.height) * f64::from(h)).round() as i32,
        (b.size.width * f64::from(w)).round() as i32,
        (b.size.height * f64::from(h)).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    #[test]
    fn vision_coordinates_flip_y_and_preserve_negative_origin() {
        let b = CGRect::new(CGPoint::new(0.25, 0.75), CGSize::new(0.5, 0.1));
        assert_eq!(
            desktop_rect(b, 1000, 1000, (-1000, -200)),
            (-750, -50, 500, 100)
        );
    }
}
