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
    /// 常驻剪贴板实例（X11 下必须常驻，见 clipboard 模块说明）；惰性初始化。
    clipboard: Option<arboard::Clipboard>,
}

pub struct ExecOutcome {
    pub executed: bool,
    pub observation: Option<Observation>,
    /// 文本型结果（读剪贴板、屏幕信息这类只读查询的回传内容）
    pub note: Option<String>,
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
            clipboard: None,
        })
    }

    fn clipboard(&mut self) -> Result<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().context("open clipboard")?);
        }
        Ok(self.clipboard.as_mut().expect("clipboard just set"))
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
                    note: None,
                })
            }
            Decision::Execute => {
                let result = self.execute_inner(action);
                match &result {
                    Ok((obs, note)) => {
                        let detail = obs
                            .as_ref()
                            .and_then(|o| o.screenshot_path.as_ref())
                            .map(|p| p.display().to_string())
                            .or_else(|| match action {
                                // 只有屏幕信息可以进审计；剪贴板内容绝不落日志
                                Action::ScreenInfo => note.clone(),
                                _ => None,
                            });
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
                let (observation, note) = result?;
                Ok(ExecOutcome {
                    executed: true,
                    observation,
                    note,
                })
            }
        }
    }

    /// 主屏尺寸（像素）。
    pub fn display_size(&mut self) -> Result<(i32, i32)> {
        Ok(self.enigo.main_display()?)
    }

    /// 当前鼠标位置（像素）。
    pub fn cursor_location(&mut self) -> Result<(i32, i32)> {
        Ok(self.enigo.location()?)
    }

    fn execute_inner(
        &mut self,
        action: &Action,
    ) -> Result<(Option<Observation>, Option<String>)> {
        // 坐标边界校验：超出主屏范围的指向直接报错，不让误坐标
        // 静默落到屏幕外或被系统钳到边缘误触（实机验收反馈）。
        let (sw, sh) = self.display_size().unwrap_or((0, 0));
        let in_bounds = |x: i32, y: i32| -> Result<()> {
            if sw > 0 && (x < 0 || y < 0 || x >= sw || y >= sh) {
                return Err(anyhow::anyhow!(
                    "point ({x}, {y}) is outside the main screen ({sw}x{sh})"
                ));
            }
            Ok(())
        };
        match action {
            Action::Screenshot => {
                let obs = self.observe()?;
                Ok((Some(obs), None))
            }
            Action::MouseMove { x, y } => {
                in_bounds(*x, *y)?;
                self.enigo.move_mouse(*x, *y, Coordinate::Abs)?;
                Ok((None, None))
            }
            Action::Click { button, at } => {
                if let Some((x, y)) = at {
                    in_bounds(*x, *y)?;
                    self.enigo.move_mouse(*x, *y, Coordinate::Abs)?;
                }
                self.enigo.button(to_button(*button), Direction::Click)?;
                Ok((None, None))
            }
            Action::DoubleClick { at } => {
                in_bounds(at.0, at.1)?;
                self.enigo.move_mouse(at.0, at.1, Coordinate::Abs)?;
                self.enigo.button(Button::Left, Direction::Click)?;
                self.enigo.button(Button::Left, Direction::Click)?;
                Ok((None, None))
            }
            Action::Scroll { dx, dy } => {
                if *dy != 0 {
                    self.enigo.scroll(*dy, Axis::Vertical)?;
                }
                if *dx != 0 {
                    self.enigo.scroll(*dx, Axis::Horizontal)?;
                }
                Ok((None, None))
            }
            Action::Drag { from, to } => {
                in_bounds(from.0, from.1)?;
                in_bounds(to.0, to.1)?;
                self.enigo.move_mouse(from.0, from.1, Coordinate::Abs)?;
                self.enigo.button(Button::Left, Direction::Press)?;
                self.enigo.move_mouse(to.0, to.1, Coordinate::Abs)?;
                self.enigo.button(Button::Left, Direction::Release)?;
                Ok((None, None))
            }
            Action::TypeText { text } => {
                self.enigo.text(text)?;
                Ok((None, None))
            }
            Action::KeyPress { combo } => {
                input::press_combo(&mut self.enigo, combo)?;
                Ok((None, None))
            }
            Action::MouseButtonState { button, pressed } => {
                let dir = if *pressed {
                    Direction::Press
                } else {
                    Direction::Release
                };
                self.enigo.button(to_button(*button), dir)?;
                Ok((None, None))
            }
            Action::KeyState { combo, pressed } => {
                input::key_state(&mut self.enigo, combo, *pressed)?;
                Ok((None, None))
            }
            Action::SetClipboard { text } => {
                let text = text.clone();
                crate::clipboard::set_text(self.clipboard()?, &text)?;
                Ok((None, None))
            }
            Action::GetClipboard => {
                let text = crate::clipboard::get_text(self.clipboard()?)?;
                // 超长剪贴板截断回传，避免撑爆大脑上下文；全文长度会注明。
                const CAP: usize = 8000;
                let total = text.chars().count();
                let note = if total > CAP {
                    let head: String = text.chars().take(CAP).collect();
                    format!("{head}… [truncated: clipboard holds {total} chars]")
                } else {
                    text
                };
                Ok((None, Some(note)))
            }
            Action::ScreenInfo => {
                let (w, h) = self.display_size()?;
                let (cx, cy) = self.cursor_location()?;
                Ok((None, Some(format!("screen {w}x{h}, cursor at ({cx}, {cy})"))))
            }
            Action::Wait { ms } => {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
                Ok((None, None))
            }
            Action::LaunchApp { program, args } => {
                std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .with_context(|| format!("launch {program}"))?;
                Ok((None, None))
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
