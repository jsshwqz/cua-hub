# cua-hub

Rust 多引擎 Computer Use Agent 编排平台。

> 不做新的 Agent 模型，做 Agent 的"操作系统"。

同一个任务入口，底层可切换视觉模型（Claude / GPT / 本地 UI-TARS）、
无障碍树、浏览器、脚本执行等不同引擎，按场景自动路由。

## 状态

Phase 1 — 视觉引擎 MVP（进行中）

## 快速开始

```bash
cargo build --release
export ANTHROPIC_API_KEY=sk-ant-...
./target/release/cua-hub doctor
./target/release/cua-hub run "打开记事本输入 hello world" --yes --max-steps 15
```

## 命令

| 命令 | 说明 |
|------|------|
| `cua-hub doctor` | 检查屏幕/输入/模型配置 |
| `cua-hub capture out.png` | 截取主屏 |
| `cua-hub run "<task>"` | 执行任务（默认视觉引擎） |
| `cua-hub run "<task>" --dry-run` | 只规划不执行（安全预览） |

## 环境变量

| 变量 | 说明 |
|------|------|
| `ANTHROPIC_API_KEY` | 视觉引擎密钥 |
| `ANTHROPIC_BASE_URL` | 可选，自定义网关 |
| `CUA_HUB_MODEL` | 默认 `claude-sonnet-4-5` |
| `CUA_HUB_YES` | 设为 1 跳过危险动作确认 |

## 许可证

MIT
