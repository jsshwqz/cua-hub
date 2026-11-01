//! Model-driven browser loop: one JSON action per step.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cua_engine_vision::OpenAiConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::cdp::{self, LaunchOptions, PageSummary};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserAction {
    Navigate {
        url: String,
    },
    Click {
        selector: String,
    },
    Type {
        selector: String,
        text: String,
    },
    Press {
        key: String,
    },
    Eval {
        script: String,
    },
    Wait {
        ms: u64,
    },
    Scroll {
        #[serde(default)]
        dx: i32,
        #[serde(default = "default_scroll")]
        dy: i32,
    },
    WaitFor {
        selector: String,
        #[serde(default = "default_wait_ms")]
        timeout_ms: u64,
    },
    TabList,
    TabNew {
        url: String,
    },
    TabSwitch {
        index: usize,
    },
    TabClose {
        #[serde(default)]
        index: Option<usize>,
    },
    Done {
        summary: String,
    },
    Fail {
        reason: String,
    },
}

fn default_scroll() -> i32 {
    600
}

/// Where an action came from, because the same action is not equally trustworthy
/// from both places. A chain of thought contains hypotheticals and negations
/// ("do not run `{"action": ...}`"), and none of it was addressed to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionSource {
    Content,
    Reasoning,
}

/// The host of an http(s) URL, lowercased.
///
/// `None` for anything that is not one, including `about:blank`, `mailto:...` and a
/// bare path: those have no site to be the *same as*, and pretending the word before
/// a colon is a host would grade `about:blank` as a cross-site jump to a place
/// called "about".
fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .split('@')
        .next_back()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or(host).trim_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn navigate_risk(url: &str, page_url: Option<&str>) -> cua_core::Risk {
    use cua_core::Risk;
    // A `javascript:` URL is the eval path wearing a navigate hat, so it takes the
    // eval tier. The contract did not name it; it named the thing it does.
    if url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("javascript:")
    {
        return Risk::High;
    }
    let Some(target) = host_of(url) else {
        // Cannot tell where this goes, which is not the same as it being safe.
        return Risk::Medium;
    };
    match page_url.and_then(host_of) {
        Some(current) if current == target => Risk::Low,
        // No page to compare against (a fresh tab) is not evidence of same-site.
        Some(_) => Risk::Medium,
        None => Risk::Medium,
    }
}

fn is_submitting_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "enter" | "return" | "\\r" | "\r"
    )
}

/// Does this selector name something that submits?
///
/// Reads the selector's shape -- see [`BrowserAction::risk`] for what that can and
/// cannot catch.
fn addresses_submit_control(selector: &str) -> bool {
    let flat = selector.to_ascii_lowercase();
    [
        "submit",
        "type=submit",
        "type='submit'",
        "type=\"submit\"",
        "login",
        "log-in",
        "signin",
        "sign-in",
        "password",
        "send",
        "pay",
        "purchase",
        "checkout",
        "order",
        "register",
        "confirm",
        "proceed",
    ]
    .iter()
    .any(|needle| flat.contains(needle))
}

fn default_wait_ms() -> u64 {
    5000
}

impl BrowserAction {
    /// Whether this action is about to touch the page, or only ends the run.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BrowserAction::Done { .. } | BrowserAction::Fail { .. }
        )
    }

    /// The tier the confirmation gate demands before firing this action.
    ///
    /// `page_url` is the page the action would act on, used only to tell a
    /// same-site navigation from a cross-site one; `None` means there is no page to
    /// compare against, which is not evidence of sameness.
    ///
    /// `Click`'s submit judgement reads the **selector's shape**, not the element it
    /// resolves to -- nothing has been queried yet at this point. A selector that
    /// names a submit-ish control is treated as submitting; a precise `#save-button`
    /// that happens to post a form is not caught here. That is the same limit the
    /// command classifier has (ROADMAP: 读形状不读意图), and it errs toward Medium.
    pub fn risk(&self, page_url: Option<&str>) -> cua_core::Risk {
        use cua_core::Risk;
        match self {
            // Arbitrary JavaScript in the page: it can read cookies, forge requests,
            // and exfiltrate whatever the origin can reach. Never anything less.
            BrowserAction::Eval { .. } => Risk::High,
            BrowserAction::Navigate { url } => navigate_risk(url, page_url),
            BrowserAction::TabNew { url } => navigate_risk(url, page_url),
            BrowserAction::Click { selector } => {
                if addresses_submit_control(selector) {
                    Risk::Medium
                } else {
                    Risk::Low
                }
            }
            // The contract puts Enter with the submitting actions, not with the keys:
            // in a form field it is the submit, whatever the selector looked like.
            BrowserAction::Press { key } => {
                if is_submitting_key(key) {
                    Risk::Medium
                } else {
                    Risk::Low
                }
            }
            BrowserAction::Type { .. }
            | BrowserAction::Wait { .. }
            | BrowserAction::WaitFor { .. }
            | BrowserAction::Scroll { .. }
            | BrowserAction::TabList
            | BrowserAction::TabSwitch { .. }
            | BrowserAction::TabClose { .. } => Risk::Low,
            // Nothing is fired at the browser for these; the run just stops.
            BrowserAction::Done { .. } | BrowserAction::Fail { .. } => Risk::None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            BrowserAction::Navigate { url } => format!("navigate {url}"),
            BrowserAction::Click { selector } => format!("click {selector}"),
            BrowserAction::Type { selector, text } => {
                let preview: String = text.chars().take(40).collect();
                format!("type into {selector}: {preview:?}")
            }
            BrowserAction::Press { key } => format!("press {key}"),
            BrowserAction::Eval { script } => {
                let preview: String = script.chars().take(80).collect();
                format!("eval {preview}")
            }
            BrowserAction::Wait { ms } => format!("wait {ms}ms"),
            BrowserAction::Scroll { dx, dy } => format!("scroll dx={dx} dy={dy}"),
            BrowserAction::WaitFor {
                selector,
                timeout_ms,
            } => format!("wait_for {selector} (≤{timeout_ms}ms)"),
            BrowserAction::TabList => "tab_list".into(),
            BrowserAction::TabNew { url } => format!("tab_new {url}"),
            BrowserAction::TabSwitch { index } => format!("tab_switch {index}"),
            BrowserAction::TabClose { index } => match index {
                Some(i) => format!("tab_close {i}"),
                None => "tab_close current".into(),
            },
            BrowserAction::Done { summary } => format!("done: {summary}"),
            BrowserAction::Fail { reason } => format!("fail: {reason}"),
        }
    }
}

/// Output-token cap used for this loop's chat requests when nobody configures one.
/// One JSON action plus a short reason fits well inside it; raising it buys longer
/// answers, not better ones.
pub const DEFAULT_MAX_TOKENS: u32 = 700;

/// Environment override, applied when a run does not pass `max_tokens` itself.
pub const MAX_TOKENS_ENV: &str = "CUA_HUB_BROWSER_MAX_TOKENS";

/// Pick the output-token cap: an explicit per-run value wins, then the environment,
/// then the built-in default. Takes the environment value as an argument instead of
/// reading it here, so the precedence is testable without mutating process state.
fn resolve_max_tokens(requested: Option<u32>, from_env: Option<u32>) -> u32 {
    requested.or(from_env).unwrap_or(DEFAULT_MAX_TOKENS)
}

fn max_tokens_from_env() -> Option<u32> {
    std::env::var(MAX_TOKENS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
}

/// Environment override for the cursor hint.
pub const INDICATE_MOUSE_ENV: &str = "CUA_HUB_BROWSER_INDICATE_MOUSE";

/// Whether to follow the target with the real cursor: an explicit per-run value
/// wins, then the environment, then **on**. It is on by default because a visual
/// aid nobody switches on is a visual aid nobody gets; `=0` is the escape hatch
/// for someone who wants their mouse back.
///
/// Takes the environment value as an argument rather than reading it here, so the
/// precedence is testable without mutating process state.
fn resolve_indicate_mouse(requested: Option<bool>, from_env: Option<bool>) -> bool {
    requested.or(from_env).unwrap_or(true)
}

/// Whether this run should follow its targets with the real cursor.
///
/// A dry run never does: there is no action coming, so a cursor parked on a
/// submit button would be a promise the run has no intention of keeping.
fn pointer_hint_wanted(dry_run: bool, requested: Option<bool>, from_env: Option<bool>) -> bool {
    !dry_run && resolve_indicate_mouse(requested, from_env)
}

fn indicate_mouse_from_env() -> Option<bool> {
    parse_indicate_mouse(std::env::var(INDICATE_MOUSE_ENV).ok().as_deref())
}

/// Read the environment switch. Anything that is not a recognisable on/off word
/// is ignored rather than obeyed — a typo should not silently disable a safety
/// feature nobody asked to remove, nor silently enable one they turned off.
fn parse_indicate_mouse(raw: Option<&str>) -> Option<bool> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "0" | "false" | "no" | "off" => Some(false),
        "1" | "true" | "yes" | "on" => Some(true),
        other => {
            warn!("ignoring {INDICATE_MOUSE_ENV}={other:?}: not a recognised on/off value");
            None
        }
    }
}

/// Options for one browser task. `Clone` but deliberately not `Debug`: the approval
/// hook is a `dyn Fn`, same as `RunOptions` in the CLI.
#[derive(Clone)]
pub struct BrowserRunOptions {
    pub task: String,
    pub max_steps: u32,
    pub dry_run: bool,
    pub model: Option<String>,
    pub headed: bool,
    pub start_url: Option<String>,
    pub run_dir: Option<PathBuf>,
    pub screenshot_quality: u8,
    /// Output-token cap for this loop's chat-completions requests. `None` falls
    /// back to `CUA_HUB_BROWSER_MAX_TOKENS`, then to [`DEFAULT_MAX_TOKENS`] (700).
    /// This only bounds how much the model may answer; it does not touch the model,
    /// the sampling temperature, or the message structure.
    pub max_tokens: Option<u32>,
    /// Whether to park the real cursor on each click/type target first, so a human
    /// watching can follow the tool. `None` falls back to
    /// `CUA_HUB_BROWSER_INDICATE_MOUSE`, then to **true**. Only ever a visual aid:
    /// it presses nothing, and it is skipped entirely for a dry run or a headless
    /// browser, which has no cursor on screen to follow.
    pub indicate_mouse: Option<bool>,
    /// Moves the OS cursor for the hint. Injected by whoever owns the desktop
    /// input backend — this crate deliberately does not link one, so it still
    /// runs where there is no pointer. `None` means no hint even when enabled.
    pub pointer: Option<cdp::PointerMover>,
    /// The same three switches the single-action path uses. They live here because
    /// this loop plans and fires its own actions, and a gate that only guards the
    /// other path is a gate with a door in it.
    pub yes: bool,
    pub allow_high: bool,
    pub approver: Option<cua_core::policy::Approver>,
    pub timeout: Option<Duration>,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for BrowserRunOptions {
    fn default() -> Self {
        Self {
            task: String::new(),
            max_steps: 15,
            dry_run: false,
            model: None,
            headed: false,
            start_url: None,
            run_dir: None,
            screenshot_quality: 70,
            max_tokens: None,
            indicate_mouse: None,
            pointer: None,
            // Neither default may be `true`: letting a model drive a real browser has
            // to be said out loud, or answered by a client that can confirm.
            yes: false,
            allow_high: false,
            approver: None,
            timeout: None,
            cancel: None,
        }
    }
}

/// Check if cancellation was requested via a sentinel file in `run_dir`
/// (`cancel` or `.cancel`) or an in-memory `AtomicBool` signal.
pub fn is_cancelled(run_dir: &Path, cancel: Option<&Arc<AtomicBool>>) -> bool {
    if run_dir.join("cancel").exists() || run_dir.join(".cancel").exists() {
        return true;
    }
    if let Some(c) = cancel {
        if c.load(Ordering::Relaxed) {
            return true;
        }
    }
    false
}

#[derive(Debug, PartialEq)]
pub enum BrowserOutcome {
    Completed(String),
    Failed(String),
    StepLimit,
    /// Planned action, described as text (dry-run).
    DryRun(String),
}

impl BrowserOutcome {
    pub fn summary(&self, steps: usize) -> String {
        match self {
            BrowserOutcome::Completed(s) => format!("completed: {s}"),
            BrowserOutcome::Failed(s) => format!("failed: {s}"),
            BrowserOutcome::StepLimit => format!("stopped after {steps} steps"),
            BrowserOutcome::DryRun(s) => format!("dry-run planned: {s}"),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(
            self,
            BrowserOutcome::Completed(_) | BrowserOutcome::DryRun(_)
        )
    }
}

#[derive(Debug)]
pub struct BrowserReport {
    pub outcome: BrowserOutcome,
    pub run_dir: PathBuf,
    pub steps: usize,
}

pub async fn run_task(opts: BrowserRunOptions) -> Result<BrowserReport> {
    let total_timeout = opts.timeout.unwrap_or(Duration::from_secs(600));
    let task_start = std::time::Instant::now();

    let mut config = OpenAiConfig::from_env()?.with_model(opts.model.clone());
    config.max_tokens = resolve_max_tokens(opts.max_tokens, max_tokens_from_env());
    eprintln!(
        "provider: browser model={} max_tokens={}",
        config.model, config.max_tokens
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_default();

    let mut session = cdp::launch(LaunchOptions {
        headed: opts.headed,
        start_url: opts.start_url.clone(),
        ..Default::default()
    })
    .await
    .context("launch browser")?;

    // The cursor hint is a visual aid for whoever is watching the screen, so it
    // never gates anything: a dry run has no action to point at, and a headless
    // browser has no cursor on screen to follow.
    let indicate =
        pointer_hint_wanted(opts.dry_run, opts.indicate_mouse, indicate_mouse_from_env());
    match (indicate, opts.pointer.clone()) {
        (true, Some(mover)) => session.set_pointer_hint(Some(mover)),
        (true, None) => tracing::debug!(
            "pointer hint is on but no desktop input backend was supplied; nothing will be indicated"
        ),
        (false, _) => {}
    }

    if let Some(url) = &opts.start_url {
        session.navigate(url).await?;
    }

    let run_dir = opts.run_dir.clone().unwrap_or_else(default_run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let mut log = std::fs::File::create(run_dir.join("steps.jsonl"))?;
    let mut history: Vec<String> = Vec::new();
    let mut outcome = BrowserOutcome::StepLimit;
    let mut executed_steps = 0usize;

    for index in 0..opts.max_steps {
        if is_cancelled(&run_dir, opts.cancel.as_ref()) {
            outcome = BrowserOutcome::Failed(
                "cancelled: task cancelled via signal or sentinel file".into(),
            );
            break;
        }
        if task_start.elapsed() >= total_timeout {
            let note = format!(
                "timeout: task exceeded total timeout of {}s",
                total_timeout.as_secs()
            );
            outcome = BrowserOutcome::Failed(note);
            break;
        }

        let summary = session.page_summary().await.unwrap_or(PageSummary {
            url: String::new(),
            title: String::new(),
            text: String::new(),
            ax: String::new(),
        });
        let shot = match session.screenshot_jpeg(opts.screenshot_quality).await {
            Ok(s) => s,
            Err(e) => {
                let note = format!("screenshot failed: {e}");
                eprintln!("[{index:02}] {note}");
                let fail_action = BrowserAction::Fail {
                    reason: note.clone(),
                };
                record(&mut log, index, &fail_action, &note)?;
                executed_steps += 1;
                outcome = BrowserOutcome::Failed(note);
                break;
            }
        };
        if let Err(e) = std::fs::write(run_dir.join(format!("browser-step-{index:02}.jpg")), &shot)
        {
            let note = format!("screenshot failed: failed to save step image: {e}");
            eprintln!("[{index:02}] {note}");
            let fail_action = BrowserAction::Fail {
                reason: note.clone(),
            };
            record(&mut log, index, &fail_action, &note)?;
            executed_steps += 1;
            outcome = BrowserOutcome::Failed(note);
            break;
        }

        // A model failure (retries exhausted) is a Failed outcome, not a
        // crashed run: steps.jsonl gets its Fail record and outcome.json is
        // written through the normal exit — same shape as the screenshot
        // failure path above. Losing the audit trail to a model hiccup was
        // the whole defect.
        let (action, source) =
            match ask(&http, &config, &opts.task, &summary, &history, &shot).await {
                Ok(pair) => pair,
                Err(e) => {
                    let note = format!("model failed at browser step {index}: {e:#}");
                    eprintln!("[{index:02}] {note}");
                    let fail_action = BrowserAction::Fail {
                        reason: note.clone(),
                    };
                    record(&mut log, index, &fail_action, &note)?;
                    executed_steps += 1;
                    outcome = BrowserOutcome::Failed(note);
                    break;
                }
            };
        let base_risk = action.risk(Some(&summary.url));
        let risk = if source == ActionSource::Reasoning {
            base_risk.escalate()
        } else {
            base_risk
        };
        eprintln!(
            "[{index:02}] {} [risk={}]",
            action.describe(),
            risk.as_str()
        );
        if source == ActionSource::Reasoning {
            // It used to parse the chain of thought in silence, so a step that came
            // from a hypothetical rather than from what the model chose to say looked
            // exactly like a normal one in every log.
            warn!(
                action = action.describe(),
                risk = risk.as_str(),
                stated_risk = base_risk.as_str(),
                "browser action recovered from model reasoning, not content: raising one tier"
            );
        }

        match &action {
            BrowserAction::Done { summary } => {
                record(&mut log, index, &action, "completed")?;
                outcome = BrowserOutcome::Completed(summary.clone());
                break;
            }
            BrowserAction::Fail { reason } => {
                record(&mut log, index, &action, "failed")?;
                outcome = BrowserOutcome::Failed(reason.clone());
                break;
            }
            _ => {}
        }

        if opts.dry_run {
            record(&mut log, index, &action, "dry-run")?;
            outcome =
                BrowserOutcome::DryRun(format!("{} [risk={}]", action.describe(), risk.as_str()));
            break;
        }

        if let Err(refused) = cua_core::policy::confirm(
            risk,
            &action.describe(),
            opts.yes,
            opts.allow_high,
            opts.approver.as_ref(),
        )
        .await
        {
            // Stop the run rather than skipping one step: the next plan would be built
            // on a page we deliberately did not touch.
            let note = format!("blocked before execution: {refused}");
            record(&mut log, index, &action, &note)?;
            history.push(format!("#{index} {} => {note}", action.describe()));
            outcome = BrowserOutcome::Failed(note);
            break;
        }

        if is_cancelled(&run_dir, opts.cancel.as_ref()) {
            outcome = BrowserOutcome::Failed(
                "cancelled: task cancelled via signal or sentinel file".into(),
            );
            break;
        }
        if task_start.elapsed() >= total_timeout {
            let note = format!(
                "timeout: task exceeded total timeout of {}s",
                total_timeout.as_secs()
            );
            outcome = BrowserOutcome::Failed(note);
            break;
        }

        let result = execute(&mut session, &action, &run_dir, opts.cancel.as_ref()).await;
        let note = match result {
            Ok(note) => note,
            Err(e) => format!("error: {e}"),
        };
        eprintln!("      -> {note}");
        record(&mut log, index, &action, &note)?;
        history.push(format!("#{index} {} => {}", action.describe(), note));
        executed_steps += 1;

        if is_cancelled(&run_dir, opts.cancel.as_ref()) {
            outcome = BrowserOutcome::Failed(
                "cancelled: task cancelled via signal or sentinel file".into(),
            );
            break;
        }
        if task_start.elapsed() >= total_timeout {
            let note = format!(
                "timeout: task exceeded total timeout of {}s",
                total_timeout.as_secs()
            );
            outcome = BrowserOutcome::Failed(note);
            break;
        }

        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    let report = BrowserReport {
        outcome,
        run_dir: run_dir.clone(),
        steps: executed_steps,
    };
    std::fs::write(
        run_dir.join("outcome.json"),
        serde_json::to_vec_pretty(&json!({
            "task": opts.task,
            "outcome": report.outcome.summary(report.steps),
            "steps": report.steps,
        }))?,
    )?;
    eprintln!("\n{}", report.outcome.summary(report.steps));
    eprintln!("artifacts: {}", run_dir.display());
    Ok(report)
}

pub async fn execute_wait(ms: u64, run_dir: &Path, cancel: Option<&Arc<AtomicBool>>) -> String {
    const MAX_SINGLE_WAIT_MS: u64 = 60_000;
    let clamped = ms > MAX_SINGLE_WAIT_MS;
    let effective_ms = ms.min(MAX_SINGLE_WAIT_MS);
    let mut remaining = effective_ms;
    while remaining > 0 {
        if is_cancelled(run_dir, cancel) {
            break;
        }
        let chunk = remaining.min(100);
        tokio::time::sleep(Duration::from_millis(chunk)).await;
        remaining = remaining.saturating_sub(chunk);
    }
    if clamped {
        format!("waited {effective_ms}ms (clamped from {ms}ms)")
    } else {
        format!("waited {effective_ms}ms")
    }
}

async fn execute(
    session: &mut cdp::BrowserSession,
    action: &BrowserAction,
    run_dir: &Path,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<String> {
    match action {
        BrowserAction::Navigate { url } => {
            session.navigate(url).await?;
            let s = session.page_summary().await?;
            Ok(format!("loaded {} ({})", s.url, s.title))
        }
        BrowserAction::Click { selector } => {
            let r = session.click(selector).await?;
            if r == "NOT_FOUND" {
                bail!("selector not found: {selector}");
            }
            Ok(r)
        }
        BrowserAction::Type { selector, text } => {
            let r = session.type_text(selector, text).await?;
            if r == "NOT_FOUND" {
                bail!("selector not found: {selector}");
            }
            Ok(r)
        }
        BrowserAction::Press { key } => {
            session.press_key(key).await?;
            Ok(format!("pressed {key}"))
        }
        BrowserAction::Eval { script } => {
            let value = session.eval(script).await?;
            let text = value.to_string();
            Ok(text.chars().take(2000).collect())
        }
        BrowserAction::Wait { ms } => {
            let note = execute_wait(*ms, run_dir, cancel).await;
            Ok(note)
        }
        BrowserAction::Scroll { dx, dy } => {
            session.scroll(*dx, *dy).await?;
            Ok(format!("scrolled dx={dx} dy={dy}"))
        }
        BrowserAction::WaitFor {
            selector,
            timeout_ms,
        } => {
            let found = session.wait_for_selector(selector, *timeout_ms).await?;
            Ok(if found {
                format!("found {selector}")
            } else {
                format!("timeout waiting for {selector}")
            })
        }
        BrowserAction::TabList => {
            let tabs = session.list_tabs().await?;
            let mut out = String::new();
            for (i, tab) in tabs.iter().enumerate() {
                out.push_str(&format!("{i}: {} — {}\n", tab.title, tab.url));
            }
            Ok(out.trim_end().to_string())
        }
        BrowserAction::TabNew { url } => {
            session.new_tab(url).await?;
            let s = session.page_summary().await?;
            Ok(format!("opened tab {} ({})", s.url, s.title))
        }
        BrowserAction::TabSwitch { index } => {
            session.switch_tab(*index).await?;
            let s = session.page_summary().await?;
            Ok(format!("tab {index}: {} ({})", s.url, s.title))
        }
        BrowserAction::TabClose { index } => {
            session.close_tab(*index).await?;
            Ok(format!(
                "closed tab; {} remaining",
                session.list_tabs().await?.len()
            ))
        }
        BrowserAction::Done { .. } | BrowserAction::Fail { .. } => Ok("terminal".into()),
    }
}

fn record(file: &mut std::fs::File, index: u32, action: &BrowserAction, note: &str) -> Result<()> {
    use std::io::Write;
    let line = json!({ "index": index, "action": action, "note": note });
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

/// The chat-completions request for one step of the loop. Pulled out of `ask` so the
/// shape of the body — in particular the configured `max_tokens` — is checkable
/// without a network round trip. Every other field is deliberately left as it was.
fn chat_body(config: &OpenAiConfig, user_text: &str, image_b64: &str) -> Value {
    json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "temperature": 0,
        "messages": [
            { "role": "system", "content": system_prompt() },
            { "role": "user", "content": [
                { "type": "text", "text": user_text },
                { "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{image_b64}") } }
            ]}
        ]
    })
}

/// Per-step model attempts (transport and content errors share this budget).
const MODEL_RETRY_ATTEMPTS: u32 = 3;

/// Appended to the prompt when the previous reply was a content error — the
/// model, not the network, was the problem, so re-sending the identical body
/// would plausibly produce the identical reply.
const REPAIR_NOTE: &str = "NOTE: Your previous reply was not a valid JSON action. Reply with exactly one JSON object and no prose.";

/// How one ask attempt failed.
enum AskFailure {
    /// The reply arrived but was unusable (invalid JSON envelope, empty
    /// content, unparseable action). A re-prompt with the repair note can
    /// plausibly fix it.
    Content { class: &'static str, err: String },
    /// The network or server failed (send error, 5xx, 429). Worth retrying
    /// with the original body.
    Transport { err: String },
    /// Not worth retrying (e.g. HTTP 4xx): the request itself is wrong.
    Fatal(String),
}

/// One request/reply round against the model endpoint. Classifies the
/// failure, if any, so the retry loop can decide between repair-note
/// retries, plain retries, and giving up.
async fn ask_once(
    http: &reqwest::Client,
    url: &str,
    config: &OpenAiConfig,
    body: &Value,
) -> Result<(BrowserAction, ActionSource), AskFailure> {
    let result = http
        .post(url)
        .bearer_auth(&config.api_key)
        .json(body)
        .send()
        .await;
    let resp = match result {
        Ok(r) => r,
        Err(e) => return Err(AskFailure::Transport { err: e.to_string() }),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let err = format!(
            "HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        );
        return if status.is_server_error() || status.as_u16() == 429 {
            Err(AskFailure::Transport { err })
        } else {
            Err(AskFailure::Fatal(err))
        };
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| AskFailure::Content {
        class: "invalid JSON",
        err: format!(
            "invalid JSON: {e}: {}",
            text.chars().take(300).collect::<String>()
        ),
    })?;
    let message = &value["choices"][0]["message"];
    let content = message["content"].as_str().unwrap_or_default();
    if content.trim().is_empty() {
        if let Some(reasoning) = message["reasoning"].as_str() {
            if reasoning.contains("\"action\"") {
                // Kept, not refused -- but it is a chain of thought, not
                // something the model decided to say. The caller raises
                // the tier and logs where the step really came from. A
                // fallback that fails to parse is a content error like any
                // other and joins the retry loop.
                return parse_browser_action(reasoning)
                    .map(|action| (action, ActionSource::Reasoning))
                    .map_err(|e| AskFailure::Content {
                        class: "reasoning parse",
                        err: format!("reasoning fallback parse failed: {e}"),
                    });
            }
        }
        return Err(AskFailure::Content {
            class: "empty content",
            err: format!(
                "empty model content: {}",
                text.chars().take(300).collect::<String>()
            ),
        });
    }
    parse_browser_action(content)
        .map(|action| (action, ActionSource::Content))
        .map_err(|e| AskFailure::Content {
            class: "action parse",
            err: e.to_string(),
        })
}

async fn ask(
    http: &reqwest::Client,
    config: &OpenAiConfig,
    task: &str,
    page: &PageSummary,
    history: &[String],
    screenshot: &[u8],
) -> Result<(BrowserAction, ActionSource)> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(screenshot);

    let trail = if history.is_empty() {
        "(none)".to_string()
    } else {
        history
            .iter()
            .rev()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user_text = format!(
        "TASK: {task}\n\nURL: {}\nTITLE: {}\n\nACCESSIBILITY TREE (trimmed):\n{}\n\nPAGE TEXT (trimmed):\n{}\n\nRECENT STEPS:\n{}\n\nChoose one next action. JSON only.",
        page.url,
        page.title,
        page.ax.chars().take(2500).collect::<String>(),
        page.text.chars().take(2000).collect::<String>(),
        trail
    );

    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
    let mut last_err = String::new();
    let mut content_error = false;
    for attempt in 0..MODEL_RETRY_ATTEMPTS {
        // Transport retries keep the original body; content retries append
        // the repair note — the previous reply, not the network, was wrong.
        let body = if attempt > 0 && content_error {
            chat_body(config, &format!("{user_text}\n\n{REPAIR_NOTE}"), &b64)
        } else {
            chat_body(config, &user_text, &b64)
        };
        match ask_once(http, &url, config, &body).await {
            Ok(pair) => return Ok(pair),
            Err(AskFailure::Fatal(err)) => bail!("{err}"),
            Err(AskFailure::Content { class, err }) => {
                content_error = true;
                tracing::warn!(
                    attempt = attempt + 1,
                    class,
                    reply = %err,
                    "model reply unusable; retrying with repair note"
                );
                last_err = format!("[{class}] {err}");
            }
            Err(AskFailure::Transport { err }) => {
                content_error = false;
                last_err = err;
            }
        }
        if attempt + 1 == MODEL_RETRY_ATTEMPTS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(700 * 2u64.pow(attempt))).await;
    }
    bail!("{last_err}")
}

fn system_prompt() -> &'static str {
    "You are a browser automation agent driving a Chromium page over CDP. \
Reply with ONE JSON object, no markdown, no prose.\n\
Actions:\n\
{\"action\":\"navigate\",\"url\":\"https://...\"}\n\
{\"action\":\"click\",\"selector\":\"css selector or text=Visible label\"}\n\
{\"action\":\"type\",\"selector\":\"css selector\",\"text\":\"...\"}\n\
{\"action\":\"press\",\"key\":\"Enter|Tab|Escape\"}\n\
{\"action\":\"eval\",\"script\":\"JS expression returning JSON-able data\"}\n\
{\"action\":\"wait\",\"ms\":1000}\n\
{\"action\":\"scroll\",\"dy\":600}                 [RESTRICTED: exploration-only on unbounded canvas/feeds; never use as prefix to click/type]\n\
{\"action\":\"wait_for\",\"selector\":\"...\",\"timeout_ms\":5000}\n\
{\"action\":\"tab_list\"}\n\
{\"action\":\"tab_new\",\"url\":\"https://...\"}\n\
{\"action\":\"tab_switch\",\"index\":0}\n\
{\"action\":\"tab_close\",\"index\":0}            omit index to close current\n\
{\"action\":\"done\",\"summary\":\"one line result\"}\n\
{\"action\":\"fail\",\"reason\":\"...\"}\n\
Rules: prefer `text=` selectors when you cannot see exact CSS. Selectors also match inside same-origin iframes. \
Never issue 'scroll' to locate questions or inputs; click and type automatically scroll targets into view and resolve occlusions. \
Use the accessibility tree for roles/names; use eval for reading data. \
Do not submit forms, purchase, or log in unless the task explicitly asks. \
Never repeat a failing action more than twice. When the goal is reached, reply done."
}

/// Parse a browser action from a model reply, tolerating fences and prose.
pub fn parse_browser_action(content: &str) -> Result<BrowserAction> {
    let trimmed = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```");
    let value =
        extract_object(trimmed).ok_or_else(|| anyhow::anyhow!("no JSON action in model reply"))?;
    let action = value["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing action field"))?
        .to_ascii_lowercase();

    let s = |key: &str| -> Result<String> {
        value[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("missing string field `{key}`"))
    };

    Ok(match action.as_str() {
        "navigate" | "open" | "goto" => BrowserAction::Navigate { url: s("url")? },
        "click" => BrowserAction::Click {
            selector: s("selector")?,
        },
        "type" | "fill" => BrowserAction::Type {
            selector: s("selector")?,
            text: s("text")?,
        },
        "press" => BrowserAction::Press { key: s("key")? },
        "eval" | "extract" => BrowserAction::Eval {
            script: s("script")?,
        },
        "wait" => BrowserAction::Wait {
            ms: value["ms"].as_u64().unwrap_or(800),
        },
        "scroll" => BrowserAction::Scroll {
            dx: value["dx"].as_i64().unwrap_or(0) as i32,
            dy: value["dy"].as_i64().unwrap_or(default_scroll() as i64) as i32,
        },
        "wait_for" => BrowserAction::WaitFor {
            selector: s("selector")?,
            timeout_ms: value["timeout_ms"].as_u64().unwrap_or(default_wait_ms()),
        },
        "tab_list" => BrowserAction::TabList,
        "tab_new" => BrowserAction::TabNew { url: s("url")? },
        "tab_switch" => BrowserAction::TabSwitch {
            index: value["index"].as_u64().unwrap_or(0) as usize,
        },
        "tab_close" => BrowserAction::TabClose {
            index: value["index"].as_u64().map(|i| i as usize),
        },
        "done" => BrowserAction::Done {
            summary: value["summary"].as_str().unwrap_or("task complete").into(),
        },
        "fail" => BrowserAction::Fail {
            reason: value["reason"]
                .as_str()
                .unwrap_or("model reported failure")
                .into(),
        },
        other => bail!("unsupported browser action: {other}"),
    })
}

fn extract_object(s: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(s.trim()) {
        if v.get("action").is_some() {
            return Some(v);
        }
    }
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'{' {
            continue;
        }
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for (j, &b) in bytes.iter().enumerate().skip(i) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_string = false;
                }
                continue;
            }
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = serde_json::from_str::<Value>(&s[i..=j]) {
                            if v.get("action").is_some() {
                                return Some(v);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn default_run_dir() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(".runs").join(format!("browser-{stamp}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_core::Risk;

    fn probe_config(cap: u32) -> OpenAiConfig {
        OpenAiConfig {
            api_key: "unit-test".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "unit-test-model".into(),
            max_tokens: cap,
        }
    }

    #[test]
    fn the_untouched_default_is_still_700_output_tokens() {
        assert_eq!(DEFAULT_MAX_TOKENS, 700);
        assert_eq!(BrowserRunOptions::default().max_tokens, None);
        assert_eq!(resolve_max_tokens(None, None), 700);
        let body = chat_body(
            &probe_config(resolve_max_tokens(None, None)),
            "TASK",
            "AA==",
        );
        assert_eq!(body["max_tokens"], 700);
    }

    #[test]
    fn a_configured_cap_reaches_the_request_body() {
        let body = chat_body(
            &probe_config(resolve_max_tokens(Some(4096), None)),
            "TASK",
            "AA==",
        );
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn per_run_value_beats_environment_and_both_beat_the_default() {
        assert_eq!(resolve_max_tokens(Some(900), Some(500)), 900);
        assert_eq!(resolve_max_tokens(None, Some(500)), 500);
        assert_eq!(resolve_max_tokens(None, None), DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn the_cursor_hint_is_on_unless_someone_says_otherwise() {
        assert!(resolve_indicate_mouse(None, None), "default is on");
        assert!(!resolve_indicate_mouse(Some(false), None));
        assert!(
            !resolve_indicate_mouse(None, Some(false)),
            "env can turn it off"
        );
        assert!(
            resolve_indicate_mouse(Some(true), Some(false)),
            "the per-run answer beats the environment"
        );
    }

    #[test]
    fn a_dry_run_never_points_at_anything() {
        assert!(!pointer_hint_wanted(true, Some(true), None));
        assert!(!pointer_hint_wanted(true, None, Some(true)));
        assert!(pointer_hint_wanted(false, None, None));
    }

    #[test]
    fn the_environment_switch_accepts_words_and_ignores_garbage() {
        assert_eq!(parse_indicate_mouse(Some("0")), Some(false));
        assert_eq!(parse_indicate_mouse(Some(" off ")), Some(false));
        assert_eq!(parse_indicate_mouse(Some("FALSE")), Some(false));
        assert_eq!(parse_indicate_mouse(Some("1")), Some(true));
        assert_eq!(parse_indicate_mouse(Some("on")), Some(true));
        assert_eq!(parse_indicate_mouse(None), None);
        assert_eq!(parse_indicate_mouse(Some("")), None);
        assert_eq!(parse_indicate_mouse(Some("maybe")), None);
    }

    #[test]
    fn raising_the_cap_moves_no_other_request_field() {
        // The cap is the only thing this change is allowed to touch in the body, so
        // say it in a test: same request with two different caps must differ in one
        // key and nothing else.
        let low = chat_body(&probe_config(700), "TASK", "AA==");
        let high = chat_body(&probe_config(4000), "TASK", "AA==");
        let strip = |mut v: Value| {
            v.as_object_mut().map(|o| o.remove("max_tokens"));
            v
        };
        assert_eq!(low["max_tokens"], 700);
        assert_eq!(high["max_tokens"], 4000);
        assert_eq!(strip(low), strip(high), "only max_tokens may differ");
    }

    #[test]
    fn arbitrary_script_is_never_anything_but_high() {
        assert_eq!(
            BrowserAction::Eval {
                script: "document.cookie".into()
            }
            .risk(Some("https://example.com")),
            Risk::High,
            "eval reads cookies and can send them anywhere the origin can reach"
        );
    }

    #[test]
    fn navigation_is_judged_against_the_page_it_leaves() {
        let here = Some("https://example.com/products");
        assert_eq!(
            BrowserAction::Navigate {
                url: "https://example.com/cart".into()
            }
            .risk(here),
            Risk::Low,
            "same host: the contract calls this Low"
        );
        assert_eq!(
            BrowserAction::Navigate {
                url: "https://other.test/".into()
            }
            .risk(here),
            Risk::Medium,
            "cross-site is where a task can be carried off to somewhere else"
        );
        assert_eq!(
            BrowserAction::Navigate {
                url: "https://example.com/".into()
            }
            .risk(None),
            Risk::Medium,
            "no page to compare against is not evidence of same-site"
        );
        assert_eq!(
            BrowserAction::Navigate {
                url: "not a url".into()
            }
            .risk(here),
            Risk::Medium,
            "undeterminable counts as Medium, per the contract"
        );
    }

    #[test]
    fn a_javascript_url_is_the_eval_path_wearing_a_navigate_hat() {
        // Not named by the contract's table; it is stated there as Eval -> High, and
        // `javascript:` executes exactly that. Flagged in the PR as an addition.
        assert_eq!(
            BrowserAction::Navigate {
                url: "javascript:fetch('//attacker')".into()
            }
            .risk(Some("https://example.com")),
            Risk::High
        );
    }

    #[test]
    fn a_submit_looking_target_is_medium_and_a_plain_one_low() {
        // The judgement reads the selector's shape: nothing has been resolved yet.
        assert_eq!(
            BrowserAction::Click {
                selector: "button[type=submit]".into()
            }
            .risk(None),
            Risk::Medium
        );
        assert_eq!(
            BrowserAction::Click {
                selector: "#login-button".into()
            }
            .risk(None),
            Risk::Medium
        );
        assert_eq!(
            BrowserAction::Click {
                selector: "#cookie-banner-accept".into()
            }
            .risk(None),
            Risk::Low
        );
    }

    #[test]
    fn enter_is_a_submit_whatever_it_was_typed_into() {
        assert_eq!(
            BrowserAction::Press {
                key: "Enter".into()
            }
            .risk(None),
            Risk::Medium
        );
        assert_eq!(
            BrowserAction::Press {
                key: "Return".into()
            }
            .risk(None),
            Risk::Medium
        );
        assert_eq!(
            BrowserAction::Press { key: "Tab".into() }.risk(None),
            Risk::Low
        );
    }

    #[test]
    fn the_quiet_steps_stay_low_and_the_stoppers_are_nothing() {
        for action in [
            BrowserAction::Type {
                selector: "#q".into(),
                text: "hello".into(),
            },
            BrowserAction::Wait { ms: 500 },
            BrowserAction::WaitFor {
                selector: "#done".into(),
                timeout_ms: 1000,
            },
            BrowserAction::Scroll { dx: 0, dy: 400 },
            BrowserAction::TabList,
            BrowserAction::TabSwitch { index: 2 },
            BrowserAction::TabClose { index: None },
        ] {
            assert_eq!(action.risk(None), Risk::Low, "{}", action.describe());
        }
        assert_eq!(
            BrowserAction::Done {
                summary: "ok".into()
            }
            .risk(None),
            Risk::None
        );
        assert!(BrowserAction::Done {
            summary: "ok".into()
        }
        .is_terminal());
    }

    #[test]
    fn opening_a_new_tab_to_another_site_is_a_navigation_not_a_tab_housekeeping() {
        // The contract lists "Tab 类" as Low. A `tab_new` carrying a URL is that same
        // cross-site move, so it is judged as one; `TabList`/`Switch`/`Close` stay Low.
        // Raised in the PR as the one place I did not follow the letter.
        assert_eq!(
            BrowserAction::TabNew {
                url: "https://other.test/".into()
            }
            .risk(Some("https://example.com")),
            Risk::Medium
        );
        assert_eq!(
            BrowserAction::TabNew {
                url: "https://example.com/other".into()
            }
            .risk(Some("https://example.com")),
            Risk::Low
        );
    }

    #[test]
    fn an_action_recovered_from_reasoning_is_one_tier_up_and_capped() {
        // The composition the loop relies on: the same action is not equally
        // trustworthy from reasoning, and the ceiling is the top tier, not a wrap.
        let typed = BrowserAction::Type {
            selector: "#q".into(),
            text: "hi".into(),
        };
        assert_eq!(typed.risk(None).escalate(), Risk::Medium);
        let eval = BrowserAction::Eval { script: "1".into() };
        assert_eq!(eval.risk(None).escalate(), Risk::High);
    }

    #[test]
    fn hosts_compare_without_the_noise_that_makes_them_look_different() {
        // Uppercase, a path, a port, or a trailing dot must not turn a same-site move
        // into a cross-site one -- or the reverse: no host at all is not "the same host".
        assert_eq!(
            host_of("HTTPS://Example.COM/A?b=1").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("https://example.com.").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("https://example.com:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("about:blank"), None);
        assert_eq!(host_of("/just/a/path"), None);
        // A scheme that is not http(s) has no site to be the same as: the word before
        // the colon is a scheme name, not a host.
        assert_eq!(host_of("mailto:someone@example.com"), None);
        assert_eq!(host_of("ftp://example.com/x"), None);
        assert_eq!(host_of("file:///C:/secrets.txt"), None);
    }

    #[test]
    fn a_non_http_target_is_undeterminable_rather_than_same_site() {
        assert_eq!(
            BrowserAction::Navigate {
                url: "file:///C:/Users/me/.ssh/id_ed25519".into()
            }
            .risk(Some("https://example.com")),
            Risk::Medium,
            "not a site move, and not a safe one either"
        );
    }

    #[test]
    fn parses_navigate() {
        let a =
            parse_browser_action(r#"{"action":"navigate","url":"https://example.com"}"#).unwrap();
        assert!(matches!(a, BrowserAction::Navigate { url } if url == "https://example.com"));
    }

    #[test]
    fn parses_fenced_click() {
        let a =
            parse_browser_action("```json\n{\"action\":\"click\",\"selector\":\"text=More\"}\n```")
                .unwrap();
        assert!(matches!(a, BrowserAction::Click { selector } if selector == "text=More"));
    }

    #[test]
    fn parses_done_from_prose() {
        let a = parse_browser_action("Result: {\"action\":\"done\",\"summary\":\"ok\"}").unwrap();
        assert!(matches!(a, BrowserAction::Done { summary } if summary == "ok"));
    }

    #[test]
    fn parses_scroll_and_wait_for() {
        let a = parse_browser_action(r#"{"action":"scroll","dy":800}"#).unwrap();
        assert!(matches!(a, BrowserAction::Scroll { dy: 800, .. }));
        let b =
            parse_browser_action(r#"{"action":"wait_for","selector":"main","timeout_ms":3000}"#)
                .unwrap();
        assert!(matches!(
            b,
            BrowserAction::WaitFor {
                timeout_ms: 3000,
                ..
            }
        ));
    }

    #[test]
    fn parses_tab_actions() {
        let a =
            parse_browser_action(r#"{"action":"tab_new","url":"https://example.com"}"#).unwrap();
        assert!(matches!(a, BrowserAction::TabNew { .. }));
        let b = parse_browser_action(r#"{"action":"tab_close"}"#).unwrap();
        assert!(matches!(b, BrowserAction::TabClose { index: None }));
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(parse_browser_action(r#"{"action":"teleport"}"#).is_err());
    }

    #[test]
    fn test_is_cancelled_detects_sentinel_files_and_atomic_bool() {
        let temp_dir = std::env::temp_dir().join(format!("cua-test-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // 1. Neither set
        assert!(!is_cancelled(&temp_dir, None));
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!is_cancelled(&temp_dir, Some(&flag)));

        // 2. AtomicBool set
        flag.store(true, Ordering::Relaxed);
        assert!(is_cancelled(&temp_dir, Some(&flag)));
        flag.store(false, Ordering::Relaxed);
        assert!(!is_cancelled(&temp_dir, Some(&flag)));

        // 3. Sentinel file "cancel"
        let sentinel = temp_dir.join("cancel");
        std::fs::write(&sentinel, "stop").unwrap();
        assert!(is_cancelled(&temp_dir, None));
        std::fs::remove_file(&sentinel).unwrap();
        assert!(!is_cancelled(&temp_dir, None));

        // 4. Sentinel file ".cancel"
        let dot_sentinel = temp_dir.join(".cancel");
        std::fs::write(&dot_sentinel, "stop").unwrap();
        assert!(is_cancelled(&temp_dir, None));
        std::fs::remove_file(&dot_sentinel).unwrap();

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_execute_wait_clamping_and_cancellation() {
        let temp_dir = std::env::temp_dir().join(format!("cua-test-wait-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Normal wait (10ms)
        let note_normal = execute_wait(10, &temp_dir, None).await;
        assert_eq!(note_normal, "waited 10ms");

        // Clamped wait with cancellation (simulating cancellation during a 120s wait)
        let flag = Arc::new(AtomicBool::new(true)); // Pre-cancelled
        let note_clamped = execute_wait(120_000, &temp_dir, Some(&flag)).await;
        assert_eq!(note_clamped, "waited 60000ms (clamped from 120000ms)");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_browser_outcome_summary_formats() {
        let outcome_timeout =
            BrowserOutcome::Failed("timeout: task exceeded total timeout of 600s".into());
        assert_eq!(
            outcome_timeout.summary(3),
            "failed: timeout: task exceeded total timeout of 600s"
        );

        let outcome_cancel =
            BrowserOutcome::Failed("cancelled: task cancelled via signal or sentinel file".into());
        assert_eq!(
            outcome_cancel.summary(2),
            "failed: cancelled: task cancelled via signal or sentinel file"
        );

        let outcome_shot = BrowserOutcome::Failed("screenshot failed: browser closed".into());
        assert_eq!(
            outcome_shot.summary(1),
            "failed: screenshot failed: browser closed"
        );
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Loopback model endpoint: replies with the (status, body) pairs in
    /// order — the last pair repeats forever. Counts requests and records
    /// their bodies so tests can assert on the repair note.
    fn loopback_model(
        responses: Vec<(u16, String)>,
    ) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let count = Arc::new(AtomicUsize::new(0));
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (c, b) = (count.clone(), bodies.clone());
        std::thread::spawn(move || {
            let responses = responses;
            for (served, stream) in listener.incoming().flatten().enumerate() {
                let mut stream = stream;
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        _ => {}
                    }
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head_text = String::from_utf8_lossy(&head).to_string();
                let len: usize = head_text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if len > 0 {
                    stream.read_exact(&mut body).ok();
                }
                c.fetch_add(1, Ordering::SeqCst);
                b.lock()
                    .expect("lock")
                    .push(String::from_utf8_lossy(&body).to_string());
                let (status, text) = responses
                    .get(served.min(responses.len() - 1))
                    .cloned()
                    .unwrap_or((500, "{\"error\":\"exhausted\"}".into()));
                let http = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                stream.write_all(http.as_bytes()).ok();
            }
        });
        (format!("http://{addr}/v1"), count, bodies)
    }

    fn config(base: String) -> OpenAiConfig {
        OpenAiConfig {
            api_key: "test".into(),
            base_url: base,
            model: "test-model".into(),
            max_tokens: 100,
        }
    }

    fn page() -> PageSummary {
        PageSummary {
            url: "https://example.com".into(),
            title: "Example".into(),
            text: String::new(),
            ax: String::new(),
        }
    }

    fn legal_reply() -> String {
        r#"{"choices":[{"message":{"content":"{\"action\":\"done\",\"summary\":\"ok\"}"}}]}"#.into()
    }

    /// ① First reply is not JSON, second is a legal action: exactly two
    /// requests, the second carries the repair note, the action lands.
    #[tokio::test]
    async fn content_error_is_retried_with_repair_note_then_succeeds() {
        let (base, count, bodies) = loopback_model(vec![
            (200, "oops, not a json envelope".into()),
            (200, legal_reply()),
        ]);
        let http = reqwest::Client::new();
        let shot = vec![0u8; 16];
        let (action, source) = ask(&http, &config(base), "t", &page(), &[], &shot)
            .await
            .expect("second attempt must succeed");
        assert_eq!(count.load(Ordering::SeqCst), 2, "exactly one retry");
        assert!(matches!(source, ActionSource::Content));
        assert!(matches!(action, BrowserAction::Done { .. }));
        let sent = bodies.lock().expect("lock");
        assert_eq!(sent.len(), 2);
        assert!(
            !sent[0].contains("NOTE:"),
            "the first body is the original prompt"
        );
        assert!(
            sent[1].contains("NOTE: Your previous reply was not a valid JSON action"),
            "the retry body must carry the repair note"
        );
    }

    /// ② A persistently invalid reply exhausts the budget: exactly three
    /// requests, then an error that names the content class.
    #[tokio::test]
    async fn persistent_content_error_exhausts_three_attempts() {
        let (base, count, _) = loopback_model(vec![(200, "still not json".into())]);
        let http = reqwest::Client::new();
        let shot = vec![0u8; 16];
        let err = ask(&http, &config(base), "t", &page(), &[], &shot)
            .await
            .expect_err("budget exhaustion must be an error");
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert!(err.to_string().contains("[invalid JSON]"), "err: {err}");
    }

    /// ③ A 5xx is still a transport retry with the original body (no repair
    /// note injected when the network, not the reply, was the problem).
    #[tokio::test]
    async fn transport_error_retries_the_original_body() {
        let (base, count, bodies) = loopback_model(vec![
            (500, "{\"error\":\"boom\"}".into()),
            (200, legal_reply()),
        ]);
        let http = reqwest::Client::new();
        let shot = vec![0u8; 16];
        let (action, _) = ask(&http, &config(base), "t", &page(), &[], &shot)
            .await
            .expect("retry after 5xx must succeed");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(matches!(action, BrowserAction::Done { .. }));
        let sent = bodies.lock().expect("lock");
        assert!(
            !sent[1].contains("NOTE:"),
            "transport retry keeps the original body"
        );
    }

    /// ④ Run-level: a persistently failing model ends the run as a Failed
    /// outcome with outcome.json and a Fail step record — not a crashed task.
    #[tokio::test]
    #[ignore = "real-machine: needs a real Chromium over CDP; run on innoforge for E2E evidence"]
    async fn persistent_model_failure_writes_failed_outcome_and_audit_trail() {
        let (base, count, _) = loopback_model(vec![(200, "still not json".into())]);
        let run_dir = std::env::temp_dir().join(format!("retry-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&run_dir).expect("run dir");
        // Point the config source of truth at the loopback endpoint.
        std::env::set_var("CUA_HUB_OPENAI_BASE_URL", &base);
        std::env::set_var("OPENAI_API_KEY", "test");
        std::env::set_var("CUA_HUB_MODEL", "test-model");
        let report = run_task(BrowserRunOptions {
            task: "open example.com".into(),
            max_steps: 2,
            dry_run: false,
            model: Some("test-model".into()),
            headed: false,
            start_url: None,
            run_dir: Some(run_dir.clone()),
            screenshot_quality: 50,
            max_tokens: Some(100),
            ..Default::default()
        })
        .await
        .expect("run_task must not error on model failure");
        assert!(matches!(report.outcome, BrowserOutcome::Failed(_)));
        assert!(count.load(Ordering::SeqCst) >= 3);
        let outcome = std::fs::read_to_string(run_dir.join("outcome.json")).expect("outcome.json");
        assert!(
            outcome.contains("model failed at browser step"),
            "outcome: {outcome}"
        );
        let steps = std::fs::read_to_string(run_dir.join("steps.jsonl")).expect("steps.jsonl");
        assert!(steps.contains("\"fail\""), "terminal Fail record: {steps}");
        let _ = std::fs::remove_dir_all(&run_dir);
    }
}
