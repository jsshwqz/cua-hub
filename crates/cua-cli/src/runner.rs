//! Shared task runner used by the CLI and the MCP server.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use cua_core::{Action, Engine, EngineKind, StepRecord};
use cua_engine_code::CodeEngine;
use cua_engine_vision::{AnthropicConfig, OpenAiConfig, OpenAiVisionEngine, VisionEngine};
use cua_router::{Router, RuleRouter};
use tracing::{info, warn};

/// Async approval hook for actions that need a decision (used by MCP elicitation).
///
/// The shape lives in `cua_core::policy` now because the browser engine runs its own
/// loop and has to ask through the same channel; keeping it here would have meant two
/// gates disagreeing about what `yes` buys.
pub type Approver = cua_core::policy::Approver;

pub async fn confirm(
    action: &Action,
    yes: bool,
    allow_high: bool,
    approver: Option<&Approver>,
) -> Result<bool> {
    // One ladder, one implementation. The CLI adds nothing to it: this is the same
    // gate the browser loop consults before it fires an action.
    cua_core::policy::confirm(action.risk(), &describe(action), yes, allow_high, approver).await?;
    Ok(true)
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum EngineArg {
    Vision,
    Access,
    Browser,
    Code,
}

impl From<EngineArg> for EngineKind {
    fn from(v: EngineArg) -> Self {
        match v {
            EngineArg::Vision => EngineKind::Vision,
            EngineArg::Access => EngineKind::Access,
            EngineArg::Browser => EngineKind::Browser,
            EngineArg::Code => EngineKind::Code,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    /// Anthropic if its key is set, otherwise any OpenAI-compatible gateway.
    Auto,
    /// Anthropic Computer Use (native tool protocol).
    Anthropic,
    /// OpenAI / OpenRouter / Ollama / vLLM chat completions.
    Openai,
}

#[derive(Clone)]
pub struct RunOptions {
    pub task: String,
    pub max_steps: u32,
    /// Skip confirmation for medium-risk actions.
    pub yes: bool,
    /// Permit high-risk actions (destructive commands); never implied by `yes`.
    pub allow_high: bool,
    pub dry_run: bool,
    pub engine: Option<EngineKind>,
    pub provider: ProviderArg,
    pub model: Option<String>,
    pub max_edge: u32,
    pub run_dir: Option<PathBuf>,
    pub settle_ms: u64,
    pub ui_tree: bool,
    /// Attach the unified element registry (perception access+OCR merge) to
    /// each vision step. Registry numbers are for reading only — actions
    /// still address controls via window_state element indexes or targets.
    pub ui_elements: bool,
    /// Show the browser window instead of headless mode.
    pub browser_headed: bool,
    /// Optional async approval hook used instead of `allow_high` when a
    /// high-risk action appears (MCP stdio wires this to elicitation).
    pub approver: Option<Approver>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            task: String::new(),
            max_steps: 20,
            yes: false,
            allow_high: false,
            dry_run: false,
            engine: None,
            provider: ProviderArg::Auto,
            model: None,
            max_edge: 1568,
            run_dir: None,
            settle_ms: 600,
            ui_tree: false,
            ui_elements: false,
            browser_headed: false,
            approver: None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Completed(String),
    Failed(String),
    StepLimit,
    /// Planned action already described as text.
    DryRun(String),
}

#[derive(Debug)]
pub struct RunReport {
    pub outcome: Outcome,
    pub run_dir: PathBuf,
    pub steps: usize,
}

impl RunReport {
    pub fn summary(&self) -> String {
        match &self.outcome {
            Outcome::Completed(s) => format!("completed: {s}"),
            Outcome::Failed(s) => format!("failed: {s}"),
            Outcome::StepLimit => format!("stopped after {} steps", self.steps),
            Outcome::DryRun(planned) => format!("dry-run planned: {planned}"),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self.outcome, Outcome::Completed(_) | Outcome::DryRun(_))
    }
}

pub async fn run_task(opts: RunOptions) -> Result<RunReport> {
    let decision = match opts.engine {
        Some(kind) => RuleRouter::forced(kind).route(&opts.task),
        None => RuleRouter::new().route(&opts.task),
    };
    info!(
        engine = decision.kind.as_str(),
        reason = %decision.reason,
        "routing task"
    );
    if decision.kind == EngineKind::Access {
        warn!(
            requested = decision.kind.as_str(),
            "engine not implemented yet; falling back to vision"
        );
    }

    // Browser tasks run in their own CDP session and action space -- and now carry
    // the same switches, because a gate that only guards the single-action path is a
    // gate with a door in it.
    if decision.kind == EngineKind::Browser {
        let report = cua_engine_browser::run_task(cua_engine_browser::BrowserRunOptions {
            task: opts.task.clone(),
            max_steps: opts.max_steps,
            dry_run: opts.dry_run,
            model: opts.model.clone(),
            headed: opts.browser_headed,
            start_url: None,
            run_dir: opts.run_dir.clone(),
            screenshot_quality: 70,
            max_tokens: None,
            indicate_mouse: None,
            pointer: Some(crate::pointer::pointer_mover()),
            yes: opts.yes,
            allow_high: opts.allow_high,
            approver: opts.approver.clone(),
            timeout: None,
            cancel: None,
        })
        .await?;
        let outcome = match report.outcome {
            cua_engine_browser::BrowserOutcome::Completed(s) => Outcome::Completed(s),
            cua_engine_browser::BrowserOutcome::Failed(s) => Outcome::Failed(s),
            cua_engine_browser::BrowserOutcome::StepLimit => Outcome::StepLimit,
            cua_engine_browser::BrowserOutcome::DryRun(s) => Outcome::DryRun(s),
        };
        return Ok(RunReport {
            outcome,
            run_dir: report.run_dir,
            steps: report.steps,
        });
    }

    let provider = resolve_provider(opts.provider)?;
    let mut engine = build_engine(decision.kind, provider, opts.model.clone())?;
    let vision_engine = engine.kind() == EngineKind::Vision;

    if opts.dry_run {
        eprintln!("[dry-run] max_steps={}", opts.max_steps);
    }

    let run_dir = opts.run_dir.clone().unwrap_or_else(default_run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let log_path = run_dir.join("steps.jsonl");
    let mut log_file = std::fs::File::create(&log_path)?;
    let mut history: Vec<StepRecord> = Vec::new();
    let mut outcome = Outcome::StepLimit;

    for index in 0..opts.max_steps {
        let frame = if vision_engine {
            let frame = cua_io::observe(opts.max_edge, 80)?;
            let shot_path = run_dir.join(format!("step-{index:02}.png"));
            std::fs::write(&shot_path, &frame.png)?;
            Some(frame)
        } else {
            None
        };
        let mut observation = frame
            .as_ref()
            .map(|f| f.observation.clone())
            .unwrap_or_else(cua_core::Observation::empty);
        if opts.ui_tree && vision_engine {
            // Depth 1 keeps the per-step COM cost under a second; deeper trees
            // are available through `cua-hub tree` for debugging.
            match cua_engine_access::snapshot(60, 1) {
                Ok(snap) => observation.ui_tree = Some(snap.to_text()),
                Err(e) => tracing::warn!(error = %e, "ui tree snapshot failed"),
            }
        }
        if opts.ui_elements && vision_engine {
            // Reuse the frame this step already captured — OCR and the tree
            // walk read the same pixels the model is about to see.
            match build_elements_text(&observation) {
                Ok(text) if !text.is_empty() => observation.ui_elements = Some(text),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "ui elements build failed"),
            }
        }

        let action = engine
            .next_action(&opts.task, &observation, &history)
            .await
            .with_context(|| format!("engine failed at step {index}"))?;

        let mut record = StepRecord::new(index, action.clone());
        eprintln!(
            "[{index:02}] {} {}",
            action.risk().as_str(),
            describe(&action)
        );

        match action {
            Action::Done { summary } => {
                record.note = Some(format!("completed: {summary}"));
                write_record(&mut log_file, &record)?;
                outcome = Outcome::Completed(summary);
                break;
            }
            Action::Fail { reason } => {
                record.note = Some(format!("failed: {reason}"));
                write_record(&mut log_file, &record)?;
                outcome = Outcome::Failed(reason);
                break;
            }
            Action::Wait { ms } => {
                write_record(&mut log_file, &record)?;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                continue;
            }
            Action::Screenshot => {
                write_record(&mut log_file, &record)?;
                continue;
            }
            _ => {}
        }

        if opts.dry_run {
            write_record(&mut log_file, &record)?;
            outcome = Outcome::DryRun(describe(&action));
            break;
        }

        if !confirm(&action, opts.yes, opts.allow_high, opts.approver.as_ref()).await? {
            record.note = Some("skipped: user declined".into());
            write_record(&mut log_file, &record)?;
            outcome = Outcome::Failed("action declined by user".into());
            break;
        }

        if let Action::RunCommand { command, shell, .. } = &action {
            match cua_io::run_command(command, *shell).await {
                Ok(out) => record.note = Some(out.to_note()),
                Err(e) => record.note = Some(format!("error: {e}")),
            }
            record.input_scale = frame.as_ref().map(|f| f.input_scale);
            write_record(&mut log_file, &record)?;
            engine.on_action_result(&record).await;
            history.push(record);
            continue;
        }

        // Cheap before/after fingerprint so a no-op click or typed key gets
        // reported back to the model instead of silently "succeeding".
        let before_hash = if vision_engine {
            cua_io::screen_hash().ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking({
            let action = action.clone();
            let scale = frame.as_ref().map(|f| f.input_scale).unwrap_or(1.0);
            move || cua_io::apply(&action, scale)
        })
        .await
        .context("input task panicked")?;

        record.input_scale = frame.as_ref().map(|f| f.input_scale);
        match &result {
            Ok(()) => record.note = Some("executed".into()),
            Err(e) => record.note = Some(format!("error: {e}")),
        }

        tokio::time::sleep(Duration::from_millis(opts.settle_ms)).await;

        if let (Ok(()), Some(before)) = (&result, before_hash) {
            if let Ok(after) = cua_io::screen_hash() {
                if after == before {
                    record.note = Some(
                        "executed, but the screen did not change; try a different approach".into(),
                    );
                }
            }
        }

        write_record(&mut log_file, &record)?;
        engine.on_action_result(&record).await;
        history.push(record);
    }

    let report = RunReport {
        outcome,
        run_dir: run_dir.clone(),
        steps: history.len(),
    };
    std::fs::write(
        run_dir.join("outcome.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "task": opts.task,
            "outcome": report.summary(),
            "steps": report.steps,
        }))?,
    )?;
    eprintln!("\n{}", report.summary());
    eprintln!("artifacts: {}", run_dir.display());
    Ok(report)
}

fn resolve_provider(requested: ProviderArg) -> Result<ProviderArg> {
    if requested != ProviderArg::Auto {
        return Ok(requested);
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return Ok(ProviderArg::Anthropic);
    }
    if std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("OPENROUTER_API_KEY").is_ok() {
        return Ok(ProviderArg::Openai);
    }
    bail!("no model credentials: set ANTHROPIC_API_KEY, OPENAI_API_KEY or OPENROUTER_API_KEY")
}

fn build_engine(
    kind: EngineKind,
    provider: ProviderArg,
    model: Option<String>,
) -> Result<Box<dyn Engine>> {
    if kind == EngineKind::Code {
        let config = OpenAiConfig::from_env()?.with_model(model);
        eprintln!("provider: llm-code model={}", config.model);
        return Ok(Box::new(CodeEngine::new(config)));
    }

    match provider {
        ProviderArg::Anthropic => {
            let mut config = AnthropicConfig::from_env()?;
            if let Some(m) = model {
                config.model = m;
            }
            eprintln!("provider: anthropic model={}", config.model);
            Ok(Box::new(VisionEngine::new(config)))
        }
        ProviderArg::Openai | ProviderArg::Auto => {
            let config = OpenAiConfig::from_env()?.with_model(model);
            eprintln!("provider: openai-compatible model={}", config.model);
            Ok(Box::new(OpenAiVisionEngine::new(config)))
        }
    }
}

fn write_record(file: &mut std::fs::File, record: &StepRecord) -> Result<()> {
    let line = serde_json::to_string(record)?;
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(())
}

pub fn describe(action: &Action) -> String {
    match action {
        Action::Screenshot => "screenshot".into(),
        Action::Click { at, button, double } => format!(
            "{}{}click at ({}, {})",
            if *double { "double " } else { "" },
            button_name(*button),
            at.x,
            at.y
        ),
        Action::Move { to } => format!("move to ({}, {})", to.x, to.y),
        Action::Drag { from, to } => {
            format!("drag ({}, {}) -> ({}, {})", from.x, from.y, to.x, to.y)
        }
        Action::Type { text, .. } => {
            let preview: String = text.chars().take(60).collect();
            format!("type {preview:?}")
        }
        Action::Key { combo } => format!("key {combo}"),
        Action::Scroll { at, dx, dy } => {
            format!("scroll ({dx}, {dy}) at ({}, {})", at.x, at.y)
        }
        Action::Wait { ms } => format!("wait {ms}ms"),
        Action::RunCommand {
            command,
            shell,
            from_reasoning,
        } => {
            let preview: String = command.chars().take(120).collect();
            // Whoever reads the prompt has to be able to see that this line did not
            // come from what the model chose to say.
            let origin = if *from_reasoning {
                " (from reasoning)"
            } else {
                ""
            };
            format!("run [{shell:?}] {preview}{origin}")
        }
        Action::Done { summary } => format!("done: {summary}"),
        Action::Fail { reason } => format!("fail: {reason}"),
    }
}

fn button_name(b: cua_core::MouseButton) -> &'static str {
    match b {
        cua_core::MouseButton::Left => "left ",
        cua_core::MouseButton::Right => "right ",
        cua_core::MouseButton::Middle => "middle ",
    }
}

/// Confidence floor applied to OCR lines before the registry merge (the B3
/// fix for SOC's "ignore the score" defect). Windows Media.Ocr reports 1.0
/// (no score in the API); the floor therefore bites on tesseract.
const OCR_CONFIDENCE_FLOOR: f32 = 0.5;

/// Builds the unified element registry text for one observation: the
/// accessibility snapshot mapped to normalized access elements, OCR lines
/// decoded from the same frame, merged (batch B4) and rendered without
/// coordinates. Empty string means "nothing worth attaching" — callers skip.
fn build_elements_text(observation: &cua_core::Observation) -> anyhow::Result<String> {
    use cua_engine_perception::ocr::{make_backend, prepare_ocr_boxes, OcrBackendKind};
    use cua_engine_perception::registry::{merge_access_and_ocr, AccessElement};
    use cua_engine_perception::Bbox;

    let snap = cua_engine_access::snapshot(60, 1)?;
    let (screen_w, screen_h) = (observation.width as f32, observation.height as f32);
    let access: Vec<AccessElement> = snap
        .nodes
        .iter()
        .filter_map(|n| {
            let (x, y, w, h) = n.rect;
            if w <= 0 || h <= 0 || screen_w <= 0.0 || screen_h <= 0.0 {
                return None;
            }
            let bbox = Bbox::new(
                (x as f32 / screen_w).clamp(0.0, 1.0),
                (y as f32 / screen_h).clamp(0.0, 1.0),
                ((x + w) as f32 / screen_w).clamp(0.0, 1.0),
                ((y + h) as f32 / screen_h).clamp(0.0, 1.0),
            )
            .ok()?;
            Some(AccessElement {
                bbox,
                role: n.control_type.clone(),
                name: (!n.name.is_empty()).then(|| n.name.clone()),
                interactive: is_interactive_role(&n.control_type),
            })
        })
        .collect();

    let mut ocr_boxes = Vec::new();
    if !observation.image.is_empty() {
        let frame = image::load_from_memory(&observation.image)
            .map_err(|e| anyhow::anyhow!("decode observation image: {e}"))?
            .to_rgb8();
        if let Some(backend) = make_backend(OcrBackendKind::from_env())? {
            let lines = backend.detect(&frame)?;
            ocr_boxes = prepare_ocr_boxes(lines, OCR_CONFIDENCE_FLOOR);
        }
    }

    let table = merge_access_and_ocr(&access, &ocr_boxes)?;
    Ok(table.to_model_text())
}

/// Roles treated as actionable when mapping the tree to access elements.
fn is_interactive_role(control_type: &str) -> bool {
    matches!(
        control_type,
        "Button"
            | "MenuItem"
            | "Edit"
            | "CheckBox"
            | "RadioButton"
            | "ComboBox"
            | "ListItem"
            | "TabItem"
            | "Hyperlink"
            | "TreeItem"
            | "SplitButton"
            | "ToggleButton"
            | "Spinner"
            | "Slider"
            | "List"
            | "Tab"
            | "Tree"
            | "Menu"
            | "MenuBar"
            | "ComboBoxItem"
    )
}

pub fn default_run_dir() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    PathBuf::from(".runs").join(stamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_core::ShellKind;

    fn approver(answer: bool) -> Approver {
        std::sync::Arc::new(move |_risk, _description| {
            Box::pin(async move { answer })
                as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        })
    }

    fn command(text: &str) -> Action {
        Action::RunCommand {
            command: text.to_string(),
            shell: ShellKind::Auto,
            from_reasoning: false,
        }
    }

    #[tokio::test]
    async fn a_read_needs_no_decision() {
        assert!(confirm(&command("ls -la"), false, false, None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn an_unlisted_command_asks_the_client_when_yes_is_off() {
        assert!(
            confirm(&command("cargo build"), false, false, Some(&approver(true)))
                .await
                .unwrap()
        );
        let declined = confirm(
            &command("cargo build"),
            false,
            false,
            Some(&approver(false)),
        )
        .await;
        assert!(
            declined.is_err(),
            "a declined confirmation must stop the action"
        );
    }

    #[tokio::test]
    async fn yes_does_not_buy_a_destructive_command() {
        // The bypass chain was: an unrecognised teardown classified as Medium, and
        // `yes` waved every Medium through. The classifier now calls it High, and
        // High is never satisfied by `yes` alone.
        let outcome = confirm(&command("rm -rf /tmp/build"), true, false, None).await;
        let message = outcome.unwrap_err().to_string();
        assert!(
            message.contains("allow-high") || message.contains("allow_high"),
            "the error has to name the switch that would actually allow it: {message}"
        );
    }

    #[tokio::test]
    async fn a_destructive_command_needs_an_explicit_decision() {
        assert!(confirm(
            &command("rm -rf /tmp/build"),
            false,
            false,
            Some(&approver(true))
        )
        .await
        .unwrap());
        assert!(confirm(&command("rm -rf /tmp/build"), false, true, None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn no_channel_means_no_run() {
        // `cargo test` stdin is not a terminal, which is the headless MCP case:
        // silence is not consent, and there is nothing to ask.
        assert!(confirm(&command("cargo build"), false, false, None)
            .await
            .is_err());
    }
}

#[cfg(test)]
mod elements_tests {
    use super::*;

    #[test]
    fn interactive_roles_cover_actionable_controls_only() {
        for role in ["Button", "Edit", "MenuItem", "CheckBox", "Hyperlink"] {
            assert!(is_interactive_role(role), "{role} should be interactive");
        }
        for role in ["Text", "Image", "Pane", "Window", "Document", ""] {
            assert!(
                !is_interactive_role(role),
                "{role} should not be interactive"
            );
        }
    }

    /// Real-machine evidence (Windows session): the registry text builds from
    /// a live snapshot + OCR with no model involved. Ignored by default — run
    /// explicitly: `cargo test -p cua-cli -- --ignored elements_registry_smoke`
    #[test]
    #[ignore = "real-machine: needs a desktop session with OCR language packs"]
    fn elements_registry_smoke() {
        let obs = cua_core::Observation {
            image: std::fs::read("registry-smoke-frame.png").unwrap_or_default(),
            media_type: "image/png".into(),
            width: 0,
            height: 0,
            cursor: None,
            ui_tree: None,
            ui_elements: None,
        };
        // No frame file handy: width/height zero would skip normalization —
        // capture one live instead through the io crate's primary capture.
        let _ = obs;
        let mut obs = cua_core::Observation::empty();
        let captured = cua_io::capture::capture_primary().expect("primary capture");
        obs.image = {
            use image::ImageEncoder;
            let rgb = image::DynamicImage::ImageRgba8(captured.image).to_rgb8();
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .expect("encode");
            png
        };
        obs.width = captured.width;
        obs.height = captured.height;
        let text = build_elements_text(&obs).expect("registry build");
        println!("--- element registry ({} chars) ---", text.len());
        println!("{text}");
        assert!(
            !text.is_empty(),
            "a real desktop yields a non-empty registry"
        );
        assert!(
            !text.contains("bbox"),
            "no coordinates leak into the text form"
        );
    }
}
