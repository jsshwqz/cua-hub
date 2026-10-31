//! Geometric merge rules (issue #18 P1-1 batch B2).
//!
//! A faithful port of OmniParser's `remove_overlap_new`
//! (microsoft/OmniParser@3540212 `util/utils.py:241`, called with the default
//! `iou_threshold=0.9` from `get_som_labeled_img`), adapted to the B1
//! contract: boxes are normalized [`Bbox`]es, the output is an
//! [`ElementTable`], and each element's [`Source`] says which rule produced it.
//!
//! Rules, in the original order:
//!
//! 1. **Keep the smaller box.** An icon box is dropped when some other icon
//!    box has containment-aware IoU above the threshold with it *and* is
//!    smaller. Containment-aware IoU is `max(standard IoU, intersection/area1,
//!    intersection/area2)` — a box almost fully inside a bigger one counts as
//!    overlapping even when the standard IoU is small.
//! 2. **OCR text is the ground truth.** An OCR box that sits (≥80% of its
//!    area) inside a surviving icon box is absorbed: its text becomes the
//!    icon's `content` and the OCR box disappears as a standalone element.
//! 3. **An icon inside an OCR box is a false positive.** If ≥80% of the icon
//!    sits inside an OCR box, the icon is dropped (and the remaining OCR
//!    boxes are not checked — OCR boxes do not overlap each other, so an
//!    icon can only be inside one of them; the original relies on this and
//!    breaks early).
//!
//! Two deliberate deviations from the Python original, both pinned by tests:
//!
//! - The original accumulates the same OCR text twice when two icon boxes
//!   both try to absorb it (its `filtered_boxes.remove` raises for the
//!   second taker and the bare `except` swallows it *after* the label was
//!   already appended). An absorbed-set fixes that; a test pins
//!   "absorbed once, never twice".
//! - The original joins absorbed labels with a trailing space
//!   (`content + ' '` per box); this port joins with single spaces and no
//!   trailing whitespace.

use crate::{Bbox, Element, ElementKind, ElementTable, Source};

/// What the detector side hands over: an icon box with no text yet.
#[derive(Debug, Clone, PartialEq)]
pub struct IconBox {
    pub bbox: Bbox,
}

/// What the OCR side hands over: a text box with its transcription.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrBox {
    pub bbox: Bbox,
    pub content: String,
}

/// Thresholds for [`remove_overlap`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergeConfig {
    /// Containment-aware IoU above which two icon boxes count as duplicates.
    /// The OmniParser default is 0.9.
    pub iou_threshold: f32,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self { iou_threshold: 0.9 }
    }
}

/// Ported from the original's hardcoded `is_inside`: true when more than 80%
/// of `inner`'s area lies inside `outer`.
const INSIDE_THRESHOLD: f32 = 0.80;

fn area(b: &Bbox) -> f32 {
    (b.x2 - b.x1) * (b.y2 - b.y1)
}

fn intersection_area(a: &Bbox, b: &Bbox) -> f32 {
    let x1 = a.x1.max(b.x1);
    let y1 = a.y1.max(b.y1);
    let x2 = a.x2.min(b.x2);
    let y2 = a.y2.min(b.y2);
    (x2 - x1).max(0.0) * (y2 - y1).max(0.0)
}

/// Containment-aware IoU: `max(standard IoU, intersection/area(a),
/// intersection/area(b))`. The `1e-6` guard on the union is from the
/// original. Both boxes have strictly positive area by the B1 contract
/// (`Bbox::new` refuses degenerate boxes), so no division by zero.
fn containment_iou(a: &Bbox, b: &Bbox) -> f32 {
    let intersection = intersection_area(a, b);
    let union = area(a) + area(b) - intersection + 1e-6;
    let ratio_a = intersection / area(a);
    let ratio_b = intersection / area(b);
    (intersection / union).max(ratio_a).max(ratio_b)
}

/// True when more than 80% of `inner`'s area lies inside `outer`.
fn is_inside(inner: &Bbox, outer: &Bbox) -> bool {
    intersection_area(inner, outer) / area(inner) > INSIDE_THRESHOLD
}

/// Applies the merge rules and returns a numbered element table.
///
/// Output order matches the original: every not-absorbed OCR box first (in
/// input order), then every surviving icon box (in input order). Ids are
/// assigned `1..=n` in that order.
pub fn remove_overlap(
    icons: &[IconBox],
    ocr: &[OcrBox],
    config: &MergeConfig,
) -> Result<ElementTable, crate::PerceptionError> {
    // Rule 1: an icon dies when a *smaller* icon overlaps it past the
    // threshold. `survivors` keeps input order.
    let mut survivors: Vec<usize> = Vec::with_capacity(icons.len());
    for (i, icon) in icons.iter().enumerate() {
        let killed = icons.iter().enumerate().any(|(j, other)| {
            j != i
                && containment_iou(&icon.bbox, &other.bbox) > config.iou_threshold
                && area(&icon.bbox) > area(&other.bbox)
        });
        if !killed {
            survivors.push(i);
        }
    }

    // Rule 2 + 3: fuse each surviving icon with the OCR boxes around it.
    // `absorbed` is the deliberate fix for the original's double-count (see
    // module docs): one OCR box feeds at most one icon's content.
    let mut absorbed: Vec<usize> = Vec::new();
    let mut fused_icons: Vec<(usize, Option<String>, Source)> = Vec::new();
    for &i in &survivors {
        let icon = &icons[i];
        let mut labels: Vec<&str> = Vec::new();
        let mut swallowed = false;
        for (k, o) in ocr.iter().enumerate() {
            if is_inside(&o.bbox, &icon.bbox) {
                // OCR text inside the icon: the transcription is the truth.
                if !absorbed.contains(&k) {
                    labels.push(&o.content);
                    absorbed.push(k);
                }
            } else if is_inside(&icon.bbox, &o.bbox) {
                // Icon inside an OCR box: a detector false positive.
                swallowed = true;
                break;
            }
        }
        if swallowed {
            continue;
        }
        let (content, source) = if labels.is_empty() {
            (None, Source::Detection)
        } else {
            (Some(labels.join(" ")), Source::Merge)
        };
        fused_icons.push((i, content, source));
    }

    let mut elements: Vec<Element> = Vec::new();
    for (k, o) in ocr.iter().enumerate() {
        if absorbed.contains(&k) {
            continue;
        }
        elements.push(Element {
            id: 0,
            kind: ElementKind::Text,
            bbox: o.bbox,
            interactivity: false,
            content: Some(o.content.clone()),
            source: Source::Ocr,
        });
    }
    for (i, content, source) in fused_icons {
        elements.push(Element {
            id: 0,
            kind: ElementKind::Icon,
            bbox: icons[i].bbox,
            interactivity: true,
            content,
            source,
        });
    }
    for (n, e) in elements.iter_mut().enumerate() {
        e.id = (n + 1) as u32;
    }
    ElementTable::new(elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(x1: f32, y1: f32, x2: f32, y2: f32) -> IconBox {
        IconBox {
            bbox: Bbox::new(x1, y1, x2, y2).unwrap(),
        }
    }

    fn ocr(x1: f32, y1: f32, x2: f32, y2: f32, text: &str) -> OcrBox {
        OcrBox {
            bbox: Bbox::new(x1, y1, x2, y2).unwrap(),
            content: text.to_string(),
        }
    }

    #[test]
    fn a_larger_icon_overlapping_a_smaller_one_is_dropped() {
        // The small box is fully inside the big one: containment IoU hits 1.0
        // (the intersection/area(small) ratio), past the 0.9 threshold — the
        // *bigger* side dies, the smaller survives.
        let icons = vec![icon(0.1, 0.1, 0.9, 0.9), icon(0.2, 0.2, 0.4, 0.4)];
        let table = remove_overlap(&icons, &[], &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1, "big duplicate dropped, small kept");
        assert_eq!(
            table.elements()[0].bbox,
            Bbox::new(0.2, 0.2, 0.4, 0.4).unwrap()
        );
        assert_eq!(table.elements()[0].source, Source::Detection);
    }

    #[test]
    fn two_icons_that_barely_overlap_both_survive() {
        // Standard IoU 0.1*0.1/... and containment ratios both far below 0.9.
        let icons = vec![icon(0.0, 0.0, 0.5, 0.5), icon(0.45, 0.0, 0.95, 0.5)];
        let table = remove_overlap(&icons, &[], &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 2, "light overlap is not a duplicate");
    }

    #[test]
    fn the_area_comparison_alone_decides_when_containment_iou_fires() {
        // containment_iou is symmetric (max includes both ratios), so once it
        // fires the bigger box always dies — even when the small box is the
        // one "mostly inside" by ratio.
        let icons = vec![icon(0.0, 0.0, 1.0, 1.0), icon(0.4, 0.4, 0.6, 0.6)];
        let table = remove_overlap(&icons, &[], &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.elements()[0].bbox,
            Bbox::new(0.4, 0.4, 0.6, 0.6).unwrap()
        );
    }

    #[test]
    fn ocr_text_inside_an_icon_becomes_the_icon_content_and_vanishes_as_a_box() {
        // Icon 0.0..0.6; OCR box 0.1..0.5 sits fully inside (100% ≥ 80%).
        let icons = vec![icon(0.0, 0.0, 0.6, 0.6)];
        let ocr = vec![ocr(0.1, 0.1, 0.5, 0.5, "Save")];
        let table = remove_overlap(&icons, &ocr, &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1, "absorbed OCR box is gone as a standalone");
        let e = table.elements()[0].clone();
        assert_eq!(e.kind, ElementKind::Icon);
        assert_eq!(e.content.as_deref(), Some("Save"));
        assert_eq!(e.source, Source::Merge);
        assert!(e.interactivity);
    }

    #[test]
    fn an_icon_mostly_inside_an_ocr_box_is_dropped_as_a_false_positive() {
        // OCR 0.0..1.0 wide bar; icon 0.4..0.5 fully inside it.
        let icons = vec![icon(0.4, 0.4, 0.5, 0.5)];
        let ocr = vec![ocr(0.0, 0.0, 1.0, 1.0, "toolbar")];
        let table = remove_overlap(&icons, &ocr, &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.elements()[0].kind,
            ElementKind::Text,
            "icon dropped, OCR kept"
        );
        assert_eq!(table.elements()[0].source, Source::Ocr);
    }

    #[test]
    fn the_same_ocr_text_is_absorbed_once_even_with_two_takers() {
        // Two equal-area icons both contain the same OCR box. The original
        // would append the text twice (its remove raises for the second
        // taker and the bare except swallows it — but only after += ran).
        // Equal areas so rule 1 kills neither (strictly-greater required).
        let icons = vec![icon(0.0, 0.0, 0.4, 0.4), icon(0.1, 0.0, 0.5, 0.4)];
        let ocr = vec![ocr(0.15, 0.1, 0.35, 0.3, "Open")];
        let table = remove_overlap(&icons, &ocr, &MergeConfig::default()).unwrap();
        let merged: Vec<&str> = table
            .elements()
            .iter()
            .filter_map(|e| e.content.as_deref())
            .filter(|s| s.contains("Open"))
            .collect();
        assert_eq!(merged.len(), 1, "text absorbed once, not once per taker");
        assert_eq!(table.len(), 2, "one icon carries the text, one icon empty");
    }

    #[test]
    fn ocr_boxes_below_the_inside_threshold_stay_standalone() {
        // OCR box only 40% inside the icon: not absorbed, stays as its own
        // text element.
        let icons = vec![icon(0.5, 0.0, 1.0, 1.0)];
        let ocr = vec![ocr(0.3, 0.0, 0.8, 1.0, "half in")]; // 50% inside
        let table = remove_overlap(&icons, &ocr, &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 2, "neither rule fired at 50% containment");
        assert_eq!(table.elements()[0].kind, ElementKind::Text);
        assert_eq!(table.elements()[1].kind, ElementKind::Icon);
        assert_eq!(table.elements()[1].content, None);
    }

    #[test]
    fn empty_inputs_degrade_to_a_plain_passthrough() {
        let table = remove_overlap(&[], &[], &MergeConfig::default()).unwrap();
        assert!(table.is_empty());

        let icons = vec![icon(0.0, 0.0, 0.5, 0.5)];
        let table = remove_overlap(&icons, &[], &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.elements()[0].source, Source::Detection);

        let ocr = vec![ocr(0.0, 0.0, 0.5, 0.5, "only text")];
        let table = remove_overlap(&[], &ocr, &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.elements()[0].kind, ElementKind::Text);
        assert!(!table.elements()[0].interactivity);
    }

    #[test]
    fn output_is_ordered_ocr_first_then_icons_with_sequential_ids() {
        let icons = vec![icon(0.7, 0.7, 0.9, 0.9), icon(0.0, 0.0, 0.2, 0.2)];
        let ocr = vec![
            ocr(0.75, 0.75, 0.85, 0.85, "tag"),
            ocr(0.0, 0.3, 0.1, 0.4, "note"),
        ];
        // "tag" is inside the first icon → absorbed; "note" stays standalone.
        let table = remove_overlap(&icons, &ocr, &MergeConfig::default()).unwrap();
        let kinds: Vec<(ElementKind, &str)> = table
            .elements()
            .iter()
            .map(|e| (e.kind, e.content.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (ElementKind::Text, "note"),
                (ElementKind::Icon, "tag"), // first icon, absorbed text
                (ElementKind::Icon, ""),    // second icon, no text
            ]
        );
        let ids: Vec<u32> = table.elements().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3], "ids are 1..=n in output order");
    }

    #[test]
    fn multiple_ocr_boxes_inside_one_icon_are_joined_with_single_spaces() {
        let icons = vec![icon(0.0, 0.0, 0.8, 0.8)];
        let ocr = vec![
            ocr(0.05, 0.1, 0.2, 0.2, "File"),
            ocr(0.25, 0.1, 0.4, 0.2, "Edit"),
        ];
        let table = remove_overlap(&icons, &ocr, &MergeConfig::default()).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.elements()[0].content.as_deref(), Some("File Edit"));
        assert_eq!(table.elements()[0].source, Source::Merge);
    }
}
