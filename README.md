# cua-hub

Rust 多引擎 Computer Use Agent 编排平台。

> 不做新的 Agent 模型，做 Agent 的"操作系统"。

同一个任务入口，底层可切换视觉模型（Claude / GPT / 本地 UI-TARS）、
无障碍树、浏览器、脚本执行等不同引擎，按场景自动路由。

## 状态

Phase 2 进行中，Windows 真机验证：

- **视觉引擎**：截图 → 模型决策 → 点击/按键 → 复核截图（含 125% DPI 坐标换算）
- **代码引擎**：模型生成单条 PowerShell/sh 命令并读取输出（只读任务比视觉快两个数量级）
- **无障碍树**：Windows UIA 快照，`--ui-tree` 可与截图一起作为混合上下文

## 引擎

| 引擎 | 触发方式 | 适合 |
|------|----------|------|
| Vision | 默认 / `--engine vision` | 未知 GUI、跨应用操作 |
| Code | 任务含"命令/脚本/磁盘空间/进程列表…"或 `--engine code` | 确定性数据获取、批处理 |
| Access | 计划中（当前提供快照） | 精确控件操作 |

## 快速开始

```bash
cargo build --release
./target/release/cua-hub doctor

# Anthropic 原生 computer use
export ANTHROPIC_API_KEY=sk-ant-...
./target/release/cua-hub run "打开记事本输入 hello world" --yes

# 任意 OpenAI 兼容网关（OpenRouter / OpenAI / Ollama / vLLM）
export OPENROUTER_API_KEY=sk-or-...
./target/release/cua-hub run "打开开始菜单" --yes \
  --model "minimax/minimax-m3.1-flash:free"

# 命令类任务自动走代码引擎
./target/release/cua-hub run "查看 C 盘剩余空间" --yes \
  --model "minimax/minimax-m3.1-flash:free"
```

## 命令

| 命令 | 说明 |
|------|------|
| `cua-hub doctor` | 检查屏幕/输入/模型配置 |
| `cua-hub capture out.png` | 截取主屏 |
| `cua-hub click X Y` | 按物理坐标点击（调试/脚本用） |
| `cua-hub key "ctrl+esc"` | 发送按键组合 |
| `cua-hub tree --depth 2` | 打印 Windows 无障碍树（调试/混合上下文） |
| `cua-hub find "开始"` | 按无障碍名称查找控件并输出矩形 |
| `cua-hub invoke "开始"` | 按名称找到控件并点击其中心 |
| `cua-hub run "<task>"` | 执行任务（默认视觉引擎） |
| `cua-hub run "<task>" --dry-run` | 只规划一步，不操作机器 |
| `cua-hub run "<task>" --provider openai` | 强制使用 OpenAI 兼容网关 |
| `cua-hub run "<task>" --ui-tree` | 截图之外附带浅层无障碍树 |

## 每次运行的产物

`.runs/<时间戳>/` 下包含 `steps.jsonl`（每步动作与结果）、`step-NN.png`
（每步截图）和 `outcome.json`，便于复盘与重放。

## 环境变量

| 变量 | 说明 |
|------|------|
| `ANTHROPIC_API_KEY` | Anthropic 视觉引擎密钥 |
| `ANTHROPIC_BASE_URL` | 可选，自定义 Anthropic 网关 |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` | OpenAI 兼容引擎密钥 |
| `CUA_HUB_OPENAI_BASE_URL` | 默认取 `OPENROUTER_BASE_URL`，否则 `https://api.openai.com/v1` |
| `CUA_HUB_MODEL` | Anthropic 默认 `claude-sonnet-4-5`；OpenAI 默认取 `OPENROUTER_MODEL` 或 `gpt-4o-mini` |
| `CUA_HUB_YES` | 设为 1 跳过危险动作确认 |

## 架构

```
cua-cli  ->  cua-router  ->  cua-engine-vision (Anthropic / OpenAI-compatible)
   |                              |
   +--> cua-io (xcap capture, enigo input)
   +--> cua-core (Action / Observation / Engine traits)
```

坐标链：模型截图像素 → 物理像素（`Frame.scale`）→ 输入逻辑像素
（`Frame.input_scale`，已处理 Windows 125% DPI 缩放）。

细节见 [docs/DESIGN.md](docs/DESIGN.md)，路线图见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## 许可证

MIT
