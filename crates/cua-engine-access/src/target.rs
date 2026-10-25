//! Identity addressing: what an action means when it says which control it wants.
//!
//! Indexes are a display convenience -- they renumber under you, and a stale index
//! silently points at a different control. A `Target` says what the caller actually
//! meant (a name, optionally a role, optionally which one of several) and is resolved
//! against the live tree at the moment of the action. The one rule this module exists
//! to enforce: **an ambiguous request is an error, never a coin flip**. Taking the
//! first match is what turns a slightly-wrong guess into a silent click on something
//! else.

use serde::Deserialize;

/// What a caller says about the control it means.
///
/// Unknown fields are rejected on purpose: `{"name": "ok", "index": 4}` is exactly the
/// double address that used to let a stale index win, and ignoring the `index` would
/// leave the caller believing it had been considered.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Case-insensitive substring of the accessible name, or a pattern when it starts
    /// with `re:` (matched case-insensitively, unanchored unless you anchor it).
    pub name: String,
    /// The control type it must be, e.g. `Button`. Case-insensitive exact match.
    #[serde(default)]
    pub role: Option<String>,
    /// Which of several matches to take, **0-based**. Absent means "must be unique".
    #[serde(default)]
    pub nth: Option<usize>,
    /// Only consider controls inside something whose name matches this, which is how
    /// two identical labels in different panels are told apart.
    #[serde(default)]
    pub container: Option<String>,
}

impl Target {
    /// The shorthand every name-addressed tool already accepted: a name, nothing else.
    pub fn by_name(name: &str) -> Self {
        Target {
            name: name.to_string(),
            role: None,
            nth: None,
            container: None,
        }
    }
}

/// What is known about one candidate at the moment it is being considered.
pub struct Facts<'a> {
    pub name: &'a str,
    /// The control type as the tree reports it (`Button`, `CheckBox`, ...).
    pub role: &'a str,
    /// Enclosing groups, **nearest first**: a `container` matches if any of them does.
    pub ancestors: &'a [String],
}

impl Facts<'_> {
    /// How a candidate is named back to the caller when nothing matched or too many did.
    pub fn describe(&self) -> String {
        let inside = self
            .ancestors
            .first()
            .map(|a| format!(", in {a:?}"))
            .unwrap_or_default();
        format!("{} {:?}{inside}", self.role, self.name)
    }
}

/// Why a target did not resolve. Each variant says what to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Miss {
    /// Nothing in the tree carried that name (and role, and container).
    NotFound {
        searched: String,
        /// Near misses: same role, or same container, so the caller can correct itself.
        nearby: Vec<String>,
    },
    /// More than one candidate survived every filter. `nth` is the way out.
    Ambiguous { candidates: Vec<String> },
    /// `nth` was beyond what was found.
    NthOutOfRange { found: usize, asked: usize },
    /// An `re:` pattern that does not compile.
    BadPattern { pattern: String, reason: String },
}

impl Miss {
    /// One line for a tool error. The point is that the caller can act on it: the
    /// candidates are listed rather than described in prose.
    pub fn to_reason(&self) -> String {
        match self {
            Miss::NotFound { searched, nearby } => {
                let hints = if nearby.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; nothing matched, but these were close: {}",
                        nearby.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
                    )
                };
                format!("no control matches {searched}{hints}")
            }
            Miss::Ambiguous { candidates } => format!(
                "{} controls match, which is not one of them: {} -- narrow with role or container, or name one with nth (0-based)",
                candidates.len(),
                candidates.iter().take(8).cloned().collect::<Vec<_>>().join("; ")
            ),
            Miss::NthOutOfRange { found, asked } => format!(
                "nth {asked} is beyond the {found} match{} found",
                if *found == 1 { "" } else { "es" }
            ),
            Miss::BadPattern { pattern, reason } => {
                format!("invalid name pattern {pattern:?}: {reason}")
            }
        }
    }

    /// Does this miss deserve a fresh look at the screen before being reported?
    /// Only `NotFound` does: the tree may have been mid-render. An ambiguous or out
    /// of range answer is stable, and re-walking is both a lie and a wasted second.
    pub fn worth_one_retry(&self) -> bool {
        matches!(self, Miss::NotFound { .. })
    }
}

/// Which name test a pattern asks for.
enum NameTest {
    Contains(String),
    Regex(regex::Regex),
}

fn name_test(pattern: &str) -> Result<NameTest, Miss> {
    if let Some(re) = pattern.strip_prefix("re:") {
        return regex::RegexBuilder::new(re)
            .case_insensitive(true)
            .build()
            .map(NameTest::Regex)
            .map_err(|e| Miss::BadPattern {
                pattern: pattern.to_string(),
                reason: e.to_string(),
            });
    }
    Ok(NameTest::Contains(pattern.to_lowercase()))
}

impl NameTest {
    fn holds(&self, haystack: &str) -> bool {
        match self {
            NameTest::Contains(needle) => haystack.to_lowercase().contains(needle),
            NameTest::Regex(re) => re.is_match(haystack),
        }
    }
}

fn matches_one(facts: &Facts, target: &Target, name: &NameTest, container: &NameTest) -> bool {
    if !name.holds(facts.name) {
        return false;
    }
    if let Some(role) = &target.role {
        if !role.is_empty() && !facts.role.eq_ignore_ascii_case(role) {
            return false;
        }
    }
    if let Some(needle) = &target.container {
        if !needle.is_empty() && !facts.ancestors.iter().any(|a| container.holds(a)) {
            return false;
        }
    }
    true
}

/// Pick the one candidate a `Target` refers to, or say why it cannot.
///
/// `facts` is in tree order. Nothing here walks or acts -- this is the decision, so
/// the two platforms cannot drift apart on when a match counts as ambiguous.
pub fn select(facts: &[Facts<'_>], target: &Target) -> Result<usize, Miss> {
    let name = name_test(&target.name)?;
    let container = name_test(target.container.as_deref().unwrap_or(""))?;

    let mut hits: Vec<usize> = Vec::new();
    let mut nearby: Vec<String> = Vec::new();
    for (index, fact) in facts.iter().enumerate() {
        if matches_one(fact, target, &name, &container) {
            hits.push(index);
            continue;
        }
        // Worth mentioning only when the name is the part that matched: that is the
        // case where a missing `role` or `container` is the caller's real problem.
        if name.holds(fact.name) && nearby.len() < 6 {
            nearby.push(fact.describe());
        }
    }

    if hits.is_empty() {
        return Err(Miss::NotFound {
            searched: describe_target(target),
            nearby,
        });
    }
    // `nth` is the caller naming one of several explicitly. It is still checked
    // against what was found, so a wrong count is an error rather than a different
    // control.
    if let Some(asked) = target.nth {
        return hits.get(asked).copied().ok_or(Miss::NthOutOfRange {
            found: hits.len(),
            asked,
        });
    }
    if hits.len() == 1 {
        return Ok(hits[0]);
    }
    Err(Miss::Ambiguous {
        candidates: hits.iter().take(8).map(|i| facts[*i].describe()).collect(),
    })
}

/// What the caller asked for, as one phrase for the error message.
pub fn describe_target(target: &Target) -> String {
    let mut out = format!("{:?}", target.name);
    if let Some(role) = &target.role {
        out.push_str(&format!(" as {role}"));
    }
    if let Some(container) = &target.container {
        out.push_str(&format!(" inside {container:?}"));
    }
    if let Some(nth) = target.nth {
        out.push_str(&format!(", nth {nth}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NONE: &[String] = &[];

    fn facts(list: &[(&str, &str)]) -> Vec<(String, String, Vec<String>)> {
        list.iter()
            .map(|(role, name)| (role.to_string(), name.to_string(), Vec::new()))
            .collect()
    }

    fn view(held: &[(String, String, Vec<String>)]) -> Vec<Facts<'_>> {
        held.iter()
            .map(|(role, name, ancestors)| Facts {
                name,
                role,
                ancestors,
            })
            .collect()
    }

    fn select_in(held: &[(String, String, Vec<String>)], target: &Target) -> Result<usize, Miss> {
        select(&view(held), target)
    }

    #[test]
    fn a_unique_name_resolves_and_case_does_not_matter() {
        let held = facts(&[("Button", "Send report"), ("Text", "path")]);
        let target = Target::by_name("send REPORT");
        assert_eq!(select_in(&held, &target), Ok(0));
    }

    #[test]
    fn two_matches_are_an_error_and_the_first_one_is_not_taken() {
        // This is the whole reason the module exists. "Pick one" used to mean "pick
        // the first", which is how a slightly-wrong guess became a silent click on a
        // different control.
        let held = facts(&[("Button", "OK"), ("Checkbox", "OK")]);
        let miss = select_in(&held, &Target::by_name("ok")).unwrap_err();
        match &miss {
            Miss::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected an ambiguity, got {other:?}"),
        }
        assert!(miss.to_reason().contains("nth"), "{}", miss.to_reason());
    }

    #[test]
    fn a_role_filter_ends_the_ambiguity_instead_of_a_coin_flip() {
        let held = facts(&[("Text", "probe-ambiguous"), ("Button", "probe-ambiguous")]);
        let mut target = Target::by_name("probe-ambiguous");
        target.role = Some("Button".into());
        // The label comes first in tree order; the caller means the button.
        assert_eq!(select_in(&held, &target), Ok(1));
        target.role = Some("Nonexistent".into());
        let miss = select_in(&held, &target).unwrap_err();
        assert!(matches!(miss, Miss::NotFound { .. }), "{miss:?}");
        let reason = miss.to_reason();
        assert!(
            reason.contains("Text \"probe-ambiguous\"")
                && reason.contains("Button \"probe-ambiguous\""),
            "both same-name candidates are what the caller needs to see: {reason}"
        );
    }

    #[test]
    fn a_not_found_answer_shows_the_near_misses_it_can_act_on() {
        let held = facts(&[("Button", "Send report"), ("Text", "path")]);
        let mut target = Target::by_name("send");
        target.role = Some("Text".into());
        let miss = select_in(&held, &target).unwrap_err();
        let reason = miss.to_reason();
        assert!(
            reason.contains("Button \"Send report\""),
            "the same-name, other-role candidate is the useful hint: {reason}"
        );
        assert!(
            miss.worth_one_retry(),
            "a mid-render tree deserves one fresh look"
        );
    }

    #[test]
    fn nth_names_one_of_several_explicitly() {
        let held = facts(&[("Button", "row"), ("Button", "row"), ("Button", "row")]);
        let mut target = Target::by_name("row");
        target.nth = Some(1);
        assert_eq!(select_in(&held, &target), Ok(1));
        target.nth = Some(0);
        assert_eq!(select_in(&held, &target), Ok(0));
        target.nth = Some(3);
        assert_eq!(
            select_in(&held, &target),
            Err(Miss::NthOutOfRange { found: 3, asked: 3 })
        );
    }

    #[test]
    fn nth_beyond_a_single_match_is_not_silently_accepted() {
        let held = facts(&[("Button", "Send")]);
        let mut target = Target::by_name("send");
        target.nth = Some(4);
        assert_eq!(
            select_in(&held, &target),
            Err(Miss::NthOutOfRange { found: 1, asked: 4 })
        );
    }

    #[test]
    fn a_container_separates_two_panels_that_say_the_same_thing() {
        let held: Vec<(String, String, Vec<String>)> = vec![
            ("Button".into(), "Settings".into(), vec!["Toolbar".into()]),
            (
                "Button".into(),
                "Settings".into(),
                vec!["System tray".into()],
            ),
        ];
        let mut target = Target::by_name("settings");
        target.container = Some("system tray".into());
        assert_eq!(select_in(&held, &target), Ok(1));
        // Nearest-first matters: the outer groups must not be the only ones consulted.
        target.container = Some("tool".into());
        assert_eq!(select_in(&held, &target), Ok(0));
    }

    #[test]
    fn a_regex_names_the_exact_control_when_a_substring_would_hit_several() {
        let held = facts(&[
            ("Button", "Save draft"),
            ("Button", "Save and close"),
            ("Button", "Saved"),
        ]);
        let target = Target::by_name("re:^save$");
        assert!(matches!(
            select_in(&held, &target),
            Err(Miss::NotFound { .. })
        ));
        // A pattern loose enough to hit two is still an ambiguity, not a pick.
        let loose = Target::by_name("re:^save ");
        assert!(matches!(
            select_in(&held, &loose),
            Err(Miss::Ambiguous { candidates }) if candidates.len() == 2
        ));
        let exact = Target::by_name("re:^save and");
        assert_eq!(
            select_in(&held, &exact),
            Ok(1),
            "matches Save and close only"
        );
    }

    #[test]
    fn an_uncompilable_pattern_is_reported_as_one_thing_wrong_with_the_input() {
        let held = facts(&[("Button", "OK")]);
        let miss = select_in(&held, &Target::by_name("re:([unclosed")).unwrap_err();
        assert!(matches!(miss, Miss::BadPattern { .. }), "{miss:?}");
        assert!(
            miss.to_reason().contains("re:([unclosed"),
            "say which input was bad: {}",
            miss.to_reason()
        );
        assert!(!miss.worth_one_retry(), "a bad pattern is not a stale tree");
    }

    #[test]
    fn a_target_refuses_the_fields_that_used_to_double_address_it() {
        // `index` inside a target is the old ambiguity in new clothing.
        let err = serde_json::from_value::<Target>(json!({"name": "ok", "index": 4}));
        assert!(err.is_err(), "unknown fields must not be ignored quietly");
        let message = err.unwrap_err().to_string();
        assert!(message.contains("index"), "{message}");
    }

    #[test]
    fn a_bare_name_is_still_a_complete_target() {
        let target = Target::by_name("probe-toggle");
        assert_eq!(target.role, None);
        assert_eq!(target.nth, None);
        assert_eq!(describe_target(&target), "\"probe-toggle\"");
        let full = Target {
            name: "ok".into(),
            role: Some("Button".into()),
            nth: Some(2),
            container: Some("dialog".into()),
        };
        let said = describe_target(&full);
        assert!(
            said.contains("as Button")
                && said.contains("inside \"dialog\"")
                && said.contains("nth 2"),
            "{said}"
        );
    }

    #[test]
    fn an_empty_role_or_container_filter_is_no_filter_rather_than_no_matches() {
        let held = facts(&[("Button", "OK")]);
        let mut target = Target::by_name("ok");
        target.role = Some(String::new());
        target.container = Some(String::new());
        assert_eq!(select_in(&held, &target), Ok(0));
    }

    #[test]
    fn facts_are_described_the_way_the_error_will_quote_them() {
        let inside = vec!["File menu".to_string()];
        let fact = Facts {
            name: "Open",
            role: "MenuItem",
            ancestors: &inside,
        };
        assert_eq!(fact.describe(), "MenuItem \"Open\", in \"File menu\"");
        let bare = Facts {
            name: "Open",
            role: "Button",
            ancestors: NONE,
        };
        assert_eq!(bare.describe(), "Button \"Open\"");
    }
}
