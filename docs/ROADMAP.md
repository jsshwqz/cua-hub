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
- [x] AT-SPI DoAction/SetTextContents (implemented; live Linux runtime pending)
- [x] Linux AT-SPI2 tree + find (verified in WSL with a GTK3 window)
- [ ] Linux AT-SPI action invocation (DoAction) and full CLI build on Linux
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
- [x] `then:[…]` 服务端动作链：一次调用按序跑确定性图元动作，只回一份逐步 trace
  加一次观测；步骤继承 `observation_id`/`window`，失败即停在 `stopped_at`，
  模型驱动工具与嵌套 `then` 一律拒绝（本机实测 5 次调用 → 2 次，字节省 48%）
- [x] Linux 窗口级 AT-SPI 树的真机验证（2026-10-23 完成：InstaBox 云机 Debian 13 + Xfce 真图形会话，tree 返回 119→151 节点含真实应用窗口，find 按名定位准确；详见 CHANGELOG 的 Unreleased 验证记录）
- [ ] 观测全面切到 WGC（当前 PrintWindow 为主、WGC 为兜底）
- [ ] 连续感知层（帧流 / 变化检测 / ROI / 本地 CV-OCR / 稳定 ID 场景图）——用户明确"暂不做"
