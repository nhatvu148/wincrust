//! Native macOS services. Coordinates use top-left desktop points.
pub mod capture;
pub mod input;
pub mod ocr;

/// Read-only setup diagnostics. Never prompts or changes system permissions.
pub fn diagnostics() -> serde_json::Value {
    serde_json::json!({
        "platform":"macos", "accessibility":crate::uia::mac::trusted(),
        "screen_recording":objc2_core_graphics::CGPreflightScreenCaptureAccess(),
        "screenshot_api_available":objc2::available!(macos = 14.0),
        "coordinate_space":"desktop points, top-left origin",
        "capabilities":{"accessibility":true,"keyboard":true,"desktop_capture":true,"ocr":true,
            "launch":false,"window_capture":false,"ocr_click":false},
        "note":"Initial macOS backend: US/ABC alphanumeric shortcuts; named navigation keys and Unicode typing. Grant permissions to the launching host or installed executable, then restart. No permissions are requested automatically."
    })
}
