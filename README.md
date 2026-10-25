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
| `cua-hub browser "<task>" --start-url <url>` | 浏览器任务（默认 headless）。确认门与桌面引擎同一套：`--yes` 放行中危、`--allow-high` 才放行 `eval`；`--dry-run` 只规划并打印档位 |
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
| `browser_task` | 在隔离 Chromium 里执行网页任务（CDP）。每一步都过确认门：`eval` 一律高危（要 `allow_high`），跨站 `navigate`、指向提交控件的 `click`、任何 `press Enter` 是中危（要 `yes`），其余低危直接跑；`dry_run` 会打印每步档位。默认 `yes:false`、`allow_high:false`。click/type 的答复是**回读裁决**：`clicked <sel> [effect=changed] (checked Some(false)->Some(true))`、`typed N chars into <sel> [verified: value reads as sent]`；富文本走 `Input.insertText`（光标落末尾、不覆盖已有内容）并按三条标准回读；读不一致报 `FAILED:`，**这条路径不再出现 `OK`** |
| `orchestrate` | 跨引擎计划与执行，步骤间传值 |
| `list_runs` / `replay_run` | 查看历史运行 / 重放某次运行 |
| `capture_screen` | 返回主屏 JPEG + 尺寸 |
| `list_windows` | 列出可捕获的顶层窗口（标题、应用、坐标、焦点、最小化） |
| `list_apps` | 按进程分组的应用清单（含各自窗口与可启动的 exe 路径）；`include_all` 追加开始菜单里未运行的程序 |
| `launch_app` | 启动应用并等待其窗口出现；已在运行的直接返回现有窗口；`ready=false` 表示超时内没等到窗口 |
| `window_state` | 观测原语：一次返回窗口截图（含菜单/对话框等独立弹层，每张图带自己的 `screenshot_id`/原点）与窗口内 UIA 元素树 + `element_index` + 焦点元素，并给出 `observation_id` 作为作用域令牌。焦点元素只在 Windows 侧读得到；其他平台该字段为 null 并附 `focused_element_note` 说明"这里读不到"，不要当成"没有控件持有焦点"。元素编号在同一窗口内保持稳定，因此第二次起默认返回 unified diff（`mode=diff`），`unchanged` 表示映射仍有效，`baseline_reset` 表示旧编号全部作废。树里同时给出控件当前值、可用动作与选中/焦点标记；`accessibility` 还带 `selected_text`、`document_text`、`selected_elements`（支持 Text 模式的控件）。有应用没应答时 `accessibility` 里会多一个 `unreadable`，写明是哪个对象路径读不到——看到这个键就别把"树里没有"当成"控件不存在" |
| `capture_window` | 抓取窗口自身表面（被遮挡也完整），返回 `origin`/`image_scale`/`source`/`hash`，坐标换算由服务端完成；PrintWindow 渲染成全黑的硬件合成分布式表面会自动改用 Windows.Graphics.Capture（`source=GraphicsCapture`），也可用 `CUA_HUB_CAPTURE_BACKEND=wgc` 强制 |
| `wait_for_change` | 阻塞等待窗口或其弹层真的发生变化，用来验证动作是否生效 |
| `perform_secondary_action` | 调用控件除点击之外的无障碍动作：Expand/Collapse/Raise/Scroll Up·Down·Left·Right/Toggle/Select/Focus/Invoke；只接受该控件在树里 `actions=` 列出的名字，猜错返回 `unsupported_action` 与真实清单，执行后回读状态供验证 |
| `focus_window` | 按 id、必须唯一匹配的标题/应用选择器，或 `{app,id}` 把窗口带到前台；0 个或多个匹配时返回候选，不猜 |
| `window_input` | 原子操作：聚焦窗口 → 可选窗口相对坐标点击 → 输入文本 → 可选回车；窗口目标必须唯一，失败返回候选。`x`/`y` 必须落在该窗口自己的矩形内，越界返回 `offset_outside_window`（这个工具不会点到窗口外），只给一个轴也拒绝。窗口激活只在 Windows 实现，Linux 上这条工具回 `unsupported` + `retry:false`（不拿可重试的错误糊过去） |
| `accessibility_tree` | 带编号的桌面级 UIA 树（窗口/按钮/输入框 + 矩形）；日常观测请用 `window_state`。Linux 上如果某些应用没应答，树尾会列出 `! parts of this tree were not read:`（最多点名 6 个子树，其余给条数）——少节点不再无声 |
| `find_control` / `invoke_control` | 按 `target` 身份地址定位（`name` 简写仍可用）；invoke 优先走 Invoke 模式，失败再点中心。一个 target 命中多个控件时**报错并列出候选**，绝不替调用方挑一个。`invoke_control` 可选 `max_nodes`/`max_depth`，失败要按原话读：`did not reach` = 搜索被节点预算截断（提高 `max_nodes` 再来），`did not get an answer` = 那个应用没回话（Linux 单次调用最多等 1200ms），两者都**不**表示控件没有该模式；只有走到控件后的 `None` 才是"没有该模式"。三种都不该顺手降级成坐标点击。成功的答案还带**效果裁决** `effect=changed`/`unchanged`/`unverifiable`（回读控件自己的状态得出；普通按钮没有可读状态位，所以是 `unverifiable` 而不是"没变"——要看画面或树自己确认） |
| `invoke_element` | 按 `accessibility_tree` 的编号调用控件（模式同 invoke_control） |
| `set_control_text` / `set_element_value` | 用 Value/EditableText 模式按名称/编号直接设值（不模拟键盘）。`set_control_text` 同样接受可选 `max_nodes`/`max_depth`，读失败口径同 `invoke_control`：`did not reach` = 预算不够，`did not get an answer` = 那个应用没回话，两者都不是"控件不支持"。效果裁决按"字段里是不是我要的那段文字"判：Windows 读得回来所以能报 `changed`/`unchanged`；Linux 还没接 Text 接口，诚实报 `unverifiable`，不拿"模式回了 true"冒充"写进去了" |
| `control_state` | 读取启用/可见、勾选、展开状态、范围值。两平台都实现：Windows 走 UIA 的 Toggle/ExpandCollapse/Value/RangeValue 模式，Linux 走 AT-SPI 的 `GetState` 位图（`toggle` = `on`/`off`/`indeterminate`，`expanded` = `expanded`/`collapsed`/`partially_expanded`/`leaf`，口径一致）。Linux 侧 `value` 与 `range_value` 暂不填——那两个在 Text / Value 接口后面，还没接 |
| `click` / `move` / `drag` | 左/右/中键点击（可双击）、移动指针、左键拖拽；`click` 支持 `observation_id`+`element_index`，或 `observation_id`+`screenshot_id`+`x/y`（弹层图片的坐标系与主窗口不同，必须指名） |
| `scroll` | 在指定坐标滚轮（dx/dy，支持横向） |
| `type_text` | 向焦点窗口输入任意 Unicode 文本（自动处理 Shift 大写/括号） |
| `key` | 发送按键组合（PageUp/PageDown/方向键/系统快捷键等） |
| `mouse_down` / `mouse_up` | 按住/释放鼠标键（Shift 多选、Ctrl 拖拽） |
| `key_down` / `key_up` | 按住/释放按键或组合键 |
| `clipboard_get` / `clipboard_set` | 剪贴板文本读写（常驻实例） |
| `screen_info` | 主屏尺寸、原点、DPI、光标位置 |

控件寻址用 `target`：`{"name": "...", "role": "Button", "nth": 0, "container": "..."}`。
`name` 是不区分大小写的子串，也可写 `re:` 正则或 `id:<AutomationId>` 形式（用于名称动态或
为空的控件，例如 Win32 编辑框 `id:15`、计算器按键 `id:num1Button`、结果显示
`id:CalculatorResults`）；`role` 是控件类型，`container` 只匹配位于同名容器内的控件，
`nth` 是多个命中里的第几个（0 起）。**一个 target 命中多个控件时一律报错并列出候选，
绝不由引擎挑第一个**；要选就由调用方用 role/container/nth 说明白。旧的字符串 `name`
参数是 `{"target":{"name":...}}` 的简写，仍然可用；同时给 `target` 和 `name` 会被拒绝
（双重地址）。

解析和动手对着同一个节点：Linux 上身份是 `(bus name, object path)`，Windows 上用
RawViewWalker 走到哪个元素就对哪个元素动手，解析之后不再按名二次查找。代价是 Windows
动作从"按名 FindFirst"变成每次走一遍树：本机约 2300 个可见节点的桌面上一次 `find_control`
约 15s（同等节点的 control-view 快照也要 14s，慢在树的大小，不在 walker）。

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

## 确认与风险分级

命令按形状分成三档，**中间那档是默认**——不是"名单里没有就放行"：

| 级别 | 判定 | 默认行为 |
|------|------|----------|
| 只读 | `ls` `cat` `git status` `Get-Content` 这类只读状态的命令 | 直接执行，不打扰 |
| 未列名 | 其余一切：会写文件、联网、构建、跑脚本 | 需要 `yes=true`，或由能应答的客户端确认（stdio 走 MCP elicitation） |
| 高危 | 递归或强制删除、`rd /s /q`、`Remove-Item -Recurse`/`-Force`、下载直接管进 shell、`-EncodedCommand`、`reg delete HKLM…`、磁盘与关机类 | 需要 `allow_high=true` 或人工批准；`yes` 买不到这一档 |

- 判定读的是**参数**而不是字面名单：`rm  -rf  /`、`Remove-Item -Force -Recurse C:\`、
  `rd /s /q D:\data` 与拼写完整的同一条命令同级；`sudo` / `cmd /c` / `sh -c "…"` 这类包装会被剥开再判。
- 也不误伤散文：`printf "format %s"` 与 `please format the document` 都不算高危，
  因为只会误伤的"提到就拦"会把人训练成无脑放行。
- **入口默认**：MCP 的 `run_task` / `start_task` / `orchestrate` 默认 `yes=false`
  （0.1.4 起，此前是 `true`；`orchestrate` 此前在 MCP 与 CLI 两条路上都硬编码跳过确认）。
  CLI 用 `--yes` 或 `CUA_HUB_YES=1` 显式放开中危，高危始终需要 `--allow-high`。
  默认必须是"问"，因为这些命令是大模型写的。

## 环境变量

| 变量 | 说明 |
|------|------|
| `ANTHROPIC_API_KEY` | Anthropic 视觉引擎密钥 |
| `ANTHROPIC_BASE_URL` | 可选，自定义 Anthropic 网关 |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` | OpenAI 兼容引擎密钥 |
| `CUA_HUB_OPENAI_BASE_URL` | 默认取 `OPENROUTER_BASE_URL`，否则 `https://api.openai.com/v1` |
| `CUA_HUB_MODEL` | Anthropic 默认 `claude-sonnet-4-5`；OpenAI 默认取 `OPENROUTER_MODEL` 或 `gpt-4o-mini` |
| `CUA_HUB_YES` | 设为 1 跳过**中危**确认（CLI 入口）；高危动作始终还要 `--allow-high` |
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
