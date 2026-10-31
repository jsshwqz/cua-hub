//! Real-machine OCR evidence (batch B3): capture the primary screen, run the
//! platform-default OCR backend, print the recognized lines as JSON.
//!
//! Usage: `cargo run -p cua-engine-perception --example ocr_smoke`
//! Evidence goes to stdout; exit codes: 0 ok, 2 backend unavailable,
//! 3 OCR explicitly off, 4 capture/detect failure.

use cua_engine_perception::ocr::{make_backend, prepare_ocr_boxes, OcrBackendKind};
use image::DynamicImage;

fn main() {
    let kind = OcrBackendKind::from_env();
    let backend = match make_backend(kind) {
        Ok(Some(b)) => b,
        Ok(None) => {
            eprintln!("OCR explicitly off (CUA_HUB_OCR_BACKEND=none)");
            std::process::exit(3);
        }
        Err(e) => {
            eprintln!("OCR backend unavailable: {e}");
            std::process::exit(2);
        }
    };

    let captured = match cua_io::capture::capture_primary() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("capture failed: {e}");
            std::process::exit(4);
        }
    };
    let rgb = DynamicImage::ImageRgba8(captured.image).to_rgb8();

    let lines = match backend.detect(&rgb) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("detect failed: {e}");
            std::process::exit(4);
        }
    };
    let boxes = prepare_ocr_boxes(lines, 0.0);

    let lines_json: Vec<serde_json::Value> = boxes
        .iter()
        .map(|b| {
            serde_json::json!({
                "bbox": [
                    (b.bbox.x1 * 10000.0).round() / 10000.0,
                    (b.bbox.y1 * 10000.0).round() / 10000.0,
                    (b.bbox.x2 * 10000.0).round() / 10000.0,
                    (b.bbox.y2 * 10000.0).round() / 10000.0,
                ],
                "content": b.content,
                "confidence": b.confidence,
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::json!({
            "backend": backend.name(),
            "image": { "width": rgb.width(), "height": rgb.height() },
            "line_count": lines_json.len(),
            "lines": lines_json,
        })
    );
}
