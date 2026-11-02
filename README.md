# cua-hub

<p align="center">
  <strong>面向智能体（Agent）的多引擎计算机操控系统（Computer Use Operating System）</strong>
</p>

<p align="center">
  <a href="https://github.com/jsshwqz/cua-hub/releases/latest"><img src="https://img.shields.io/badge/release-v0.1.7-blue.svg" alt="Latest Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-green.svg" alt="License"></a>
  <a href=".github/workflows/ci.yml"><img src="https://img.shields.io/badge/CI-passing-brightgreen.svg" alt="CI Status"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux-lightgrey.svg" alt="Platform Support">
  <img src="https://img.shields.io/badge/Rust-1.85%2B-orange.svg" alt="Rust Version">
  <img src="https://img.shields.io/badge/protocol-MCP%20Enabled-purple.svg" alt="Model Context Protocol">
</p>

---

## 核心理念

> **不做新的 Agent 基础模型，做面向 Agent 的计算机操控“操作系统”。**

大模型缺乏稳定的计算机交互原语：纯视觉常有幻觉坐标、无障碍树常遇无名控件、网页自动化容易被 iframe 遮蔽、命令执行容易遭遇未授权越权。

`cua-hub` 提供统一的编排层与底层操作系统抽象：同一个自然语言或自动化入口，由系统在**视觉（Vision）**、**无障碍树（Access）**、**浏览器（Browser）**、**确定性脚本（Code）**与**结构化感知（Perception）**之间智能调度与互补协同。对外提供标准 **Model Context Protocol (MCP)** 服务与统一命令行界面。

---

## 系统架构

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            接入层 (Presentation Layer)                      │
│      CLI (`cua-hub`)    │    Web Console (`:8787`)    │    MCP Server (Stdio/HTTP)  │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │
┌──────────────────────────────────────▼──────────────────────────────────────┐
│                       跨引擎调度与策略网闸 (`cua-router`)                    │
│   · 意图规则路由 (Task Shape Scoring)        · 跨引擎变量流水线 (Orchestrator)     │
│   · 故障降级与重规划 (Retry & Re-plan)       · 三级风险确认门禁 (Risk Gate)        │
└──────┬───────────────────┬──────────────────────┬────────────────────┬──────┘
       │                   │                      │                    │
┌──────▼──────┐     ┌──────▼──────┐        ┌──────▼──────┐      ┌──────▼──────┐
│ 视觉驱动引擎 │     │  浏览器引擎  │        │ 系统代码引擎 │      │  无障碍引擎  │
│ cua-engine- │     │ cua-engine- │        │ cua-engine- │      │ cua-engine- │
│   vision    │     │   browser   │        │    code     │      │   access    │
│ (Anthropic/ │     │ (CDP协议/   │        │ (PowerShell/│      │ (Win UIA /  │
│  OpenAI/    │     │ 隔离环境/   │        │  Bash快速   │      │ Linux AT-SPI│
│  本地 VLM)  │     │ 回读裁决)   │        │  脚本执行)  │      │ 深度快照)   │
└──────┬──────┘     └──────┬──────┘        └──────┬──────┘      └──────┬──────┘
       │                   │                      │                    │
┌──────┴───────────────────┴──────────────────────┴────────────────────┴──────┐
│                      视觉解析与结构化感知 (`cua-engine-perception`)          │
│   · Set-of-Mark 标记渲染     · 包含感知 IoU 几何去重     · 跨引擎读动编号归一体系 │
│   · 多后端 OCR (Media.Ocr / Tesseract)        · 树与视觉双源统一注册表        │
└──────────────────────────────────────┬──────────────────────────────────────┘
                                       │
┌──────────────────────────────────────▼──────────────────────────────────────┐
│                        基础运行与平台底层 (`cua-core` / `cua-io`)             │
│   · 统一 Action/Observation 模型              · 窗口全景/被遮挡捕获 (WGC/PrintWin)   │
│   · 进程级互斥输入注入 (Enigo)                · 物理卡键对账与防泄漏扫场 (Hold/Sweep)│
│   · 屏幕物理坐标与 DPI 自动折算               · 常驻剪贴板上下文管理 (Arboard)       │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 核心引擎矩阵

| 引擎 | 触发与路由条件 | 技术底座 | 优势与核心价值 |
| :--- | :--- | :--- | :--- |
| **Vision 视觉引擎** | 默认路由 / `--engine vision` | Anthropic Computer Use / OpenAI 兼容网关 / 本地 VLM | 适应未知 GUI 界面、跨应用异构桌面交互；自动完成 125%/150% 等系统 DPI 物理坐标转换。 |
| **Browser 浏览器引擎** | 任务含 URL、网页关键词，或 `browser` 命令 | Chromium DevTools Protocol (CDP) 直连 | 独立隔离环境运行；支持内联滚动、同源 iframe 深度穿透、元素自动可见性对齐与回读校验。 |
| **Code 脚本引擎** | 任务含命令、脚本、诊断、系统状态等关键词 | 原生 PowerShell / Bash 沙箱执行 | 比视觉截屏快两个数量级；适合高确定性数据检索、批量运维与文件处理。 |
| **Access 无障碍引擎** | 任务含无障碍、控件名、AutomationId 等关键词 | Windows UIA / Linux AT-SPI2 协议桥 | 毫秒级语义定位；提供 `target` 精确身份寻址，拒绝模糊推测；支持次级动作调用。 |
| **Perception 感知引擎** | `ui_elements: true` 时自动协同 | 内置点阵 SoM 渲染 + 系统 OCR + IoU 空间合并 | 结合屏幕像素与无障碍树，将非语义图像转化为结构化编号注册表，大模型只决策编号、永不直触绝对坐标。 |

---

## 平台支持矩阵

| 操作系统 | 架构 | 验证状态 | 说明 |
| :--- | :--- | :--- | :--- |
| **Windows** | x86_64 | ✅ 生产真机验证 | 支持 Win10 / Win11，支持 Per-Monitor V2 DPI 缩放与 Windows.Graphics.Capture |
| **Linux** | x86_64 | ✅ 生产真机验证 | 深度适配 Ubuntu 24.04 / Debian 13，原生 AT-SPI2 D-Bus 通信 |
| **macOS** | x86_64 (Intel) | ⚠️ CI 交叉编译构建 | 社区实验性支持，欢迎反馈实测结果 |
| **macOS** | aarch64 (Apple Silicon) | ⚠️ CI 交叉编译构建 | 社区实验性支持，欢迎反馈实测结果 |

*注：GitHub Releases 页面提供全平台预编译二进制包。*

---

## 快速上手

### 1. 构建与环境自检

```bash
# 检出源码并构建优化二进制
cargo build --release

# 运行系统诊断：屏幕适配、DPI、键鼠注入权限与模型网关连通性
./target/release/cua-hub doctor
```

### 2. 命令行任务执行

```bash
# 方式 A：Anthropic 原生 Computer Use 协议
export ANTHROPIC_API_KEY=sk-ant-api03-...
./target/release/cua-hub run "打开记事本并输入系统状态汇总" --yes

# 方式 B：任意 OpenAI 兼容模型（OpenRouter / vLLM / Ollama）
export OPENROUTER_API_KEY=sk-or-v1-...
./target/release/cua-hub run "整理桌面临时文件" --yes \
  --model "minimax/minimax-m3.1-flash:free"

# 方式 C：浏览器专属引擎执行（支持有头与无头模式）
./target/release/cua-hub browser "打开控制台并提取数据指标" \
  --start-url "https://news.ycombinator.com" --headed

# 方式 D：跨引擎流水线编排（浏览器提取 -> 脚本写入 -> 桌面交互）
./target/release/cua-hub orchestrate \
  "从目标网页抓取最新发布信息，生成为本地文件并弹窗提醒"
```

### 3. 运行本地 Web 交互控制台

```bash
./target/release/cua-hub console --bind 127.0.0.1:8787
```

- 浏览器访问 `http://127.0.0.1:8787` 查看实时桌面推流（带分辨率、DPI、光标位置）。
- 在线任务分发、步进监控、动作审计日志与实时急停熔断。
- 相同进程直接开放 `/mcp` 接口，支持混合多任务协同。

---

## MCP 协议全面集成

支持作为标准 MCP 服务挂载至 **Claude Desktop**、**Cursor**、**Qoder** 等各类 AI 宿主：

```json
{
  "mcpServers": {
    "cua-hub": {
      "command": "C:\\path\\to\\cua-hub.exe",
      "args": ["serve"],
      "env": {
        "OPENROUTER_API_KEY": "sk-or-..."
      }
    }
  }
}
```

系统对外暴露 **38 个全功能原子工具与编排原语**，按交互层次划分如下：

### 1. 智能规划与跨引擎编排
- `run_task`：阻塞式自然语言任务执行，可启用 `ui_elements` 携带结构化注册表。
- `start_task` / `task_status` / `task_cancel`：非阻塞后台长任务管理，支持进度轮询与即时终止。
- `browser_task`：隔离 Chromium 网页专属任务执行，具备回读裁决、自动滚动定位与光标指示。
- `orchestrate`：跨引擎多阶段流水线计划生成与分步执行，支持步骤间变量级联传递。
- `list_runs` / `replay_run`：任务运行审计流查询与基于 `steps.jsonl` 的确定性回放。

### 2. 屏幕捕获与环境感知
- `capture_screen`：高保真截取主屏幕（返回 JPEG 字节流与物理尺寸）。
- `capture_window`：捕获指定窗口独立表面（被遮蔽亦完整，自动适配硬件加速与 WGC 捕获）。
- `wait_for_change`：基于平均哈希差分的窗口像素阻断等待，用以可靠验证动态动作着陆。
- `screen_info`：获取当前显示器尺寸、原点、DPI 缩放倍率及实时光标坐标。

### 3. 窗口管理与无障碍观测
- `list_windows`：枚举系统全部顶层窗口的句柄、进程 PID、标题、几何区域与焦点状态。
- `list_apps`：按进程分组汇总应用程序及其拥有的窗口，支持枚举已安装未启动的应用。
- `launch_app`：拉起目标应用程序并持续观测直至其物理主窗口就位。
- `focus_window`：精确将目标窗口置于前台，遇到模糊匹配时强制报错并列出候选。
- `window_state`：**核心观测原语**。单次返回窗口高分辨率画面、完整 UIA/AT-SPI 控件树、稳定 `element_index`、焦点元素及当前选中文本。后续调用默认返回统一差分（`mode=diff`）。
- `accessibility_tree`：获取桌面全局范围的有界无障碍树快照。

### 4. 精准控件定位与属性交互
- `find_control`：依据精确 `target` 身份地址计算控件屏幕区域，多目标冲突时坚决列出候选并中断。
- `invoke_control` / `invoke_element`：调用控件默认行为模式（优先走无障碍模式，失败平滑回退中心物理点击），并提供执行后效果裁决。
- `set_control_text` / `set_element_value`：通过 Value/EditableText 语义直接赋值文本（免模拟键盘），自动回读字段验证真实着陆。
- `control_state`：即时查询控件的开关状态（Toggle）、展开状态（Expand）、范围进度及启用标记。
- `perform_secondary_action`：调用控件声明的高阶次级动作（如 Expand、Collapse、Scroll、Select 等）。

### 5. 底层输入注入与剪贴板
- `window_input`：高可靠原子指令：置顶窗口 -> 窗口内相对坐标点击 -> 文本键盘注入 -> 可选回车；强制校验窗口物理边界，杜绝越界误触。
- `click` / `move` / `drag` / `scroll`：物理层级指针定位、点击、拖拽与横纵滚轮驱动。
- `type_text`：焦点窗口通用 Unicode 文本注入，附带输入前后文本比对裁决（`effect=changed/unchanged/unverifiable`）。
- `key` / `key_down` / `key_up`：按键及复合修饰键（如 Ctrl/Alt/Shift/Win）的按下、保持与释放。
- `mouse_down` / `mouse_up`：鼠标单键长按与释放，支持框选与高级拖拽手势。
- `clipboard_get` / `clipboard_set`：系统剪贴板文本读写，Linux 下保持常驻防丢失。

---

## 核心技术契约与可靠性保证

为消除常见自动化 Agent 的脆弱性与误操作，`cua-hub` 在底层内置了五大硬性保障机制：

### 1. 读动编号归一体系 (Unified Numbering System)
感知注册表展现的控件序号直接绑定底层 `window_state` 的 `element_index`。避免传统架构中“提示词里看到序号 3，动手时却点了序号 5”的映射漂移问题。

### 2. 严格目标身份寻址 (Target Identity Addressing)
通过 `{"name": "...", "role": "Button", "container": "...", "nth": 0}` 精准定位。
- 绝不随机盲猜：若目标命中多个同名控件，系统强制报错并列出所有候选，要求大模型补充容器或层级修饰。
- 寻址与执行锁定同一底层对象句柄，绝不在解析后二次按名模糊查找。

### 3. 动作效果回读裁决 (Action Effect Verification)
点击、输入与模式调用后，自动回读目标控件状态或比对屏幕局部哈希：
- `changed`：确认目标状态（如 Checkbox 翻转、文本更新）真实落地。
- `unchanged`：命令虽然成功发送，但被控界面未产生响应，及时警告并中断重试。
- `unverifiable`：不可读状态控件，如实提示模型通过画面观测核实，不伪造虚假成功。

### 4. 物理按键状态泄漏双重防御 (Dual-Layer Key Hold Protection)
防止 Agent 在任务异常中断时留下按键卡死（如 Shift/Ctrl 长期按住导致后续全盘错乱）：
- **第一层（意图登记）**：注入链严格跟踪每一笔保持请求。
- **第二层（OS 物理扫描对账）**：通过底层系统 API（如 `GetAsyncKeyState`）轮询物理按键真实状态，脱节时自动触发清理程序批量释放非受控按键。

### 5. 三级风险网闸与二次确认 (Three-Tier Risk Confirmation Gate)
命令根据其语义和参数形态自动定级，避免“未在黑名单即默认放行”的设计缺陷：
- **只读级（Read-Only）**：`ls`、`cat`、`Get-Content` 等，静默放行。
- **常规级（Medium Risk，系统默认）**：涉及文件变动、脚本执行、联网请求等，需显式携带 `--yes` 或客户端授权。
- **高危级（High Risk）**：系统破坏性命令（如 `rm -rf`、`format`、注册表改动、关机），必须显式携带 `--allow-high` 或通过 MCP Elicitation 双向反问批准；`--yes` 无法逾越此级别。

---

## 环境变量参考

| 环境变量 | 默认值 | 作用说明 |
| :--- | :--- | :--- |
| `ANTHROPIC_API_KEY` | - | Anthropic 视觉引擎专属密钥 |
| `OPENAI_API_KEY` / `OPENROUTER_API_KEY` | - | OpenAI / OpenRouter 网关兼容密钥 |
| `CUA_HUB_OPENAI_BASE_URL` | `https://api.openai.com/v1` | 自定义 OpenAI 兼容网关地址（优先读取 `OPENROUTER_BASE_URL`） |
| `CUA_HUB_MODEL` | `gpt-4o-mini` | 视觉与编排引擎默认选用模型标识 |
| `CUA_HUB_BROWSER_MAX_TOKENS` | `700` | 浏览器引擎单步规划输出 Token 上限 |
| `CUA_HUB_BROWSER_INDICATE_MOUSE` | `true` | 网页动作执行前系统光标跟随指示（ headed 模式生效，停留约 300ms） |
| `CUA_HUB_OCR_BACKEND` | `auto` | OCR 引擎选择：`auto` (Win 用系统组件 / Linux 用 Tesseract) / `windows` / `tesseract` / `none` |
| `CUA_HUB_OCR_LANG` | `eng` | Tesseract 引擎识别语言包（多语言可用 `+` 连接，如 `eng+chi_sim`） |
| `CUA_HUB_MCP_TOKEN` | - | HTTP 模式接入强制 Bearer 鉴权 Token |
| `CUA_HUB_ALLOW_REMOTE` | `0` | 是否允许无 Token 监听非回环网络（仅用于前置反向代理鉴权场景） |
| `CUA_HUB_YES` | `0` | 设置为 `1` 全局放行常规中危操作二次确认 |

---

## 文档中心 (Documentation Hub)

关于系统底层机制与具体工程实践，请查阅完整技术白皮书：

- [系统核心架构设计 (`docs/DESIGN.md`)](docs/DESIGN.md)：坐标体系换算、多引擎数据流、无障碍总线与底层协议设计。
- [技术演进路线图 (`docs/ROADMAP.md`)](docs/ROADMAP.md)：已完成特性、进行中模块及中远期规划。
- [上游生态同步指南 (`docs/UPGRADING.md`)](docs/UPGRADING.md)：依赖库、前沿竞品与生态协议更新跟踪准则。
- [工业软件集成手册 (`docs/INDUSTRIAL.md`)](docs/INDUSTRIAL.md)：面向 FreeCAD 等复杂工程软件的规则包与接入模板。
- [多版本架构演进对照 (`docs/VERSIONS.md`)](docs/VERSIONS.md)：历史原型、分支演化与能力整合全景。
- [版本变更日志 (`CHANGELOG.md`)](CHANGELOG.md)：详细发布历史与版本特性记录。
- [最新工程交接备忘 (`docs/HANDOFF-2026-11-02.md`)](docs/HANDOFF-2026-11-02.md)：开发状态、测试基准与环境配置踩坑要诀。

---

## 开源许可证

本项目基于 [MIT License](LICENSE) 开源。
