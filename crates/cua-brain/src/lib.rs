//! 大脑抽象层。
//!
//! `Brain` 是所有模型/provider 必须实现的接口：看着观测想下一步动作。
//! Phase 1 只提供 `MockBrain`（脚本大脑，用于打通执行链路与测试）；
//! Anthropic / OpenAI / Gemini / UI-TARS 在 Phase 2 按此 trait 接入。

use anyhow::Result;
use cua_types::{Action, Observation};

pub trait Brain {
    fn name(&self) -> &str;

    /// 根据任务与当前观测，给出下一步动作序列。
    /// 返回空 Vec 表示任务完成。
    fn next_actions(&mut self, task: &str, observation: &Observation) -> Result<Vec<Action>>;
}

/// 脚本大脑：按预置脚本逐步吐动作，脚本耗尽后返回空（完成）。
/// 从 JSON 文件加载脚本，可在不接任何真实模型时验证整条执行链路。
pub struct MockBrain {
    steps: std::collections::VecDeque<Vec<Action>>,
}

impl MockBrain {
    pub fn new(steps: Vec<Vec<Action>>) -> Self {
        Self {
            steps: steps.into(),
        }
    }

    pub fn from_json_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let steps: Vec<Vec<Action>> = serde_json::from_str(&text)?;
        Ok(Self::new(steps))
    }
}

impl Brain for MockBrain {
    fn name(&self) -> &str {
        "mock"
    }

    fn next_actions(&mut self, _task: &str, _observation: &Observation) -> Result<Vec<Action>> {
        Ok(self.steps.pop_front().unwrap_or_default())
    }
}
