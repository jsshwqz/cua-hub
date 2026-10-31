//! Tesseract CLI backend (default on Linux/CI).
//!
//! Talks to the `tesseract` command over stdin/stdout — no libtesseract
//! binding, no new compiled dependency. Deployment is one package:
//! `apt install tesseract-ocr` (language packs like `tesseract-ocr-chi-sim`
//! as needed). The TSV parser is a pure function, tested against a fixture;
//! only the spawn touches the outside world.

use super::{OcrBackend, OcrError, OcrLine};
use crate::Bbox;
use image::codecs::png::PngEncoder;
use image::{ImageEncoder, RgbImage};
use std::io::Write;
use std::process::{Command, Stdio};

/// Default recognition languages for the CLI backend (`-l` value).
pub const DEFAULT_LANGUAGES: &str = "eng";

pub struct TesseractCli {
    languages: String,
}

impl TesseractCli {
    /// Builds the backend. Verifies up front that `tesseract` is runnable so
    /// a missing binary surfaces as [`OcrError::BackendMissing`] at
    /// construction, not as a surprise mid-screenshot.
    pub fn new() -> Result<Self, OcrError> {
        Self::with_languages(DEFAULT_LANGUAGES)
    }

    pub fn with_languages(languages: &str) -> Result<Self, OcrError> {
        let probe = Command::new("tesseract")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| {
                OcrError::BackendMissing(format!(
                    "tesseract not found on PATH ({e}); install it, e.g. apt install tesseract-ocr"
                ))
            })?;
        if !probe.success() {
            return Err(OcrError::BackendMissing(format!(
                "tesseract exited with {probe} during version probe"
            )));
        }
        Ok(Self {
            languages: languages.to_string(),
        })
    }
}

impl OcrBackend for TesseractCli {
    fn detect(&self, image: &RgbImage) -> Result<Vec<OcrLine>, OcrError> {
        // Encode the screenshot as PNG in memory, feed through stdin.
        let (w, h) = (image.width(), image.height());
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(image.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e| OcrError::Backend(format!("png encode failed: {e}")))?;

        let mut child = Command::new("tesseract")
            .arg("stdin")
            .arg("stdout")
            .arg("-l")
            .arg(&self.languages)
            .arg("tsv")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| OcrError::Backend(format!("tesseract spawn failed: {e}")))?;

        child
            .stdin
            .take()
            .ok_or_else(|| OcrError::Backend("tesseract stdin unavailable".into()))?
            .write_all(&png)
            .map_err(|e| {
                OcrError::Backend(format!("writing screenshot to tesseract failed: {e}"))
            })?;

        let out = child
            .wait_with_output()
            .map_err(|e| OcrError::Backend(format!("waiting for tesseract failed: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(OcrError::Backend(format!(
                "tesseract exited with {}: {}",
                out.status,
                stderr.trim().chars().take(300).collect::<String>()
            )));
        }
        let tsv = String::from_utf8_lossy(&out.stdout);
        Ok(parse_tsv(&tsv, w, h))
    }

    fn name(&self) -> &'static str {
        "tesseract-cli"
    }
}

/// Parses tesseract's TSV output (11 columns; word rows are `level == 5`)
/// into lines: words are grouped by `(block, par, line)`, each group's box is
/// One TSV word row: (confidence, left, top, width, height, text).
type WordRow = (f32, u32, u32, u32, u32, String);
/// Line group key in tesseract's TSV: (page, block, par, line).
type LineKey = (u32, u32, u32, u32);

/// Parses tesseract's TSV output (11 columns; word rows are `level == 5`)
/// into lines: words are grouped by `(block, par, line)`, each group's box is
/// the union of its word boxes, confidence is the mean over its words.
/// Container rows (`conf == -1`) are skipped. Boxes are normalized to
/// `0.0..=1.0` against the image size.
pub fn parse_tsv(tsv: &str, width: u32, height: u32) -> Vec<OcrLine> {
    let mut groups: Vec<(LineKey, Vec<WordRow>)> = Vec::new();
    for row in tsv.lines().skip(1) {
        let cols: Vec<&str> = row.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        let level: u32 = cols[0].parse().unwrap_or(0);
        if level != 5 {
            continue;
        }
        let conf: f32 = cols[10].parse().unwrap_or(-1.0);
        let text = cols[11].trim();
        if conf < 0.0 || text.is_empty() {
            continue;
        }
        let key = (
            cols[1].parse().unwrap_or(0),
            cols[2].parse().unwrap_or(0),
            cols[3].parse().unwrap_or(0),
            cols[4].parse().unwrap_or(0),
        );
        let word = (
            conf,
            cols[6].parse().unwrap_or(0),
            cols[7].parse().unwrap_or(0),
            cols[8].parse().unwrap_or(0),
            cols[9].parse().unwrap_or(0),
            text.to_string(),
        );
        match groups.last_mut() {
            // TSV rows for one line are contiguous.
            Some((k, words)) if *k == key => words.push(word),
            _ => groups.push((key, vec![word])),
        }
    }

    let to_norm_x = |v: u32| (v as f32 / width as f32).clamp(0.0, 1.0);
    let to_norm_y = |v: u32| (v as f32 / height as f32).clamp(0.0, 1.0);

    let mut lines = Vec::new();
    for (_, words) in groups {
        if words.is_empty() {
            continue;
        }
        let left = words.iter().map(|w| w.1).min().unwrap_or(0);
        let top = words.iter().map(|w| w.2).min().unwrap_or(0);
        let right = words.iter().map(|w| w.1 + w.3).max().unwrap_or(0);
        let bottom = words.iter().map(|w| w.2 + w.4).max().unwrap_or(0);
        let confidence = words.iter().map(|w| w.0).sum::<f32>() / words.len() as f32 / 100.0;
        let content = words
            .iter()
            .map(|w| w.5.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if right <= left || bottom <= top {
            continue;
        }
        if let Ok(bbox) = Bbox::new(
            to_norm_x(left),
            to_norm_y(top),
            to_norm_x(right),
            to_norm_y(bottom),
        ) {
            lines.push(OcrLine {
                bbox,
                content,
                confidence,
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two lines, three words total, on a 400x200 image — columns and shape
    /// match tesseract 5's real TSV output.
    const FIXTURE: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t-1\t-1\t-1\t-1\t-1\t-1
5\t1\t1\t1\t1\t1\t40\t20\t120\t30\t96.5\tFile
5\t1\t1\t1\t1\t2\t180\t20\t100\t30\t92.0\tEdit
5\t1\t1\t1\t2\t1\t40\t80\t80\t24\t88.0\tSave
5\t1\t1\t2\t1\t1\t200\t120\t60\t20\t-1\t";

    #[test]
    fn tsv_fixture_parses_into_two_lines_with_union_boxes_and_mean_confidence() {
        let lines = parse_tsv(FIXTURE, 400, 200);
        assert_eq!(lines.len(), 2, "container row and conf=-1 row are skipped");

        // Line 1: words "File"(40,20,120x30) + "Edit"(180,20,100x30)
        let first = &lines[0];
        assert_eq!(first.content, "File Edit");
        assert_eq!(first.bbox, Bbox::new(0.1, 0.1, 0.7, 0.25).unwrap());
        assert!((first.confidence - (96.5 + 92.0) / 2.0 / 100.0).abs() < 1e-6);

        // Line 2: single word "Save", 88% confidence.
        let second = &lines[1];
        assert_eq!(second.content, "Save");
        assert_eq!(second.bbox, Bbox::new(0.1, 0.4, 0.3, 0.52).unwrap());
        assert!((second.confidence - 0.88).abs() < 1e-6);
    }

    #[test]
    fn empty_and_garbage_tsv_produce_no_lines() {
        assert!(parse_tsv("", 100, 100).is_empty());
        assert!(parse_tsv("not a tsv at all", 100, 100).is_empty());
        assert!(parse_tsv("level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n", 100, 100).is_empty());
    }

    #[test]
    fn a_degenerate_word_box_is_skipped_rather_than_rejected_loudly() {
        // width=height=0 word on a real row: union collapses; parser skips.
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n5\t1\t1\t1\t1\t1\t10\t10\t0\t0\t90.0\tghost\n";
        assert!(parse_tsv(tsv, 100, 100).is_empty());
    }

    #[test]
    fn backend_name_and_missing_binary_are_explicit() {
        // `make_backend` on a machine without tesseract must say so at
        // construction time — this test documents the error kind, and on
        // machines WITH tesseract it simply constructs fine.
        match TesseractCli::new() {
            Ok(_) => {}
            Err(OcrError::BackendMissing(msg)) => assert!(msg.contains("tesseract")),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
