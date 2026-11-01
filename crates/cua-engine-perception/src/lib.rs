//! Visual element parsing contract (issue #18 P1-1, batch B1).
//!
//! The contract has three deliverables, mirroring OmniParser's `parse()` shape
//! (microsoft/OmniParser `util/omniparser.py`, absorbed per #18 P1-1):
//!
//! 1. an [`ElementTable`] — the `{id: bbox}` mapping plus the element dicts
//!    (`kind / bbox / interactivity / content / source`);
//! 2. a set-of-mark rendering ([`render_som`]) — the screenshot annotated with
//!    boxes and numeric tags, so the model can *refer to* elements by number;
//! 3. [`ElementTable::model_view`] — the model-facing projection that carries
//!    **no coordinates at all**. The model reports a number; the engine looks
//!    the bbox up itself. "Coordinates never pass through the model's hands"
//!    is a structural guarantee here, not a prompt instruction.
//!
//! This crate defines the contract only: no detector, no OCR, no model call.
//! Later batches feed it (B2 geometric merge rules, B3 OCR anchoring, B4 the
//! accessibility-tree two-backend merge); the renderer already works today
//! with any table built by hand or by those producers.

use image::{Rgb, RgbImage};
use serde::{Deserialize, Serialize};

pub mod merge;
pub mod ocr;
pub mod registry;

/// A normalized bounding box in `0.0..=1.0` xyxy order, image-relative.
///
/// Normalized on purpose (OmniParser contract): a table survives resolution
/// and DPI changes; pixel conversion happens at the renderer/consumer edge
/// via [`Bbox::to_pixels`], not inside the table.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bbox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Bbox {
    /// Builds a normalized bbox, refusing out-of-range or inverted boxes.
    pub fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Result<Self, PerceptionError> {
        for v in [x1, y1, x2, y2] {
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err(PerceptionError::InvalidBbox);
            }
        }
        if x2 <= x1 || y2 <= y1 {
            return Err(PerceptionError::InvalidBbox);
        }
        Ok(Self { x1, y1, x2, y2 })
    }

    /// Converts to pixel coordinates on an image of the given size, clamped
    /// to the image rect. Values are rounded to the nearest pixel.
    pub fn to_pixels(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let clamp_x = |v: f32| ((v * width as f32).round() as i64).clamp(0, width as i64) as u32;
        let clamp_y = |v: f32| ((v * height as f32).round() as i64).clamp(0, height as i64) as u32;
        (
            clamp_x(self.x1),
            clamp_y(self.y1),
            clamp_x(self.x2),
            clamp_y(self.y2),
        )
    }
}

/// What kind of visual element this is.
///
/// Mirrors OmniParser's detector classes (`text`, `icon`). The accessibility
/// backend merged in batch B4 adds its own variants — exhaustive matches on
/// this enum are meant to break at compile time when that happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementKind {
    Text,
    Icon,
    /// An accessibility-tree control (batch B4): role and name come from
    /// UIA/AT-SPI — the semantic ground truth when present.
    Control,
}

/// Where an element came from — the routing layer may weight confidence by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// OCR-produced text element (batch B3).
    Ocr,
    /// Detector-produced element (batch B3+/B5).
    Detection,
    /// Product of the geometric merge rules (batch B2).
    Merge,
    /// An accessibility-tree control (batch B4): the semantic ground truth.
    Access,
}

/// One visual element: numbered, boxed, optionally labelled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    /// Stable number the model refers to. Unique within a table.
    pub id: u32,
    pub kind: ElementKind,
    pub bbox: Bbox,
    /// Whether the element looks actionable (button-like, clickable).
    pub interactivity: bool,
    /// OCR text or icon caption, when available.
    pub content: Option<String>,
    /// The control's role ("Button", "Edit", …) — `Some` only for
    /// accessibility-backed elements (batch: tool wiring).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub source: Source,
}

/// The model-facing projection of an element: **no coordinates**.
///
/// The model sees numbers and content; it never sees or emits a bbox. This
/// type has no coordinate field by construction, and the serializer cannot
/// leak one — a test pins the JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelElement {
    pub id: u32,
    pub kind: ElementKind,
    pub content: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PerceptionError {
    /// A bbox was out of `0.0..=1.0` range, non-finite, or inverted.
    #[error("invalid bbox: must be finite, within 0.0..=1.0, and x2>x1, y2>y1")]
    InvalidBbox,
    /// Two elements claimed the same id.
    #[error("duplicate element id {0}")]
    DuplicateId(u32),
}

/// An ordered set of uniquely-numbered elements over one screenshot.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ElementTable {
    elements: Vec<Element>,
}

impl ElementTable {
    /// Builds a table, refusing duplicate ids (the SoM numbering must be
    /// unambiguous — a model replying "3" must resolve to exactly one box).
    ///
    /// `id == 0` is the "unaddressable visual element" sentinel (batch:
    /// unification) and may repeat freely — it is not an address, so several
    /// `- text` lines cannot collide the way two `[12]` controls would.
    pub fn new(elements: Vec<Element>) -> Result<Self, PerceptionError> {
        let mut seen = std::collections::HashSet::new();
        for e in &elements {
            if e.id != 0 && !seen.insert(e.id) {
                return Err(PerceptionError::DuplicateId(e.id));
            }
        }
        Ok(Self { elements })
    }

    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Looks up an element by the number the model reported.
    pub fn get(&self, id: u32) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }

    /// The model-facing projection: ids and content, never coordinates.
    pub fn model_view(&self) -> Vec<ModelElement> {
        self.elements
            .iter()
            .map(|e| ModelElement {
                id: e.id,
                kind: e.kind,
                content: e.content.clone(),
            })
            .collect()
    }

    /// The prompt-facing text form, **no coordinates** — the same
    /// no-coordinates guarantee as [`Self::model_view`], in the shape an
    /// agent-loop prompt block wants. Controls carry their role ("control
    /// Button \"OK\""); text and icons degrade gracefully when they have no
    /// content.
    ///
    /// Numbering semantics (batch: unification): an element with
    /// `id > 0` carries the window_state `element_index` — one number for
    /// both reading and acting. `id == 0` marks a purely visual element
    /// (no control, no address): rendered as a `-` line **without a
    /// number**, so nothing in the prompt looks like an address it could
    /// invoke with.
    pub fn to_model_text(&self) -> String {
        self.elements
            .iter()
            .map(|e| {
                let kind = match e.kind {
                    ElementKind::Control => "control",
                    ElementKind::Text => "text",
                    ElementKind::Icon => "icon",
                };
                let role = match (&e.role, e.kind) {
                    (Some(r), ElementKind::Control) => format!(" {r}"),
                    _ => String::new(),
                };
                let label = if e.id == 0 {
                    format!("- {kind}{role}")
                } else {
                    format!("[{}] {kind}{role}", e.id)
                };
                match e.content.as_deref() {
                    Some(c) => format!("{label} \"{c}\""),
                    None => label,
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Renders the set-of-mark annotation: each element's box drawn on the
/// screenshot, with a numbered tag at the box's top-left corner.
///
/// The input image is not mutated; a copy is returned. Boxes are clamped to
/// the image rect. Tags use a built-in 3x5 pixel digit font, so rendering a
/// table with 100+ elements stays dependency-free.
pub fn render_som(screenshot: &RgbImage, table: &ElementTable) -> RgbImage {
    let mut out = screenshot.clone();
    let (w, h) = (out.width(), out.height());
    let border = Rgb([255, 0, 0]);
    let tag_bg = Rgb([0, 0, 255]);
    let tag_fg = Rgb([255, 255, 255]);

    for element in table.elements() {
        let (x1, y1, x2, y2) = element.bbox.to_pixels(w, h);
        if x2 > x1 && y2 > y1 {
            draw_rect(&mut out, x1, y1, x2, y2, border);
        }
        draw_tag(&mut out, x1, y1, element.id, tag_bg, tag_fg);
    }
    out
}

/// Draws a 1px rectangle outline (inclusive corners). Lines that fall outside
/// the image are clamped onto the edge, so a box extending past the right or
/// bottom boundary still shows a full outline on its visible part.
fn draw_rect(img: &mut RgbImage, x1: u32, y1: u32, x2: u32, y2: u32, color: Rgb<u8>) {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return;
    }
    let (x1, x2) = (x1.min(w - 1), x2.min(w - 1));
    let (y1, y2) = (y1.min(h - 1), y2.min(h - 1));
    for x in x1..=x2 {
        img.put_pixel(x, y1, color);
        img.put_pixel(x, y2, color);
    }
    for y in y1..=y2 {
        img.put_pixel(x1, y, color);
        img.put_pixel(x2, y, color);
    }
}

/// 3x5 pixel digit font, rows top-to-bottom, 3 bits per row (MSB = left).
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// Digit glyph size: 3x5 pixels, one blank column between digits.
const GLYPH_W: u32 = 3;
const GLYPH_H: u32 = 5;
const GAP_W: u32 = 1;

fn tag_size(id: u32) -> (u32, u32) {
    let digits = id.to_string().len() as u32;
    let w = digits * GLYPH_W + (digits - 1) * GAP_W;
    (w, GLYPH_H)
}

/// Draws the numbered tag with a solid background, placed just inside the
/// box's top-left corner (or just above it when the box is too short to hold
/// the tag without covering it entirely).
fn draw_tag(img: &mut RgbImage, x1: u32, y1: u32, id: u32, bg: Rgb<u8>, fg: Rgb<u8>) {
    let (w, h) = (img.width(), img.height());
    let (tw, _th) = tag_size(id);
    // One pixel of background padding around the glyphs.
    let tag_w = tw + 2;
    let tag_h = GLYPH_H + 2;
    let tx = x1.min(w.saturating_sub(tag_w));
    // Preferred: just inside the box's top-left corner; when the box sits so
    // low the tag would fall off the image, draw it just above the box.
    let ty = if y1 + tag_h <= h {
        y1
    } else {
        y1.saturating_sub(tag_h)
    };
    let ty = ty.min(h.saturating_sub(tag_h));

    // Solid background.
    for y in ty..(ty + tag_h).min(h) {
        for x in tx..(tx + tag_w).min(w) {
            img.put_pixel(x, y, bg);
        }
    }
    // Glyphs, one blank column between digits.
    let digits: Vec<usize> = id
        .to_string()
        .bytes()
        .map(|b| (b - b'0') as usize)
        .collect();
    for (di, &d) in digits.iter().enumerate() {
        let gx = tx + 1 + di as u32 * (GLYPH_W + GAP_W);
        for (row, &bits) in DIGITS[d].iter().enumerate() {
            for col in 0..GLYPH_W {
                if bits & (0b100 >> col) != 0 {
                    let px = gx + col;
                    let py = ty + 1 + row as u32;
                    if px < w && py < h {
                        img.put_pixel(px, py, fg);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(id: u32, bbox: Bbox) -> Element {
        Element {
            id,
            kind: ElementKind::Icon,
            bbox,
            interactivity: true,
            content: Some(format!("element {id}")),
            role: None,
            source: Source::Detection,
        }
    }

    #[test]
    fn to_model_text_omits_coordinates_and_carries_roles() {
        let table = ElementTable::new(vec![
            Element {
                id: 3,
                kind: ElementKind::Control,
                bbox: Bbox::new(0.1, 0.1, 0.5, 0.5).unwrap(),
                interactivity: true,
                content: Some("OK".into()),
                role: Some("Button".into()),
                source: Source::Access,
            },
            Element {
                id: 5,
                kind: ElementKind::Text,
                bbox: Bbox::new(0.0, 0.6, 0.4, 0.7).unwrap(),
                interactivity: false,
                content: Some("Open File".into()),
                role: None,
                source: Source::Ocr,
            },
            Element {
                id: 7,
                kind: ElementKind::Icon,
                bbox: Bbox::new(0.8, 0.8, 0.9, 0.9).unwrap(),
                interactivity: true,
                content: None,
                role: None,
                source: Source::Detection,
            },
        ])
        .unwrap();
        let text = table.to_model_text();
        assert_eq!(
            text,
            "[3] control Button \"OK\"\n[5] text \"Open File\"\n[7] icon"
        );
        // The no-coordinates guarantee holds in the text form too.
        assert!(!text.contains("0.1"), "leaked a coordinate: {text}");
        assert!(!text.contains("bbox"), "leaked a bbox field: {text}");
    }

    #[test]
    fn an_unaddressed_element_renders_without_a_number() {
        // Batch: unification. id 0 = unaddressable visual element — it must
        // not render as `[0]`, which would look like an invocable address.
        let table = ElementTable::new(vec![
            Element {
                id: 12,
                kind: ElementKind::Control,
                bbox: Bbox::new(0.0, 0.0, 0.3, 0.2).unwrap(),
                interactivity: true,
                content: Some("OK".into()),
                role: Some("Button".into()),
                source: Source::Access,
            },
            Element {
                id: 0,
                kind: ElementKind::Text,
                bbox: Bbox::new(0.0, 0.5, 0.4, 0.6).unwrap(),
                interactivity: false,
                content: Some("free text".into()),
                role: None,
                source: Source::Ocr,
            },
        ])
        .unwrap();
        let text = table.to_model_text();
        assert_eq!(text, "[12] control Button \"OK\"\n- text \"free text\"");
        assert!(!text.contains("[0]"), "id 0 must not render as an address");
    }

    #[test]
    fn unaddressed_elements_may_share_id_zero() {
        // The sentinel is not an address: several `-` lines coexist.
        let a = Element {
            id: 0,
            kind: ElementKind::Text,
            bbox: Bbox::new(0.0, 0.0, 0.1, 0.1).unwrap(),
            interactivity: false,
            content: Some("one".into()),
            role: None,
            source: Source::Ocr,
        };
        let b = Element {
            id: 0,
            kind: ElementKind::Text,
            bbox: Bbox::new(0.0, 0.2, 0.1, 0.3).unwrap(),
            interactivity: false,
            content: Some("two".into()),
            role: None,
            source: Source::Ocr,
        };
        assert!(ElementTable::new(vec![a, b]).is_ok(), "id 0 repeats freely");
    }

    #[test]
    fn a_bbox_out_of_range_or_inverted_is_refused() {
        assert!(Bbox::new(0.1, 0.1, 0.9, 0.9).is_ok());
        assert!(Bbox::new(-0.01, 0.1, 0.9, 0.9).is_err(), "x1 below 0");
        assert!(Bbox::new(0.1, 0.1, 1.01, 0.9).is_err(), "x2 above 1");
        assert!(Bbox::new(f32::NAN, 0.1, 0.9, 0.9).is_err(), "non-finite");
        assert!(Bbox::new(0.9, 0.1, 0.1, 0.9).is_err(), "x2 <= x1");
        assert!(Bbox::new(0.1, 0.9, 0.9, 0.9).is_err(), "y2 <= y1");
        assert!(Bbox::new(0.1, 0.1, 0.9, 0.1).is_err(), "zero height");
    }

    #[test]
    fn a_bbox_converts_to_pixels_rounded_and_clamped() {
        // 0.5 * 100 = 50 exactly; 0.335 * 10 = 3.35 -> 3.
        let b = Bbox::new(0.0, 0.0, 0.5, 0.335).unwrap();
        assert_eq!(b.to_pixels(100, 10), (0, 0, 50, 3));
        // 1.0 maps to the edge, which the clamp keeps inside the rect.
        let b = Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap();
        assert_eq!(b.to_pixels(64, 32), (0, 0, 64, 32));
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let b = Bbox::new(0.0, 0.0, 0.5, 0.5).unwrap();
        assert!(ElementTable::new(vec![elem(1, b), elem(1, b)]).is_err());
        assert!(ElementTable::new(vec![elem(1, b), elem(2, b)]).is_ok());
    }

    #[test]
    fn the_model_view_carries_no_coordinates_in_its_serialized_shape() {
        let table =
            ElementTable::new(vec![elem(3, Bbox::new(0.1, 0.1, 0.5, 0.5).unwrap())]).unwrap();
        let json = serde_json::to_string(&table.model_view()).unwrap();
        assert!(!json.contains("bbox"), "model view leaked bbox: {json}");
        assert!(!json.contains("x1"), "model view leaked x1: {json}");
        assert!(
            !json.contains("interactivity"),
            "model view leaked interactivity"
        );
        assert!(json.contains("\"id\":3"));
        assert!(json.contains("\"content\":\"element 3\""));
    }

    #[test]
    fn the_model_view_resolves_back_to_the_numbered_element() {
        let table = ElementTable::new(vec![
            elem(1, Bbox::new(0.0, 0.0, 0.2, 0.2).unwrap()),
            elem(2, Bbox::new(0.5, 0.5, 0.9, 0.9).unwrap()),
        ])
        .unwrap();
        // The loop contract: model reports a number, the engine looks the box up.
        let view = table.model_view();
        assert_eq!(view.len(), 2);
        let reported = view[1].id;
        assert_eq!(
            table.get(reported).unwrap().bbox,
            Bbox::new(0.5, 0.5, 0.9, 0.9).unwrap()
        );
        assert!(table.get(99).is_none());
    }

    #[test]
    fn elements_round_trip_through_serde() {
        let table =
            ElementTable::new(vec![elem(1, Bbox::new(0.1, 0.2, 0.3, 0.4).unwrap())]).unwrap();
        let json = serde_json::to_string(&table).unwrap();
        let back: ElementTable = serde_json::from_str(&json).unwrap();
        assert_eq!(table, back);
        // The audit-facing shape keeps the normalized bbox.
        assert!(json.contains("\"bbox\":{\"x1\":0.1"));
    }

    #[test]
    fn render_som_draws_boxes_and_tags_without_mutating_the_input() {
        let mut screenshot = RgbImage::new(100, 100);
        for y in 0..100 {
            for x in 0..100 {
                screenshot.put_pixel(x, y, Rgb([40, 40, 40]));
            }
        }
        let table = ElementTable::new(vec![
            elem(1, Bbox::new(0.1, 0.1, 0.5, 0.5).unwrap()),
            elem(23, Bbox::new(0.6, 0.6, 0.9, 0.9).unwrap()),
        ])
        .unwrap();
        let before = screenshot.clone();
        let out = render_som(&screenshot, &table);
        assert_eq!((out.width(), out.height()), (100, 100));
        assert_eq!(screenshot, before, "input must not be mutated");

        // The border of element 1 crosses (10, 30): mid-left edge of the box.
        assert_eq!(out.get_pixel(10, 30), &Rgb([255, 0, 0]), "box border drawn");
        // The tag sits just inside the top-left corner: the tag background.
        assert_eq!(
            out.get_pixel(11, 11),
            &Rgb([0, 0, 255]),
            "tag background drawn"
        );
        // Element 23's border crosses its mid-left edge.
        assert_eq!(
            out.get_pixel(60, 75),
            &Rgb([255, 0, 0]),
            "second box border drawn"
        );
    }

    #[test]
    fn render_som_clamps_boxes_that_extend_past_the_image() {
        let mut screenshot = RgbImage::new(50, 50);
        for y in 0..50 {
            for x in 0..50 {
                screenshot.put_pixel(x, y, Rgb([10, 10, 10]));
            }
        }
        // bbox extends past the right and bottom edges: clamped, no panic.
        let table =
            ElementTable::new(vec![elem(1, Bbox::new(0.8, 0.8, 1.0, 1.0).unwrap())]).unwrap();
        let out = render_som(&screenshot, &table);
        assert_eq!((out.width(), out.height()), (50, 50));
        assert_eq!(out.get_pixel(49, 49), &Rgb([255, 0, 0]));
    }

    #[test]
    fn a_multi_digit_tag_fits_inside_a_tiny_box_without_panicking() {
        let mut screenshot = RgbImage::new(20, 6);
        for y in 0..6 {
            for x in 0..20 {
                screenshot.put_pixel(x, y, Rgb([10, 10, 10]));
            }
        }
        // A 3x2-pixel box cannot hold the "123" tag: it is drawn above, clamped.
        let table =
            ElementTable::new(vec![elem(123, Bbox::new(0.1, 0.5, 0.25, 0.9).unwrap())]).unwrap();
        let out = render_som(&screenshot, &table);
        assert_eq!((out.width(), out.height()), (20, 6));
        // Somewhere in the image the tag background appeared.
        let touched = out.pixels().any(|p| p == &Rgb([0, 0, 255]));
        assert!(touched, "tag background must be drawn somewhere");
    }

    #[test]
    fn digit_glyphs_are_nonempty_and_the_font_is_well_formed() {
        for (n, glyph) in DIGITS.iter().enumerate() {
            assert_eq!(glyph.len(), 5);
            // Every digit has ink in its middle row.
            assert!(glyph[2] != 0, "digit {n} has no ink in the middle row");
            // Only the low 3 bits are used.
            for row in glyph {
                assert!(
                    row & !0b111 == 0,
                    "digit {n} row uses bits outside 3 columns"
                );
            }
        }
    }
}
