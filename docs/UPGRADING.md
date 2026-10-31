# 上游升级手册

cua-hub 的竞争力来自"集成"，所以必须把上游变化变成可发现、可评估、可回滚的流程，而不是靠记忆。

## 三层机制

| 层 | 机制 | 频率 | 负责内容 |
|----|------|------|----------|
| 依赖 | `.github/dependabot.yml` | 每周（Cargo）/ 每月（Actions） | xcap、enigo、zbus、axum、tokio… 的补丁/小版本，CI 自动验证 |
| 上游产品 | `.github/workflows/upstream-watch.yml` + `scripts/upstream_watch.mjs` | 每周一 09:00（北京时间） | UI-TARS、Cua、Browser Use、Skyvern、Agent-S、Anthropic SDK、MCP 规范的 release/tag |
| 适配层 | 本文档的手工流程 | 按 issue 触发 | 只改 adapter/常量，不动核心 trait；跑冒烟 + CI |

巡检发现新版本会创建或更新带 `upstream` 标签的 issue；没有变化则不打扰。

## 监测项与代码边界

| 上游 | 影响到 | 代码位置 | 升级方式 |
|------|--------|----------|----------|
| MCP 规范 | 协议版本、initialize、工具 schema | `crates/cua-cli/src/mcp.rs` 的 `PROTOCOL_VERSION` | 对照 changelog 改协议常量与字段；跑 `mcp_smoke` |
| Anthropic Computer Use | tool schema、beta header、动作名 | `crates/cua-engine-vision/src/lib.rs` 的 `COMPUTER_BETA` 与 `parse_tool_input` | 更新 beta 常量、补动作映射；用 Anthropic key 实测 |
| OpenAI 兼容网关 | chat completions、图片格式 | `cua-engine-vision/src/openai.rs`、`cua-engine-code`、`cua-engine-browser/agent.rs` | 通常无需改代码，模型名通过 env/参数覆盖 |
| UI-TARS | 本地视觉模型 | 尚无适配（Roadmap） | 新增 provider，实现 `Engine`，不动 CLI |
| Cua | VM/沙箱思路 | `cua-io` 环境层（Roadmap） | 参考其沙箱接口，新增 `cua-env` 实现 |
| Browser Use / Skyvern | 浏览器策略 | `cua-engine-browser` | 只借鉴动作/等待策略，不引入 Python 依赖 |
| Agent-S / Self-Operating Computer | 桌面策略 | `cua-engine-vision` 提示词 | 对比任务成功率后调 prompt，不照搬代码 |
| xcap / enigo | 截图/输入 | `cua-io` | Dependabot PR；Linux 注意 pipewire 头文件版本（CI 已锁 Ubuntu 24.04） |
| zbus | AT-SPI | `cua-engine-access/atspi.rs` | Dependabot PR；运行时仍需 Linux 桌面验证 |
| CDP | 浏览器协议 | `cua-engine-browser/cdp.rs` | Chrome/Edge 升级后跑 `browser_smoke`，重点看 AX 树与 tab 接口 |

## 采纳一次上游变化的步骤

1. **看 issue**：确认版本、release notes、是否涉及破坏性变更。
2. **开分支**：`upstream/<name>-<version>`。
3. **只改适配层**：优先改常量、parser、endpoint；核心 `Action`/`Engine` 不动。
4. **跑本地冒烟**：
   ```bash
   node scripts/mcp_smoke.mjs
   node scripts/mcp_elicit_test.mjs
   cargo run -p cua-engine-browser --example browser_smoke
   cargo test --workspace
   ```
5. **更新基线**：改 `integrations/upstreams.json` 的版本，保留 `checked_at`。
6. **合并 + 发布**：CI 双平台绿后合并；按发布节奏升版本、写 CHANGELOG、打 tag。

## 已完成的升级演练

2026-10-21 用巡检脚本做了第一次真实演练：

- **reqwest 0.12.28 → 0.13.5（已采纳）**：0.13 把 TLS feature 从 `rustls-tls` 改名为
  `rustls`，workspace 依赖改为 `features = ["json", "rustls"]` 后编译、测试通过。
  这属于"改配置即可"的升级，没有动业务代码。
- **zbus 5.19 → 6.0（暂缓）**：6.0 移除了 `blocking-api`，而 `atspi.rs` 目前依赖阻塞式
  调用。升级需要把整个 AT-SPI 适配层改成 async（或在独立线程里跑 async runtime），
  属于结构改动而不是版本号改动。已在 `dependabot.yml` 里忽略 zbus 主版本，等 AT-SPI
  异步化完成后再放开；期间巡检 issue 仍会提示有新版本。
- **tokio 1.54.0 → 1.54.1（已采纳，2026-10-31）**：单条 timer revert 修复
  （[tokio#8692](https://github.com/tokio-rs/tokio/pull/8692)）。workspace 声明是宽松的
  `version = "1"`，升级只动 `Cargo.lock`（+10/-10，与 dependabot PR #46 的 diff 逐字节一致），
  无业务代码变更。fmt / clippy / test 门禁全绿。吸收自 PR #46——其 CI 因 GitHub 账户
  billing 未能启动（job 未运行即失败），按本地三件套门禁替代验证后在 main 直接吸收、
  PR 作废（#22→#24 的先例）。
- **结论**：能过 CI 的立即采纳，过不了的记录原因并挡住自动 PR，不让"有更新"变成噪音。

## 模型/服务端的突发变化

- 模型下线或改名：用 `CUA_HUB_MODEL` / `--model` 覆盖，不改代码；把新默认值写进配置。
- API 字段变化：provider 是隔离的，只改对应 adapter，并用 `dry-run` 先验证解析。
- 浏览器升级导致 selector/AX 变化：`browser_smoke` 会最先失败；必要时退回 `eval` 路径。

## 增加一个监测源

编辑 `integrations/upstreams.json`：

```json
{ "repo": "owner/name", "tag": "v1.2.3", "affects": "说明影响面" }
```

或 crates 数组里加 `{ "name": "crate", "version": "x.y.z", "affects": "..." }`。下一次周巡检自动纳入。
