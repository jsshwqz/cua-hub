# cua-hub

Rust 多引擎 Computer Use Agent 编排平台。

> 不做新的 Agent 模型，做 Agent 的"操作系统"。

同一个任务入口，底层可切换视觉模型（Claude / GPT / 本地 UI-TARS）、
无障碍树、浏览器、脚本执行等不同引擎，按场景自动路由。

## 状态

Phase 2 进行中，Windows 真机验证：

- **视觉引擎**：截图 → 模型决策 → 点击/按键 → 复核截图（含 125% DPI 坐标换算）
- **代码引擎**：模型生成单条 PowerShell/sh 命令并读取输出（只读任务比视觉快两个数量级）
- **无障碍树**：Windows UIA 与 Linux AT-SPI2 快照 + 按名查找（Linux 在 WSL/GTK3 真机验证）
- **混合上下文**：`--ui-tree` 把浅层无障碍树与截图一起喂给视觉模型
- **浏览器引擎**：隔离 profile 拉起 Chrome/Edge，CDP 驱动导航/点击/输入/滚动/等待/多标签；同源 iframe 内可定位；附带无障碍树作为模型上下文
- **跨引擎编排**：一句话拆成 browser→code→vision 计划，前一步结果用 `{{name}}` 注入后一步
- **失败恢复**：同引擎带错误重试（自动加预算），仍失败则重新规划并换引擎，两条路径均已真机验证
- **动作校验**：桌面动作后比对屏幕指纹，没变化会在步骤记录里提示模型换方法
- **MCP Server**：`cua-hub serve` 通过 stdio 把整台机器暴露给其他 Agent（协议冒烟测试通过）

## 引擎

| 引擎 | 触发方式 | 适合 |
|------|----------|------|
| Vision | 默认 / `--engine vision` | 未知 GUI、跨应用操作 |
| Code | 任务含"命令/脚本/磁盘空间/进程列表…"或 `--engine code` | 确定性数据获取、批处理 |
| Browser | 任务含 URL / "浏览器/网页"，或 `cua-hub browser` | 网页读取、点击、表单、滚动、多标签、同源 iframe |
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

# 含 URL 的任务自动走浏览器引擎（headless，隔离 profile）
./target/release/cua-hub run "打开 https://example.com 并告诉我页面标题" \
  --model "minimax/minimax-m3.1-flash:free"

# 也可以显式进入浏览器模式并指定起始页
./target/release/cua-hub browser "点击 More information 并告诉我新标题" \
  --start-url https://example.com --headed

# 跨引擎：浏览器取数据，代码/桌面使用数据
./target/release/cua-hub orchestrate \
  "打开 https://example.com 读取页面标题，然后用命令打印出这个标题" \
  --model "minimax/minimax-m3.1-flash:free"
```

## 命令

| 命令 | 说明 |
|------|------|
| `cua-hub doctor` | 检查屏幕/输入/模型配置 |
| `cua-hub capture out.png` | 截取主屏 |
| `cua-hub click X Y` | 按物理坐标点击（调试/脚本用） |
| `cua-hub key "ctrl+esc"` | 发送按键组合 |
| `cua-hub tree --depth 2` | 打印无障碍树（Windows UIA / Linux AT-SPI） |
| `cua-hub find "开始"` | 按无障碍名称查找控件并输出矩形 |
| `cua-hub invoke "开始"` | 按名称找到控件并点击其中心 |
| `cua-hub run "<task>"` | 执行任务（默认视觉引擎） |
| `cua-hub run "<task>" --dry-run` | 只规划一步，不操作机器 |
| `cua-hub run "<task>" --provider openai` | 强制使用 OpenAI 兼容网关 |
| `cua-hub run "<task>" --ui-tree` | 截图之外附带浅层无障碍树 |
| `cua-hub browser "<task>" --start-url <url>` | 浏览器任务（默认 headless） |
| `cua-hub orchestrate "<task>"` | 跨引擎计划 + 执行（`--dry-run` 只看计划；`--retries/--max-replans` 控制恢复） |
| `cua-hub runs` | 列出 `.runs` 下的历史运行（任务、结果、步数） |
| `cua-hub replay <run_dir>` | 按 `steps.jsonl` 逐步重放（`--dry-run` 预览） |
| `cua-hub serve` | 以 MCP stdio server 运行，供其他 Agent 调用 |

## MCP 接入

任何支持 MCP 的客户端（Claude Desktop、Cursor、Qoder 等）都可以这样挂载：

```json
{
  "mcpServers": {
    "cua-hub": {
      "command": "C:\\\\path\\\\to\\\\cua-hub.exe",
      "args": ["serve"],
      "env": { "OPENROUTER_API_KEY": "sk-or-..." }
    }
  }
}
```

暴露 27 个工具：

| 工具 | 作用 |
|------|------|
| `run_task` | 自然语言任务（视觉/代码/浏览器引擎），同步阻塞返回摘要与产物路径 |
| `start_task` / `task_status` / `task_cancel` | 后台任务：立即返回 job_id，可查进度、可取消 |
| `browser_task` | 在隔离 Chromium 里执行网页任务（CDP） |
| `orchestrate` | 跨引擎计划与执行，步骤间传值 |
| `list_runs` / `replay_run` | 查看历史运行 / 重放某次运行 |
| `capture_screen` | 返回主屏 JPEG + 尺寸 |
| `accessibility_tree` | 可读的 UIA 树（窗口/按钮/输入框 + 矩形） |
| `find_control` / `invoke_control` | 按无障碍名称定位；invoke 优先走 Invoke 模式，失败再点中心 |
| `set_control_text` | 用 Value/EditableText 模式直接设值（不模拟键盘） |
| `control_state` | 读取启用/可见、勾选、文本值、展开状态、范围值 |
| `click` / `move` / `drag` | 左/右/中键点击（可双击）、移动指针、左键拖拽 |
| `scroll` | 在指定坐标滚轮（dx/dy，支持横向） |
| `type_text` | 向焦点窗口输入任意 Unicode 文本 |
| `key` | 发送按键组合（PageUp/PageDown/方向键/系统快捷键等） |
| `mouse_down` / `mouse_up` | 按住/释放鼠标键（Shift 多选、Ctrl 拖拽） |
| `key_down` / `key_up` | 按住/释放按键或组合键 |
| `clipboard_get` / `clipboard_set` | 剪贴板文本读写（常驻实例） |
| `screen_info` | 主屏尺寸、原点、DPI、光标位置 |

控件名支持 `id:<AutomationId>` 形式，用于名称动态或为空的控件，例如
Win32 编辑框 `id:15`、计算器按键 `id:num1Button`、结果显示 `id:CalculatorResults`。

远程/容器内调用可以走 HTTP（streamable POST）：

```bash
./target/release/cua-hub serve --http 127.0.0.1:8765

curl -s http://127.0.0.1:8765/health
curl -s -X POST http://127.0.0.1:8765/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

HTTP 模式默认只允许绑定回环地址。设置 `CUA_HUB_MCP_TOKEN` 后，所有 `/mcp`
请求必须带 `Authorization: Bearer <token>`：

```bash
CUA_HUB_MCP_TOKEN=my-secret ./target/release/cua-hub serve --http 127.0.0.1:8765
```

确需绑定非回环地址时，必须设置 token；`CUA_HUB_ALLOW_REMOTE=1` 仅用于你自己
已经在前面加了鉴权代理的场景。

本地协议冒烟测试：`node scripts/mcp_smoke.mjs`。

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
| `CUA_HUB_MCP_TOKEN` | HTTP 模式的 Bearer token；设置后强制鉴权 |
| `CUA_HUB_ALLOW_REMOTE` | 允许无 token 绑定非回环地址（仅限已有前置鉴权） |

## CI

已启用：[`.github/workflows/ci.yml`](.github/workflows/ci.yml) 在
**Windows** 和 **Ubuntu 24.04** 上跑 fmt / clippy / test / release build，
两条线均通过。`ci/github-actions-ci.yml` 是同一份配置的模板备份。

Ubuntu 22.04 不可用：xcap 依赖的 libspa 0.10 需要较新的 pipewire 头文件；
另外需要 `libgbm-dev` / `libdrm-dev` 才能完成链接。

## 架构

```
cua-cli  ->  cua-router  ->  cua-engine-vision (Anthropic / OpenAI-compatible)
   |                              |
   +--> cua-io (xcap capture, enigo input)
   +--> cua-core (Action / Observation / Engine traits)
```

坐标链：模型截图像素 → 物理像素（`Frame.scale`）→ 输入逻辑像素
（`Frame.input_scale`，已处理 Windows 125% DPI 缩放）。

细节见 [docs/DESIGN.md](docs/DESIGN.md)，路线图见 [docs/ROADMAP.md](docs/ROADMAP.md)，
仓库内各版本对照见 [docs/VERSIONS.md](docs/VERSIONS.md)。

## 许可证

MIT
