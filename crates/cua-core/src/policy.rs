//! The confirmation gate, shared by every engine's execution path.
//!
//! It lives here rather than in the CLI because an engine that loops on its own
//! (the browser: plan, act, observe) has to consult the same gate as the one the
//! CLI applies to a single `Action`. Duplicating the ladder is how two entry points
//! end up disagreeing about what `--yes` buys.

use serde::Serialize;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::pin::Pin;

use crate::Risk;

/// What an action visibly did, judged from state read back off the target.
///
/// A pattern call or a click answering successfully only means the request was
/// processed. The audits found a panel button that answered and opened nothing, and a
/// page click that reported `OK` while the DOM stayed still -- both were reported as
/// success, and callers that trusted it failed silently. The verdict is read back from
/// the thing itself, so "answered and something moved" is never again the same word as
/// "answered".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// A readable state of the target differs from before the action.
    Changed,
    /// Every readable state is identical to before the action.
    Unchanged,
    /// Too little was readable to claim either way.
    Unverifiable,
}

impl Effect {
    /// The word a caller sees, and the word the documentation describes.
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Changed => "changed",
            Effect::Unchanged => "unchanged",
            Effect::Unverifiable => "unverifiable",
        }
    }
}

/// An async approval hook: given what is about to happen and how risky it is, say
/// whether the caller may proceed. MCP wires this to a client elicitation.
pub type Approver = std::sync::Arc<
    dyn Fn(Risk, String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync,
>;

/// A refusal, phrased for whoever has to act on it: it names the switch that would
/// have allowed the action, because a gate whose stated escape does not work teaches
/// callers to set every switch at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked(pub String);

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Blocked {}

/// Decide and, where the caller can answer, ask.
///
/// The ladder is the one issue #8 closed with: `yes` never buys a High, and
/// `allow_high` is sufficient on its own for one.
pub async fn confirm(
    risk: Risk,
    description: &str,
    yes: bool,
    allow_high: bool,
    approver: Option<&Approver>,
) -> Result<(), Blocked> {
    if risk == Risk::High {
        if allow_high {
            return Ok(());
        }
        if let Some(approve) = approver {
            if approve(risk, description.to_string()).await {
                return Ok(());
            }
            return Err(Blocked(format!(
                "high-risk action declined by client: {description}"
            )));
        }
        return Err(Blocked(format!(
            "high-risk action blocked: {description}. Re-run with --allow-high / allow_high=true if intended"
        )));
    }
    if risk < Risk::Medium || yes {
        return Ok(());
    }
    // Medium without `yes`: ask through whatever channel the caller has. A client
    // that can answer an elicitation should get one, not an error that teaches it to
    // send `yes: true` next time -- which is how a gate gets trained out of existence.
    if let Some(approve) = approver {
        return if approve(risk, description.to_string()).await {
            Ok(())
        } else {
            Err(Blocked(format!(
                "{} risk action declined by client: {description}",
                risk.as_str()
            )))
        };
    }
    if !std::io::stdin().is_terminal() {
        return Err(Blocked(format!(
            "{} risk action requires --yes / yes=true, or a client that can answer confirmations: {description}",
            risk.as_str()
        )));
    }
    print!(
        "about to run a {} risk action: {}\nconfirm? [y/N] ",
        risk.as_str(),
        description
    );
    std::io::stdout()
        .flush()
        .map_err(|e| Blocked(format!("cannot prompt for confirmation: {e}")))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| Blocked(format!("cannot read the answer: {e}")))?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(Blocked(format!(
            "{} risk action declined by the user: {description}",
            risk.as_str()
        )))
    }
}

impl Risk {
    /// One tier up, capped at High.
    ///
    /// Used for an action recovered from the model's reasoning rather than its
    /// content: a chain of thought contains hypotheticals and negations, so the same
    /// action stated there is not safer -- and "one tier up, ceiling at the top" is
    /// the whole of what we can honestly say about it.
    pub fn escalate(self) -> Risk {
        match self {
            Risk::None => Risk::Safe,
            Risk::Safe => Risk::Low,
            Risk::Low => Risk::Medium,
            Risk::Medium => Risk::High,
            Risk::High => Risk::High,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn approver(answer: bool) -> Approver {
        Arc::new(move |_, _| Box::pin(async move { answer }))
    }

    #[test]
    fn escalating_moves_one_tier_and_stops_at_the_top() {
        assert_eq!(Risk::Low.escalate(), Risk::Medium);
        assert_eq!(Risk::Medium.escalate(), Risk::High);
        assert_eq!(Risk::High.escalate(), Risk::High, "capped, not wrapped");
        assert_eq!(Risk::Safe.escalate(), Risk::Low);
    }

    #[tokio::test]
    async fn a_low_risk_action_needs_no_decision() {
        assert!(confirm(Risk::Low, "click a button", false, false, None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn yes_does_not_buy_a_high_risk_action() {
        let blocked = confirm(Risk::High, "eval arbitrary script", true, false, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            blocked.contains("allow-high") || blocked.contains("allow_high"),
            "the message has to name the switch that actually allows it: {blocked}"
        );
    }

    #[tokio::test]
    async fn allow_high_alone_is_sufficient() {
        assert!(
            confirm(Risk::High, "eval arbitrary script", false, true, None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_declined_confirmation_is_a_refusal_not_a_pass() {
        let outcome = confirm(
            Risk::Medium,
            "press Enter",
            false,
            false,
            Some(&approver(false)),
        )
        .await;
        assert!(outcome.is_err(), "the client said no, so the action stops");
        assert!(outcome.unwrap_err().to_string().contains("declined"));
        assert!(confirm(
            Risk::Medium,
            "press Enter",
            false,
            false,
            Some(&approver(true))
        )
        .await
        .is_ok());
    }

    #[test]
    fn the_approver_is_told_both_the_tier_and_what_is_about_to_happen() {
        // A hook that only receives a description cannot word its prompt for the
        // tier it is guarding, which is how "approve this?" becomes an Enter key.
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(Risk, String)>::new()));
        let recorder = seen.clone();
        let hook: Approver = Arc::new(move |risk, description| {
            let recorder = recorder.clone();
            Box::pin(async move {
                recorder.lock().unwrap().push((risk, description));
                true
            })
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(confirm(
            Risk::Medium,
            "navigate across sites",
            false,
            false,
            Some(&hook),
        ))
        .unwrap();
        let asked = seen.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![(Risk::Medium, "navigate across sites".to_string())]
        );
    }
}
