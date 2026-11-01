//! The unified element registry (issue #18 P1-1 batch B4).
//!
//! Target shape per the #18 survey's UFO section
//! (`AppControlInfoStrategy` three-step merge + `TargetRegistry` +
//! `control_filter.py` relevance trimming):
//!
//! 1. **Accessibility first.** Access-tree controls are the semantic ground
//!    truth (role, name, automation id) — every control enters the registry
//!    as-is.
//! 2. **OCR text enhances controls.** An OCR box sitting (≥80%) inside a
//!    control's box feeds its transcription into that control's `content` —
//!    the rescue for unnamed controls like the Xfce panel's Applications
//!    button, where the tree offers a box but no readable name. The absorbed
//!    OCR box does not become a standalone element (same accounting as B2).
//! 3. **Leftover OCR stands alone.** OCR boxes not absorbed by any control
//!    become their own `Text` elements.
//!
//! Output is one numbered [`ElementTable`] in **reading order** (top-to-
//! bottom, left-to-right by box origin) — the model reads a screen, not a
//! backend dump. Ids are `1..=n` in that order.
//!
//! [`filter_relevant`] then trims a large registry down for the model:
//! task keywords score each element, hits come first, and the remainder
//! fills up to the limit so a big desktop never floods the prompt.

use crate::merge::{area, is_inside, OcrBox};
use crate::{Bbox, Element, ElementKind, ElementTable, Source};

/// What the accessibility side hands over (UIA/AT-SPI), already normalized:
/// box in `0.0..=1.0`, role string, and the control's name if the tree
/// exposes one.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessElement {
    pub bbox: Bbox,
    /// Control role as the tree reports it ("Button", "Edit", …).
    pub role: String,
    /// The control's own name — often empty for graphic-only controls.
    pub name: Option<String>,
    /// Whether the control is actionable per the tree.
    pub interactive: bool,
}

/// Merges accessibility controls and OCR text boxes into one registry.
///
/// `access` and `ocr` may each be empty; the merge degrades to the other
/// side. Output order is reading order (see module docs).
pub fn merge_access_and_ocr(
    access: &[AccessElement],
    ocr: &[OcrBox],
) -> Result<ElementTable, crate::PerceptionError> {
    // Step 2: OCR text absorbed by the control whose box contains it.
    let mut absorbed: Vec<usize> = Vec::new();
    let mut enhanced: Vec<Option<String>> = vec![None; access.len()];
    for (k, o) in ocr.iter().enumerate() {
        // One OCR box feeds at most one control: the smallest containing one.
        let mut taker: Option<usize> = None;
        for (i, a) in access.iter().enumerate() {
            if is_inside(&o.bbox, &a.bbox) {
                taker = Some(match taker {
                    None => i,
                    Some(best) => {
                        if area(&a.bbox) < area(&access[best].bbox) {
                            i
                        } else {
                            best
                        }
                    }
                });
            }
        }
        if let Some(i) = taker {
            absorbed.push(k);
            let slot = &mut enhanced[i];
            *slot = Some(match slot.take() {
                None => o.content.clone(),
                Some(existing) => format!("{existing} {}", o.content),
            });
        }
    }

    let mut elements: Vec<Element> = Vec::new();
    for (i, a) in access.iter().enumerate() {
        elements.push(Element {
            id: 0,
            kind: ElementKind::Control,
            bbox: a.bbox,
            interactivity: a.interactive,
            content: enhanced[i].clone().or_else(|| a.name.clone()),
            role: Some(a.role.clone()),
            source: Source::Access,
        });
    }
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
            role: None,
            source: Source::Ocr,
        });
    }

    // Reading order: top-to-bottom, then left-to-right (stable for ties).
    elements.sort_by(|a, b| {
        a.bbox
            .y1
            .partial_cmp(&b.bbox.y1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.bbox
                    .x1
                    .partial_cmp(&b.bbox.x1)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    for (n, e) in elements.iter_mut().enumerate() {
        e.id = (n + 1) as u32;
    }
    ElementTable::new(elements)
}

/// Trims a registry for the model: elements whose text matches task keywords
/// score higher; hits come first (score desc, then reading order), the rest
/// fills up to `limit` so the table never goes empty on a keyword miss.
/// An empty/blank task returns the first `limit` elements unchanged.
pub fn filter_relevant(table: &ElementTable, task: &str, limit: usize) -> ElementTable {
    let keywords: Vec<String> = task
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= 2)
        .collect();
    let mut scored: Vec<(usize, &Element)> = table.elements().iter().enumerate().collect();
    if !keywords.is_empty() {
        scored.sort_by(|(ia, a), (ib, b)| {
            let sa = relevance_score(a, &keywords);
            let sb = relevance_score(b, &keywords);
            sb.cmp(&sa).then(ia.cmp(ib))
        });
    }
    let kept: Vec<Element> = scored
        .into_iter()
        .take(limit)
        .map(|(_, e)| e.clone())
        .collect();
    renumber(kept)
}

fn relevance_score(e: &Element, keywords: &[String]) -> usize {
    let content = e
        .content
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_default();
    keywords
        .iter()
        .filter(|k| content.contains(k.as_str()))
        .count()
}

fn renumber(elements: Vec<Element>) -> ElementTable {
    let mut elements = elements;
    for (n, e) in elements.iter_mut().enumerate() {
        e.id = (n + 1) as u32;
    }
    ElementTable::new(elements).expect("renumber keeps ids unique")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::OcrBox;

    fn control(
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        role: &str,
        name: Option<&str>,
    ) -> AccessElement {
        AccessElement {
            bbox: Bbox::new(x1, y1, x2, y2).unwrap(),
            role: role.to_string(),
            name: name.map(|s| s.to_string()),
            interactive: true,
        }
    }

    fn ocr_box(x1: f32, y1: f32, x2: f32, y2: f32, text: &str) -> OcrBox {
        OcrBox {
            bbox: Bbox::new(x1, y1, x2, y2).unwrap(),
            content: text.to_string(),
            confidence: 1.0,
        }
    }

    #[test]
    fn access_controls_enter_the_registry_as_control_elements() {
        let table = merge_access_and_ocr(&[control(0.0, 0.0, 0.3, 0.2, "Button", Some("OK"))], &[])
            .unwrap();
        assert_eq!(table.len(), 1);
        let e = &table.elements()[0];
        assert_eq!(e.kind, ElementKind::Control);
        assert_eq!(e.source, Source::Access);
        assert_eq!(e.content.as_deref(), Some("OK"));
        assert!(e.interactivity);
        assert_eq!(e.id, 1);
    }

    #[test]
    fn ocr_inside_an_unnamed_control_becomes_its_content() {
        // The Xfce panel shape: the tree offers a box but no readable name.
        let table = merge_access_and_ocr(
            &[control(0.0, 0.0, 0.2, 0.1, "PanelButton", None)],
            &[ocr_box(0.02, 0.02, 0.18, 0.08, "Applications")],
        )
        .unwrap();
        assert_eq!(table.len(), 1, "absorbed OCR is not a standalone element");
        assert_eq!(table.elements()[0].content.as_deref(), Some("Applications"));
        assert_eq!(table.elements()[0].source, Source::Access);
    }

    #[test]
    fn an_ocr_box_outside_every_control_stands_alone() {
        let table = merge_access_and_ocr(
            &[control(0.0, 0.0, 0.2, 0.2, "Button", Some("OK"))],
            &[ocr_box(0.5, 0.5, 0.9, 0.6, "Free text")],
        )
        .unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(table.elements()[1].kind, ElementKind::Text);
        assert_eq!(table.elements()[1].source, Source::Ocr);
        assert!(!table.elements()[1].interactivity);
    }

    #[test]
    fn a_partially_overlapping_ocr_box_is_not_absorbed() {
        // Only ~25% of the OCR box sits inside the control: below 80%.
        let table = merge_access_and_ocr(
            &[control(0.8, 0.0, 1.0, 1.0, "Button", None)],
            &[ocr_box(0.7, 0.1, 0.9, 0.2, "edge")],
        )
        .unwrap();
        let texts: Vec<_> = table.elements().iter().map(|e| e.kind).collect();
        assert_eq!(texts, vec![ElementKind::Control, ElementKind::Text]);
        assert_eq!(table.elements()[0].content.as_deref(), None, "not absorbed");
    }

    #[test]
    fn ocr_spanning_two_controls_feeds_the_smaller_container() {
        // A big panel control and a small button both contain the OCR box;
        // the smaller (more specific) container wins.
        let table = merge_access_and_ocr(
            &[
                control(0.0, 0.0, 1.0, 1.0, "Panel", None),
                control(0.1, 0.1, 0.3, 0.3, "Button", None),
            ],
            &[ocr_box(0.12, 0.12, 0.28, 0.28, "Go")],
        )
        .unwrap();
        let button = table
            .elements()
            .iter()
            .find(|e| e.content.as_deref() == Some("Go"))
            .expect("smaller container carries the text");
        assert_eq!(button.bbox, Bbox::new(0.1, 0.1, 0.3, 0.3).unwrap());
        let panel = table
            .elements()
            .iter()
            .find(|e| e.bbox == Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap())
            .unwrap();
        assert_eq!(
            panel.content.as_deref(),
            None,
            "panel did not also absorb it"
        );
    }

    #[test]
    fn output_is_in_reading_order_with_sequential_ids() {
        let table = merge_access_and_ocr(
            &[
                control(0.5, 0.6, 0.9, 0.8, "Button", Some("B")),
                control(0.0, 0.0, 0.3, 0.2, "Edit", Some("A")),
            ],
            &[ocr_box(0.0, 0.5, 0.4, 0.6, "middle text")],
        )
        .unwrap();
        let order: Vec<u32> = table.elements().iter().map(|e| e.id).collect();
        // A (top-left), middle text, B (bottom-right).
        assert_eq!(order, vec![1, 2, 3]);
        assert_eq!(table.elements()[0].content.as_deref(), Some("A"));
        assert_eq!(table.elements()[2].content.as_deref(), Some("B"));
    }

    #[test]
    fn empty_inputs_degrade_either_way() {
        assert!(merge_access_and_ocr(&[], &[]).unwrap().is_empty());
        let only_ocr = merge_access_and_ocr(&[], &[ocr_box(0.0, 0.0, 0.5, 0.5, "solo")]).unwrap();
        assert_eq!(only_ocr.len(), 1);
        assert_eq!(only_ocr.elements()[0].kind, ElementKind::Text);
        let only_access =
            merge_access_and_ocr(&[control(0.0, 0.0, 0.5, 0.5, "Edit", None)], &[]).unwrap();
        assert_eq!(only_access.len(), 1);
        assert_eq!(only_access.elements()[0].kind, ElementKind::Control);
    }

    #[test]
    fn filter_relevant_puts_keyword_hits_first_and_fills_to_the_limit() {
        let table = merge_access_and_ocr(
            &[
                control(0.0, 0.0, 0.2, 0.1, "Edit", Some("username field")),
                control(0.0, 0.2, 0.2, 0.3, "Button", Some("OK")),
                control(0.0, 0.4, 0.2, 0.5, "Edit", Some("password")),
                control(0.0, 0.6, 0.2, 0.7, "Button", Some("Cancel")),
            ],
            &[],
        )
        .unwrap();
        let trimmed = filter_relevant(&table, "password", 2);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed.elements()[0].content.as_deref(), Some("password"));
        // Score order: hits first, then reading order fills the rest.
        assert_eq!(
            trimmed.elements()[1].content.as_deref(),
            Some("username field")
        );
        let ids: Vec<u32> = trimmed.elements().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2], "renumbered after the trim");
    }

    #[test]
    fn a_keyword_miss_still_returns_elements_up_to_the_limit() {
        let table = merge_access_and_ocr(
            &[
                control(0.0, 0.0, 0.2, 0.1, "Button", Some("OK")),
                control(0.0, 0.2, 0.2, 0.3, "Button", Some("Cancel")),
            ],
            &[],
        )
        .unwrap();
        let trimmed = filter_relevant(&table, "totally unrelated words", 1);
        assert_eq!(trimmed.len(), 1, "fix the flood, not the desert");
        assert_eq!(trimmed.elements()[0].content.as_deref(), Some("OK"));
    }

    #[test]
    fn a_blank_task_returns_the_table_head_unchanged() {
        let table = merge_access_and_ocr(
            &[
                control(0.0, 0.0, 0.2, 0.1, "Edit", Some("first")),
                control(0.0, 0.2, 0.2, 0.3, "Edit", Some("second")),
            ],
            &[],
        )
        .unwrap();
        let trimmed = filter_relevant(&table, "   ", 1);
        assert_eq!(trimmed.elements()[0].content.as_deref(), Some("first"));
    }
}
