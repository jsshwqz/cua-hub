//! 最小任务循环（Phase 1）：截屏观测 → 问大脑 → 执行动作 → 再观测，
//! 直到大脑返回空动作（完成）或达到步数上限。
//! 失败重试、检查点续跑、任务队列在 Phase 3 补齐。

use anyhow::{bail, Result};
use cua_brain::Brain;
use cua_runtime::{Confirmer, Runtime};
use cua_types::Action;

pub struct TaskReport {
    pub steps: usize,
    pub actions_executed: usize,
    pub finished: bool,
}

pub fn run_task<B: Brain, C: Confirmer>(
    runtime: &mut Runtime,
    brain: &mut B,
    confirmer: &mut C,
    task: &str,
    max_steps: usize,
) -> Result<TaskReport> {
    let mut report = TaskReport {
        steps: 0,
        actions_executed: 0,
        finished: false,
    };

    for step in 0..max_steps {
        report.steps = step + 1;
        // 每步先观测（截屏），大脑基于观测决策
        let obs = runtime.observe()?;
        let actions = brain.next_actions(task, &obs)?;
        if actions.is_empty() {
            report.finished = true;
            tracing::info!(step, "brain signalled completion");
            break;
        }
        for action in &actions {
            runtime.execute(action, confirmer)?;
            report.actions_executed += 1;
        }
        // 动作后留一拍给界面反应
        runtime.execute(&Action::Wait { ms: 200 }, confirmer)?;
    }

    if !report.finished {
        bail!("task did not finish within {max_steps} steps");
    }
    Ok(report)
}
