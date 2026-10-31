//! Windows.Media.Ocr backend (default on Windows).
//!
//! The OCR engine ships with Windows 10+ — zero deployment, fully offline.
//! Trade-off documented in the B3 selection note: the WinRT API exposes **no
//! confidence value**, so lines report 1.0 (the engine has already done its
//! own language-model ranking); the confidence floor is therefore effectively
//! a no-op on this backend and the real filtering happens on tesseract.

use super::{OcrBackend, OcrError, OcrLine};
use crate::Bbox;
use image::RgbImage;
use windows::Foundation::Rect;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::{DataWriter, IBuffer};

pub struct WindowsOcr {
    engine: OcrEngine,
}

impl WindowsOcr {
    /// Creates the engine from the user's profile languages. A failure here
    /// means "no usable OCR language pack installed" — reported explicitly
    /// (never as an empty result, which would read as "no text on screen").
    /// Installing a language pack (Settings → Time & language → Language)
    /// fixes it.
    pub fn new() -> Result<Self, OcrError> {
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| OcrError::LanguagePackMissing(format!("{e}")))?;
        Ok(Self { engine })
    }

    /// Copies an RGB8 screenshot into a BGRA8 software bitmap (WinRT wants
    /// premultiplied BGRA with alpha; screenshots are opaque, so 255).
    fn to_software_bitmap(image: &RgbImage) -> Result<SoftwareBitmap, OcrError> {
        let (w, h) = (image.width(), image.height());
        let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for px in image.pixels() {
            bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        let writer = DataWriter::new()
            .map_err(|e| OcrError::Backend(format!("DataWriter::new failed: {e}")))?;
        writer
            .WriteBytes(&bgra)
            .map_err(|e| OcrError::Backend(format!("DataWriter::WriteBytes failed: {e}")))?;
        let buffer: IBuffer = writer
            .DetachBuffer()
            .map_err(|e| OcrError::Backend(format!("DataWriter::DetachBuffer failed: {e}")))?;
        SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w as i32, h as i32)
            .map_err(|e| {
                OcrError::Backend(format!("SoftwareBitmap::CreateCopyFromBuffer failed: {e}"))
            })
    }
}

impl OcrBackend for WindowsOcr {
    fn detect(&self, image: &RgbImage) -> Result<Vec<OcrLine>, OcrError> {
        let (w, h) = (image.width(), image.height());
        let bitmap = Self::to_software_bitmap(image)?;
        let result = self
            .engine
            .RecognizeAsync(&bitmap)
            .map_err(|e| OcrError::Backend(format!("RecognizeAsync failed: {e}")))?
            .join()
            .map_err(|e| OcrError::Backend(format!("waiting for OCR failed: {e}")))?;

        let lines_view = result
            .Lines()
            .map_err(|e| OcrError::Backend(format!("OcrResult::Lines failed: {e}")))?;
        let mut lines = Vec::new();
        for i in 0..lines_view
            .Size()
            .map_err(|e| OcrError::Backend(e.to_string()))?
        {
            let line = lines_view
                .GetAt(i)
                .map_err(|e| OcrError::Backend(format!("OcrResult::Lines[{i}] failed: {e}")))?;
            let words = line
                .Words()
                .map_err(|e| OcrError::Backend(format!("OcrLine::Words failed: {e}")))?;
            let mut content_parts = Vec::new();
            let mut union: Option<Rect> = None;
            for j in 0..words.Size().map_err(|e| OcrError::Backend(e.to_string()))? {
                let word = words
                    .GetAt(j)
                    .map_err(|e| OcrError::Backend(format!("OcrLine::Words[{j}] failed: {e}")))?;
                let text = word
                    .Text()
                    .map_err(|e| OcrError::Backend(format!("OcrWord::Text failed: {e}")))?;
                let rect = word
                    .BoundingRect()
                    .map_err(|e| OcrError::Backend(format!("OcrWord::BoundingRect failed: {e}")))?;
                content_parts.push(text.to_string());
                union = Some(match union {
                    None => rect,
                    Some(u) => Rect {
                        X: u.X.min(rect.X),
                        Y: u.Y.min(rect.Y),
                        Width: (u.X + u.Width).max(rect.X + rect.Width) - u.X.min(rect.X),
                        Height: (u.Y + u.Height).max(rect.Y + rect.Height) - u.Y.min(rect.Y),
                    },
                });
            }
            if content_parts.is_empty() {
                continue;
            }
            // Union was set whenever there was at least one word.
            let Some(rect) = union else { continue };
            if rect.Width <= 0.0 || rect.Height <= 0.0 {
                continue;
            }
            // The WinRT API has no confidence; report 1.0 (see module docs).
            let confidence = 1.0;
            let bbox = Bbox::new(
                (rect.X / w as f32).clamp(0.0, 1.0),
                (rect.Y / h as f32).clamp(0.0, 1.0),
                ((rect.X + rect.Width) / w as f32).clamp(0.0, 1.0),
                ((rect.Y + rect.Height) / h as f32).clamp(0.0, 1.0),
            )
            .map_err(|e| OcrError::Backend(format!("word union made an invalid box: {e}")))?;
            lines.push(OcrLine {
                bbox,
                content: content_parts.join(" "),
                confidence,
            });
        }
        Ok(lines)
    }

    fn name(&self) -> &'static str {
        "windows-media-ocr"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-machine smoke: the engine must construct on a Windows session
    /// with any OCR language pack and must survive a no-text image (returning
    /// an empty result, not an error). Ignored by default — run explicitly
    /// for real-machine evidence (`cargo test -p cua-engine-perception
    /// -- --ignored windows_engine`). Text-accuracy evidence comes from the
    /// `ocr_smoke` example on a real screenshot, not from this synthetic image.
    #[test]
    #[ignore = "real-machine: needs windows OCR language packs"]
    fn windows_engine_ocrs_a_synthetic_image_without_crashing() {
        let backend = WindowsOcr::new().expect("engine should construct with a language pack");
        // A 64x32 flat gray image contains no text: detect must return Ok.
        let image = RgbImage::from_fn(64, 32, |_, _| image::Rgb([128, 128, 128]));
        let lines = backend
            .detect(&image)
            .expect("detect must not fail on no-text input");
        assert!(lines.is_empty(), "flat image has no text, got {lines:?}");
        assert_eq!(backend.name(), "windows-media-ocr");
    }
}
