# computer-use-agent

一个用 **Rust** 编写的统一 Computer Use Agent 运行时：像人一样看屏幕、点鼠标、敲键盘，
而"大脑"可插拔 —— Anthropic Computer Use、OpenAI CUA、Gemini、UI-TARS 等
都通过统一的 `Brain` 接口接入，一套执行层、多个大脑可切换、可横向评测。

## 架构速览

| Crate | 职责 |
|---|---|
| `cua-types` | 统一动作协议（Action）/ 观测 / 安全等级 |
| `cua-runtime` | 执行核心：截屏、键鼠注入、安全围栏、审计日志 |
| `cua-brain` | 大脑抽象 trait + Mock 大脑（真实 provider 在 Phase 2 接入） |
| `cua-orchestrator` | 任务循环：观测 → 规划 → 执行（完整编排在 Phase 3） |
| `cua-bench` | 评测集与打分（Phase 4） |
| `cua-web` | Web UI：屏幕直播/接管（Phase 3） |
| `apps/cua-cli` | 命令行入口：screenshot / exec / info |

详见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 与规划书 [PLAN.md](PLAN.md)。

## 快速开始（Linux / X11）

```bash
# 1. 装 Rust（已装可跳过）
bash scripts/install-rust.sh

# 2. 构建
cargo build --workspace

# 3. 截一张屏（需要 X11 会话，DISPLAY 已设置）
./target/debug/cua-cli screenshot --out /tmp/shot.png

# 4. 干跑一组动作（只记录、不执行 —— 先看审计日志再放行）
./target/debug/cua-cli exec --file examples/actions.json --mode dry-run
./target/debug/cua-cli exec --file examples/actions.json --mode autonomous
```

## 安全模型（硬性）

1. 默认 `dry-run`：只写审计日志，不真正动鼠标键盘。
2. `supervised` 模式：每个 Input/Risky 动作都要确认才执行。
3. `autonomous` 模式：全自动，但**每一个动作都进审计日志**（JSONL）。
4. API key 只走环境变量 / Secure Vault，永不进仓库。

## 状态

- Phase 0（立项/骨架/CI）：完成
- Phase 1（执行核心）：完成 Linux/X11 版（截屏走 x11rb，键鼠走 enigo）
- Phase 2（真实大脑 provider）：未开始，等 API key
