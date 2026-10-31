//! OCR anchoring (issue #18 P1-1 batch B3): turn a screenshot into text boxes,
//! then anchor a target text to a box.
//!
//! The backend contract is one method: [`OcrBackend::detect`] returns lines
//! with normalized boxes and a confidence. Everything else here is backend
//! agnostic and fixes, one for one, the three implementation defects the #18
//! survey found in self-operating-computer's OCR path (`operate/utils/ocr.py`
//! via `call_gpt_4o_with_ocr`):
//!
//! 1. **Multiple hits took the last one** → [`anchor`] returns
//!    [`AnchorResult::Ambiguous`] with *all* candidates ordered by confidence;
//!    picking among them is the caller's explicit decision, not an accident of
//!    iteration order. (Same philosophy as #37's ambiguity refusal.)
//! 2. **The OCR score was ignored** → [`prepare_ocr_boxes`] drops lines below
//!    a confidence threshold before anything downstream sees them.
//! 3. **Not-found fell back through an exception** → [`anchor`] returns
//!    [`AnchorResult::NotFound`], an ordinary value; callers switch strategy
//!    without unwinding.
//!
//! Backends live in sibling modules: [`windows::WindowsOcr`] (the system
//! engine, zero deployment) and [`tesseract::TesseractCli`] (the CLI, default
//! on Linux). [`make_backend`] picks by [`OcrBackendKind`], which comes from
//! the `CUA_HUB_OCR_BACKEND` environment variable.

use crate::merge::OcrBox;
use crate::Bbox;

pub mod tesseract;
#[cfg(windows)]
pub mod windows;

/// One OCR line: a normalized box, its transcription, and the backend's own
/// confidence in `0.0..=1.0`.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub bbox: Bbox,
    pub content: String,
    pub confidence: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// The backend binary/engine does not exist on this machine.
    #[error("OCR backend not available: {0}")]
    BackendMissing(String),
    /// The engine exists but has no usable language for the screen's text.
    /// Reported explicitly — an empty result here would read as "no text on
    /// screen", which is a different (and wrong) claim.
    #[error("OCR language pack missing: {0}")]
    LanguagePackMissing(String),
    /// The backend ran and failed.
    #[error("OCR backend error: {0}")]
    Backend(String),
}

/// A screenshot-to-lines backend.
pub trait OcrBackend {
    fn detect(&self, image: &image::RgbImage) -> Result<Vec<OcrLine>, OcrError>;
    fn name(&self) -> &'static str;
}

/// Which backend [`make_backend`] should build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrBackendKind {
    /// Platform default: Windows engine on Windows, tesseract CLI elsewhere.
    Auto,
    Windows,
    Tesseract,
    /// Explicitly off: [`make_backend`] yields `Ok(None)` and every caller
    /// proceeds without OCR — element tables still work (batch B2 handles
    /// OCR-less input).
    None,
}

impl OcrBackendKind {
    /// Parses `CUA_HUB_OCR_BACKEND` (`auto` / `windows` / `tesseract` / `none`).
    /// Unset means [`OcrBackendKind::Auto`]; anything unparseable means
    /// [`OcrBackendKind::Auto`] too — a typo must not silently disable OCR.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("windows") => Self::Windows,
            Some("tesseract") => Self::Tesseract,
            Some("none") => Self::None,
            _ => Self::Auto,
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var("CUA_HUB_OCR_BACKEND").ok().as_deref())
    }
}

/// Builds the backend for `kind`. `Ok(None)` means OCR is explicitly off.
pub fn make_backend(kind: OcrBackendKind) -> Result<Option<Box<dyn OcrBackend>>, OcrError> {
    match kind {
        OcrBackendKind::None => Ok(None),
        OcrBackendKind::Tesseract => Ok(Some(Box::new(tesseract::TesseractCli::new()?))),
        #[cfg(windows)]
        OcrBackendKind::Windows => Ok(Some(Box::new(windows::WindowsOcr::new()?))),
        #[cfg(not(windows))]
        OcrBackendKind::Windows => Err(OcrError::BackendMissing(
            "the windows OCR backend only exists on windows".into(),
        )),
        #[cfg(windows)]
        OcrBackendKind::Auto => Ok(Some(Box::new(windows::WindowsOcr::new()?))),
        #[cfg(not(windows))]
        OcrBackendKind::Auto => Ok(Some(Box::new(tesseract::TesseractCli::new()?))),
    }
}

/// Applies the confidence floor (fix #2) and orders lines best-first.
///
/// Lines below `min_confidence` are dropped; the rest come back as OCR boxes
/// sorted by confidence descending, ties in input order. An all-filtered
/// input returns an empty vec — an ordinary value the caller checks, not an
/// error to unwind (fix #3).
pub fn prepare_ocr_boxes(lines: Vec<OcrLine>, min_confidence: f32) -> Vec<OcrBox> {
    let mut kept: Vec<OcrBox> = lines
        .into_iter()
        .filter(|l| l.confidence >= min_confidence)
        .map(|l| OcrBox {
            bbox: l.bbox,
            content: l.content,
            confidence: l.confidence,
        })
        .collect();
    kept.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    kept
}

/// What anchoring a target text against OCR boxes produced.
#[derive(Debug, Clone, PartialEq)]
pub enum AnchorResult {
    /// Exactly one box matched.
    Matched(Bbox),
    /// Two or more boxes matched the same text, best confidence first. The
    /// caller decides — this port never picks for you (fix #1).
    Ambiguous(Vec<Bbox>),
    /// Nothing matched; the caller switches strategy.
    NotFound,
}

/// Anchors `text` against prepared OCR boxes: a case-insensitive, trimmed
/// exact match against each box's content.
pub fn anchor(text: &str, boxes: &[OcrBox]) -> AnchorResult {
    let needle = text.trim().to_lowercase();
    if needle.is_empty() {
        return AnchorResult::NotFound;
    }
    let hits: Vec<&OcrBox> = boxes
        .iter()
        .filter(|b| b.content.trim().to_lowercase() == needle)
        .collect();
    match hits.len() {
        0 => AnchorResult::NotFound,
        1 => AnchorResult::Matched(hits[0].bbox),
        _ => {
            let mut sorted: Vec<OcrBox> = hits.iter().map(|b| (*b).clone()).collect();
            sorted.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            AnchorResult::Ambiguous(sorted.into_iter().map(|b| b.bbox).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x1: f32, y1: f32, x2: f32, y2: f32, content: &str, confidence: f32) -> OcrLine {
        OcrLine {
            bbox: Bbox::new(x1, y1, x2, y2).unwrap(),
            content: content.to_string(),
            confidence,
        }
    }

    #[test]
    fn lines_below_the_confidence_floor_are_dropped_others_pass() {
        let lines = vec![
            line(0.0, 0.0, 0.2, 0.1, "good", 0.8),
            line(0.3, 0.0, 0.5, 0.1, "bad", 0.6),
            line(0.6, 0.0, 0.8, 0.1, "at floor", 0.7),
        ];
        let boxes = prepare_ocr_boxes(lines, 0.7);
        assert_eq!(boxes.len(), 2, "0.6 dropped, 0.7 kept (>= floor), 0.8 kept");
        assert_eq!(boxes[0].content, "good");
        assert_eq!(boxes[1].content, "at floor");
    }

    #[test]
    fn everything_filtered_returns_an_empty_vec_not_an_error() {
        let lines = vec![line(0.0, 0.0, 0.2, 0.1, "junk", 0.1)];
        assert!(
            prepare_ocr_boxes(lines, 0.9).is_empty(),
            "fix #3: empty is a value"
        );
        assert!(prepare_ocr_boxes(vec![], 0.5).is_empty());
    }

    #[test]
    fn a_unique_hit_matches_directly() {
        let boxes = prepare_ocr_boxes(
            vec![
                line(0.0, 0.0, 0.3, 0.1, "File", 0.9),
                line(0.4, 0.0, 0.6, 0.1, "Edit", 0.9),
            ],
            0.5,
        );
        assert_eq!(
            anchor("file", &boxes),
            AnchorResult::Matched(Bbox::new(0.0, 0.0, 0.3, 0.1).unwrap())
        );
    }

    #[test]
    fn multiple_hits_come_back_as_sorted_candidates_never_silently_the_last_one() {
        // Two boxes carry the same text. The SOC original would have clicked
        // the LAST one; this returns both, best confidence first.
        let boxes = prepare_ocr_boxes(
            vec![
                line(0.0, 0.0, 0.3, 0.1, "Open", 0.7),
                line(0.5, 0.5, 0.8, 0.6, "Open", 0.95),
            ],
            0.5,
        );
        let result = anchor("open", &boxes);
        match result {
            AnchorResult::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(
                    candidates[0],
                    Bbox::new(0.5, 0.5, 0.8, 0.6).unwrap(),
                    "higher confidence first"
                );
                assert_eq!(candidates[1], Bbox::new(0.0, 0.0, 0.3, 0.1).unwrap());
            }
            other => panic!("two hits must be Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn no_hit_is_not_found_and_never_an_exception() {
        let boxes = prepare_ocr_boxes(vec![line(0.0, 0.0, 0.3, 0.1, "File", 0.9)], 0.5);
        assert_eq!(anchor("Save As", &boxes), AnchorResult::NotFound);
        assert_eq!(anchor("", &boxes), AnchorResult::NotFound, "empty needle");
        assert_eq!(anchor("  ", &boxes), AnchorResult::NotFound, "blank needle");
    }

    #[test]
    fn matching_trims_and_ignores_case_on_both_sides() {
        let boxes = prepare_ocr_boxes(vec![line(0.0, 0.0, 0.3, 0.1, "  Open File  ", 0.9)], 0.5);
        assert!(matches!(
            anchor(" open file ", &boxes),
            AnchorResult::Matched(_)
        ));
    }

    #[test]
    fn backend_kind_parses_the_env_values_and_defaults_to_auto() {
        assert_eq!(OcrBackendKind::from_env_value(None), OcrBackendKind::Auto);
        assert_eq!(
            OcrBackendKind::from_env_value(Some("auto")),
            OcrBackendKind::Auto
        );
        assert_eq!(
            OcrBackendKind::from_env_value(Some("windows")),
            OcrBackendKind::Windows
        );
        assert_eq!(
            OcrBackendKind::from_env_value(Some("Tesseract")),
            OcrBackendKind::Tesseract
        );
        assert_eq!(
            OcrBackendKind::from_env_value(Some(" none ")),
            OcrBackendKind::None
        );
        // A typo must NOT silently disable OCR: unknown values mean Auto.
        assert_eq!(
            OcrBackendKind::from_env_value(Some("nonne")),
            OcrBackendKind::Auto
        );
    }

    #[test]
    fn make_backend_none_yields_no_backend_without_error() {
        assert!(make_backend(OcrBackendKind::None).unwrap().is_none());
    }
}
