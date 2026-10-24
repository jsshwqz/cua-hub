# Roadmap

## Phase 1 — Vision MVP (current)

- [x] Workspace, CI on Windows + Linux
- [x] Cross-platform capture (xcap) and input (enigo)
- [x] Anthropic Computer Use loop with official tool protocol
- [x] Rule router, risk gate, dry-run, JSONL run log
- [x] OpenAI-compatible vision provider (OpenRouter / OpenAI / Ollama / vLLM)
- [x] First end-to-end task on Windows (Start menu, visually verified)
- [x] DPI-aware coordinate mapping (model -> physical -> logical input)
- [ ] First end-to-end task on Linux
- [ ] Model adapter trait extracted from the Anthropic provider

## Phase 2 — Multi-engine

- [x] `cua-engine-code`: LLM-generated PowerShell/sh with risk-gated execution
- [x] `cua-engine-access`: Windows UIA snapshot (cached, depth-bounded)
- [x] Hybrid context: `--ui-tree` adds the tree to vision prompts
- [x] `cua-engine-access`: find/invoke controls by name or AutomationId
- [x] UIA Invoke/Value patterns and control state (Windows, verified)
- [x] AT-SPI DoAction/SetTextContents (2026-10-23 真机验证：CI 图形会话里 `invoke_control`
  触发按钮、`set_control_text` 写入 entry，probe 应用自己写回的文本证实两条路径都落地)
- [x] Linux AT-SPI2 tree + find (verified in WSL with a GTK3 window)
- [x] Linux AT-SPI action invocation (DoAction) and full CLI build on Linux
  （DoAction 见上；workspace 在 ubuntu-24.04 上 fmt/clippy/test/release build 全绿）
- [ ] UI-TARS via Ollama as a local vision backend
- [ ] Router scoring: route by app, task shape, and engine health
- [ ] Engine fallback on repeated failure

## Phase 3 — Browser and orchestration

- [x] `cua-engine-browser` over CDP (navigate/click/type/eval, isolated profile)
- [x] Router delegates URL tasks to the browser engine
- [x] Cross-engine plans (planner → browser/code/vision, `{{var}}` hand-off)
- [x] Failure recovery: same-engine retry with budget headroom, then re-plan
- [ ] Persistent task state, resume after crash
- [x] MCP server over stdio exposing run_task / capture / tree / find / invoke
- [x] MCP over HTTP (loopback-only by default)
- [x] Async background jobs (start_task / task_status / task_cancel)
- [x] MCP elicitation for high-risk actions over stdio (accept/decline verified)
- [ ] HTTP-side confirmation channel

## Phase 4 — Product

- [x] Web console: live screen preview, start/cancel jobs, hosts /mcp
- [ ] Streaming video and operator takeover
- [x] Run history and deterministic replay (`runs` / `replay`, MCP tools)
- [ ] Fine-tune pipeline for a local model (replace API calls for hot paths)
- [ ] SDK for Rust, Python, TypeScript

## Phase 4 — 观测契约与操控可靠性（2026-10-23，已发布 0.1.3）

- [x] 遮挡窗口整窗捕获：`PrintWindow(PW_RENDERFULLCONTENT)` + 物理矩形 + `source`/`hash` 自证
- [x] 坐标空间统一：进程启动即 Per-Monitor V2 DPI 感知，UIA 矩形 / 截图 / 输入注入同一物理像素
- [x] 点击时序：move → 稳定 → press → 保持 → release
- [x] `window_state` 观测原语：截图 + 窗口内元素树 + `observation_id` 作用域令牌
- [x] 稳定元素编号（结构签名）+ 无障碍树 unified diff + `baseline_reset` 语义
- [x] 弹层元素树并入同一次观测（`# related window` 小节、覆盖去重、节点记录所属窗口）
- [x] 每张图自带 `screenshot_id` 与原点，弹层坐标由服务端换算
- [x] `perform_secondary_action`（Expand/Collapse/Raise/Scroll/Toggle/Select/Focus/Invoke + 回读状态）
- [x] `list_apps` / `launch_app`（开始菜单发现、等待窗口、`matched_by` 三级识别）
- [x] `selected_text` / `document_text` / `selected_elements`（Text 模式多候选探测）
- [x] Windows.Graphics.Capture 后端（PrintWindow 全黑时降级；`CUA_HUB_CAPTURE_BACKEND=wgc` 强制）
- [x] 无障碍树深度修复：按层 `FindAllBuildCache`（`TreeScope_Children` 会把树压成一层）
- [x] 控件状态改读缓存属性（`GetCachedPatternAs` 在真实 provider 上取不到选中/值）
- [x] 安全策略对齐四档确认模型（交还用户 / 动作时确认 / 点名预批准 / 无需确认）
- [x] 确认链闭合（issue #8，0.1.4 之后）：命令分级改为"只读免打扰 / 未列名需决定 / 高危要明示许可"，
  MCP 的 `run_task`、`start_task`、`orchestrate` 不再默认 `yes: true`，编排路径接上确认通道；
  绕过例子（`rd /s /q`、`Remove-Item -Force -Recurse`、双空格、`curl … | bash`、`-EncodedCommand`）
  全部成为测试，并用旧二进制做过逐格对照
- [ ] 风险分级仍基于形状而非意图：`allow_high` 是一把大开关，一旦给出就不区分具体命令；
  下一步可做成按命令前缀的点名预批准（对齐四档模型里的"点名预批准"那一档）
- [x] `then:[…]` 服务端动作链：一次调用按序跑确定性图元动作，只回一份逐步 trace
  加一次观测；步骤继承 `observation_id`/`window`，失败即停在 `stopped_at`，
  模型驱动工具与嵌套 `then` 一律拒绝（本机实测 5 次调用 → 2 次，字节省 55%）
- [x] Linux 桌面级 AT-SPI 树的真机验证（2026-10-23，InstaBox 云机 Debian 13 + Xfce；
  `cua-hub tree` 119→151 节点、`find` 按名定位准确。注意口径：那是**桌面级** `snapshot()`，
  不是窗口级；窗口级与动作路径见下一条）
- [x] Linux 窗口级 `snapshot_window` + `DoAction` + `SetTextContents` 真机验证
  —— 2026-10-23 由 `linux-atspi.yml` 结项：Xvfb + openbox + at-spi2-core + GTK3 probe 窗口，
  MCP 服务在作业里真跑。断言含窗口级树 6 个节点、三个命名控件都在树内、帧内标签证明
  这不是桌面级整树、截图回来了一张、`set_control_text` 写入 18 字符、`invoke_control`
  触发按钮，最后由 probe 应用自己写回的文件确认文本真的进了控件。过程中修掉一个真 bug：
  `SetTextContents` 的回包是 bool，解码成 `()` 会让"应用已经写入"被报成"控件没有该模式"。
  issue #16 同期解决：`window_state` 先按**进程号**认窗口——AT-SPI 接口本身不报 pid，
  但每个应用挂在一条总线连接上，`GetConnectionUnixProcessID` 能把总线名换成与窗口列表
  同一个 pid，因此 WM_CLASS 与无障碍应用名不一致（Python 脚本、Electron 一类）也能命中，
  名字匹配退为兜底。CI 用 `PROBE_IDENTITY=mismatch` 造出两边名字确实不同的应用并断言
  树仍然完整（run `39915558784`，0 failed）。
- [x] 观测主序是否全面切 WGC：**结论是不切**，保留 PrintWindow 为主、WGC 兜底。
  同一轮交替实测（release 二进制，`capture_window` max_edge 1280）里 WGC 每次都慢
  2.1~5.8 倍（记事本 147→858ms、Chrome 628→1338ms、微信 211→912ms），
  而本机五个窗口（含三个 Chromium 合成窗口）PrintWindow 全都不空白，兜底从未触发。
  慢不在 WGC 本身，而在我们的实现：当前每次捕获都新建 D3D device + frame pool +
  session，约 0.7~1.2 秒固定开销。真要切主序，先做会话复用再谈。
  配套改动：WGC 拒绝时把**失败阶段**报出来（观测里新增 `backend_error`），
  以前 `capture_or_error` 写明了阶段却被调用方丢成 `None`，等于没有诊断。
- [x] 浏览器进程与 profile 目录不再残留（issue #10）：`launch` 在 spawn 之后失败原本
  直接返回，`Child` 还没进 `BrowserSession`，清理逻辑根本跑不到；现在由 `PendingBrowser`
  守卫接管——直到 session 真的建起来，进程与目录的所有权都在它手上，落不回就杀+删。
  顺带关掉两个连带问题：profile 目录名从 `pid-port`（可预测、且 `create_dir_all`
  会**复用**上一次泄漏的目录，lib.rs 号称的 isolated profile 在失败路径上不成立）
  改为 pid+进程内计数+亚秒时钟并用 `create_dir` 创建（已存在就换名重试）。
  **真机量出来还有一层**：`Child::kill()` 杀的是我们 spawn 的那个进程，而 Edge 会
  re-exec 出真正的浏览器再让原进程退出——本机一次正常收尾的 smoke 之后仍剩
  8 个 msedge 进程和 717 个文件的目录，`remove_dir_all` 因目录被占用而失败、
  又被 `let _` 吞掉。收尾改为按 `--user-data-dir` 反查仍在用该 profile 的浏览器进程
  并杀掉，再带重试删目录；删不掉就 `warn` 说明留在临时目录，不再假装成功。
  验证：本机三向负控——把守卫改成空实现，2 条测试立刻红（进程仍活 / 目录仍留）；把
  `claim_profile_dir` 退回 `create_dir_all`，`a_stale_profile_is_never_adopted` 红
  （"an occupied profile directory was handed out again"）；把收尾改回"只 kill + 删"，
  真机那条 `--ignored` 测试报 "profile survived the session drop"。
  修复后 workspace 执行数 **102 → 109**，真机 smoke 从 8 进程 + 717 文件目录降到 0 + 0。
- [ ] Linux 平台完整性（issue #11）**第一批**已做：`find` 不再在第一个精确命中处早停
  （同名 Label 会压过 Button，而 Label 没有动作）、`app_matches` 不再让空应用名命中任何查询、
  `state()`/`focused_element` 在 Linux 从"静默 `Ok(None)`"改为明确的 `Unsupported`
  （观测里多带 `focused_element_note`）。
  **第 1 项也已做**：Linux 的浏览器发现——候选是相对名而 `exists()` 查的是 CWD 不是 PATH，
  装着 `/usr/bin/chromium` 的机器不设 `CUA_HUB_BROWSER` 就报"no Chrome/Edge found"；
  现在按 PATH 找、返回绝对路径，并把"存在"收紧成"是文件且可执行"。
- [x] Linux 的 `control_state`（issue #11 第 3 项）：接到 `org.a11y.atspi.Accessible.GetState`
  ——无入参、回 `au`（按 `AtspiStateType` 序号排的 u32 位图，32 号以上落在第二个字）。
  字段口径与 Windows 一致；`toggle`/`expanded` 只在控件自称可勾选/可展开时出现，
  没观察到的状态不写成 `off`；`value`/`range_value` 明确留空（在 Text / Value 接口后面）。
  序号表照 at-spi2-core 的 `atspi-constants.h` 数出来：第一份文档给的 VISIBLE/HORIZONTAL
  各差一位，而这种错位不会报错，只会把一个状态读成另一个。probe 因此让 GTK 自己写
  `toggled` 信号当独立 oracle——总线的数字不等于控件的真相。
  **还差**：遍历超时与错误上报、`invoke` 后的效果校验、`invoke`/`set_value` 写死的 600/10 上限。
- [ ] 连续感知层（帧流 / 变化检测 / ROI / 本地 CV-OCR / 稳定 ID 场景图）——用户明确"暂不做"
