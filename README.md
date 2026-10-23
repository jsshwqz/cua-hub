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
| `cua-hub console` | 本地 Web 控制台：实时画面、启动/取消任务（默认 `127.0.0.1:8787`） |
| `cua-hub serve` | 以 MCP stdio server 运行，供其他 Agent 调用 |

## Web 控制台

```bash
./target/release/cua-hub console --bind 127.0.0.1:8787
# 打开 http://127.0.0.1:8787/
```

- 每 800ms 刷新主屏 JPEG，右下角显示分辨率、DPI、光标位置
- 可直接输入任务并启动（可选 vision / code / browser 引擎）
- 后台任务列表：状态、步数、摘要，可取消
- 同一进程还挂载了 `/mcp`，Agent 通过它启动的任务会实时出现在面板里
- 设置 `CUA_HUB_MCP_TOKEN` 后，页面与接口都需要 `?token=` 或 Bearer；非回环绑定必须设置 token

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

暴露 38 个工具：

| 工具 | 作用 |
|------|------|
| `run_task` | 自然语言任务（视觉/代码/浏览器引擎），同步阻塞返回摘要与产物路径 |
| `start_task` / `task_status` / `task_cancel` | 后台任务：立即返回 job_id，可查进度、可取消 |
| `browser_task` | 在隔离 Chromium 里执行网页任务（CDP） |
| `orchestrate` | 跨引擎计划与执行，步骤间传值 |
| `list_runs` / `replay_run` | 查看历史运行 / 重放某次运行 |
| `capture_screen` | 返回主屏 JPEG + 尺寸 |
| `list_windows` | 列出可捕获的顶层窗口（标题、应用、坐标、焦点、最小化） |
| `list_apps` | 按进程分组的应用清单（含各自窗口与可启动的 exe 路径）；`include_all` 追加开始菜单里未运行的程序 |
| `launch_app` | 启动应用并等待其窗口出现；已在运行的直接返回现有窗口；`ready=false` 表示超时内没等到窗口 |
| `window_state` | 观测原语：一次返回窗口截图（含菜单/对话框等独立弹层，每张图带自己的 `screenshot_id`/原点）与窗口内 UIA 元素树 + `element_index` + 焦点元素，并给出 `observation_id` 作为作用域令牌。元素编号在同一窗口内保持稳定，因此第二次起默认返回 unified diff（`mode=diff`），`unchanged` 表示映射仍有效，`baseline_reset` 表示旧编号全部作废。树里同时给出控件当前值、可用动作与选中/焦点标记；`accessibility` 还带 `selected_text`、`document_text`、`selected_elements`（支持 Text 模式的控件） |
| `capture_window` | 抓取窗口自身表面（被遮挡也完整），返回 `origin`/`image_scale`/`source`/`hash`，坐标换算由服务端完成；PrintWindow 渲染成全黑的硬件合成分布式表面会自动改用 Windows.Graphics.Capture（`source=GraphicsCapture`），也可用 `CUA_HUB_CAPTURE_BACKEND=wgc` 强制 |
| `wait_for_change` | 阻塞等待窗口或其弹层真的发生变化，用来验证动作是否生效 |
| `perform_secondary_action` | 调用控件除点击之外的无障碍动作：Expand/Collapse/Raise/Scroll Up·Down·Left·Right/Toggle/Select/Focus/Invoke；只接受该控件在树里 `actions=` 列出的名字，猜错返回 `unsupported_action` 与真实清单，执行后回读状态供验证 |
| `focus_window` | 按 id、必须唯一匹配的标题/应用选择器，或 `{app,id}` 把窗口带到前台；0 个或多个匹配时返回候选，不猜 |
| `window_input` | 原子操作：聚焦窗口 → 可选窗口相对坐标点击 → 输入文本 → 可选回车；窗口目标必须唯一，失败返回候选 |
| `accessibility_tree` | 带编号的桌面级 UIA 树（窗口/按钮/输入框 + 矩形）；日常观测请用 `window_state` |
| `find_control` / `invoke_control` | 按无障碍名称定位；invoke 优先走 Invoke 模式，失败再点中心 |
| `invoke_element` | 按 `accessibility_tree` 的编号调用控件（模式同 invoke_control） |
| `set_control_text` / `set_element_value` | 用 Value/EditableText 模式按名称/编号直接设值（不模拟键盘） |
| `control_state` | 读取启用/可见、勾选、文本值、展开状态、范围值 |
| `click` / `move` / `drag` | 左/右/中键点击（可双击）、移动指针、左键拖拽；`click` 支持 `observation_id`+`element_index`，或 `observation_id`+`screenshot_id`+`x/y`（弹层图片的坐标系与主窗口不同，必须指名） |
| `scroll` | 在指定坐标滚轮（dx/dy，支持横向） |
| `type_text` | 向焦点窗口输入任意 Unicode 文本（自动处理 Shift 大写/括号） |
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

**高危动作确认（stdio）**：支持 MCP elicitation 的客户端会收到
`elicitation/create` 请求，对删除/格式化/关机类动作逐次批准或拒绝；
客户端不支持或 60 秒未响应，按拒绝处理。HTTP 模式没有反向通道，高危动作
仍直接阻断，需要显式 `allow_high: true`。验证脚本：
`node scripts/mcp_elicit_test.mjs`。

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

## 作为 Skill / MCP 安装

一键安装（构建 release、复制 Skill 到 Qoder/Claude、注册 MCP）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install-qoder.ps1
```

安装内容：

- `dist\cua-hub.exe`（约 7MB）
- `~/.qoder/skills/cua-hub/SKILL.md` 与 `~/.claude/skills/cua-hub/SKILL.md`
- `~/.qoder/settings.json` 的 `mcpServers` 增加 `cua-hub` 条目（Qoder 桌面端读取此文件，
  原文件自动备份）；`~/.qoder/mcp.json` 同步一份给 Qoder CLI

重启 Qoder 或新开一个会话后，Agent 可直接调用 `capture_screen`、
`accessibility_tree`、`set_control_text`、`browser_task` 等工具（工具清单在会话启动/刷新时
注册；若当前对话暂时看不到，新开一个即可）。模型密钥放在用户环境变量
（`OPENROUTER_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`），子进程会继承。

细节见 [integrations/README.md](integrations/README.md)。

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
仓库内各版本对照见 [docs/VERSIONS.md](docs/VERSIONS.md)，上游升级流程见
[docs/UPGRADING.md](docs/UPGRADING.md)，工业软件适配见
[docs/INDUSTRIAL.md](docs/INDUSTRIAL.md)。

## 许可证

MIT
