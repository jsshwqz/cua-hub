//! Runtime：持有键鼠注入器 + 策略 + 审计，执行动作的唯一入口。

use anyhow::{Context, Result};
use cua_types::{Action, MouseButton, Observation};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use std::path::PathBuf;

use crate::audit::{now_ms, AuditLog};
use crate::capture;
use crate::guard::{Confirmer, Decision, Policy};
use crate::input;

pub struct Runtime {
    enigo: Enigo,
    policy: Policy,
    audit: AuditLog,
    shot_dir: PathBuf,
}

pub struct ExecOutcome {
    pub executed: bool,
    pub observation: Option<Observation>,
}

impl Runtime {
    pub fn new(policy: Policy, audit_path: PathBuf, shot_dir: PathBuf) -> Result<Self> {
        let enigo = Enigo::new(&Settings::default())
            .context("init enigo (needs an X11 session; is DISPLAY set?)")?;
        std::fs::create_dir_all(&shot_dir).context("create screenshot dir")?;
        if let Some(parent) = audit_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).context("create audit dir")?;
            }
        }
        Ok(Self {
            enigo,
            policy,
            audit: AuditLog::new(audit_path),
            shot_dir,
        })
    }

    /// 观测：截屏落盘并返回 Observation。不经安全围栏（只读操作），
    /// 但 orchestrator 的每步观测都会体现在后续动作的审计链里。
    pub fn observe(&mut self) -> Result<Observation> {
        let path = self.shot_dir.join(format!("shot-{}.png", now_ms()));
        let (width, height) = capture::capture_to_png(&path)?;
        Ok(Observation {
            width,
            height,
            screenshot_path: Some(path),
            taken_at_ms: now_ms(),
        })
    }

    /// 执行单个动作：先过安全围栏，再执行，全程审计。
    pub fn execute<C: Confirmer + ?Sized>(
        &mut self,
        action: &Action,
        confirmer: &mut C,
    ) -> Result<ExecOutcome> {
        match self.policy.decide(action, confirmer) {
            Decision::Skip(reason) => {
                self.audit
                    .record(action, "skip", &reason, None)
                    .context("audit")?;
                tracing::info!(action = %action.describe(), reason, "skipped");
                Ok(ExecOutcome {
                    executed: false,
                    observation: None,
                })
            }
            Decision::Execute => {
                let result = self.execute_inner(action);
                match &result {
                    Ok(obs) => {
                        let detail = obs
                            .as_ref()
                            .and_then(|o| o.screenshot_path.as_ref())
                            .map(|p| p.display().to_string());
                        self.audit
                            .record(action, "execute", "ok", detail.as_deref())
                            .context("audit")?;
                    }
                    Err(e) => {
                        self.audit
                            .record(action, "execute", &format!("error: {e:#}"), None)
                            .context("audit")?;
                    }
                }
                let observation = result?;
                Ok(ExecOutcome {
                    executed: true,
                    observation,
                })
            }
        }
    }

    fn execute_inner(&mut self, action: &Action) -> Result<Option<Observation>> {
        match action {
            Action::Screenshot => {
                let obs = self.observe()?;
                Ok(Some(obs))
            }
            Action::MouseMove { x, y } => {
                self.enigo.move_mouse(*x, *y, Coordinate::Abs)?;
                Ok(None)
            }
            Action::Click { button, at } => {
                if let Some((x, y)) = at {
                    self.enigo.move_mouse(*x, *y, Coordinate::Abs)?;
                }
                self.enigo.button(to_button(*button), Direction::Click)?;
                Ok(None)
            }
            Action::DoubleClick { at } => {
                self.enigo.move_mouse(at.0, at.1, Coordinate::Abs)?;
                self.enigo.button(Button::Left, Direction::Click)?;
                self.enigo.button(Button::Left, Direction::Click)?;
                Ok(None)
            }
            Action::Scroll { dx, dy } => {
                if *dy != 0 {
                    self.enigo.scroll(*dy, Axis::Vertical)?;
                }
                if *dx != 0 {
                    self.enigo.scroll(*dx, Axis::Horizontal)?;
                }
                Ok(None)
            }
            Action::Drag { from, to } => {
                self.enigo.move_mouse(from.0, from.1, Coordinate::Abs)?;
                self.enigo.button(Button::Left, Direction::Press)?;
                self.enigo.move_mouse(to.0, to.1, Coordinate::Abs)?;
                self.enigo.button(Button::Left, Direction::Release)?;
                Ok(None)
            }
            Action::TypeText { text } => {
                self.enigo.text(text)?;
                Ok(None)
            }
            Action::KeyPress { combo } => {
                input::press_combo(&mut self.enigo, combo)?;
                Ok(None)
            }
            Action::Wait { ms } => {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
                Ok(None)
            }
            Action::LaunchApp { program, args } => {
                std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .with_context(|| format!("launch {program}"))?;
                Ok(None)
            }
        }
    }
}

fn to_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}
