# Computer Use Agent 大集成项目 — 规划书

> 起草：2026-10-20｜状态：待用户确认开工｜语言：Rust｜形态：交钥匙工程

## 1. 项目目标

做一个 **Rust 编写的统一 Computer Use Agent 运行时**：像人一样看屏幕、点鼠标、敲键盘，
但"大脑"可插拔——把 Anthropic / OpenAI / Google / 微软 / 亚马逊 / 开源模型
都接成统一接口，一套代码、多个大脑可切换、可横向评测。

**不做的事**：不从零训练视觉模型（没 GPU、没数据、没必要）；不重复造"第 N 个 Python agent 框架"。

## 2. 现实约束（2026-10-20 实测）

| 资源 | 现状 | 结论 |
|---|---|---|
| Muse VM（云端电脑） | 2 核 / 8GB 内存 / 100GB 硬盘，无 GPU，无 Rust 工具链 | 可做开发机+构建机+运行机；**跑不动** UI-TARS 7B 本地推理（需 16GB 显存） |
| XFCE 桌面 + x11vnc | 已建成（restore-remote-access.sh 可恢复） | 执行层可直接对真实桌面操作，天然沙箱 |
| GCP free-vps / Oracle / 阿里云 ECS | 代理/任务机，非本项目主力 | 不用 |
| 本地电脑 | 只做 VNC 查看与最终验收 | 不装开发环境 |

**架构推论**：Rust 做"手"（截屏/键鼠/调度/沙箱），云端 API 做"脑"（视觉定位+规划）。
等以后有 GPU 机器，再把 UI-TARS 这类本地模型接进来——接口预留好，切换只是换一个 provider。

## 3. 总体架构

```
┌─────────────────────────────────────────────────┐
│ cua-orchestrator  任务编排：多步规划/重试/检查点/审计 │
├─────────────────────────────────────────────────┤
│ cua-brain         大脑抽象 trait：observe→plan→act │
│  ├─ provider-anthropic  (Computer Use API)       │
│  ├─ provider-openai     (CUA / Responses API)    │
│  ├─ provider-gemini     (Gemini computer use)   │
│  └─ provider-uitars     (本地/远端推理，预留)     │
├─────────────────────────────────────────────────┤
│ cua-runtime       执行核心（纯 Rust）：            │
│  截屏(xcap) / 键鼠(enigo) / 窗口 / 动作原语       │
│  安全围栏：allowlist / dry-run / 人工确认 / 审计   │
├─────────────────────────────────────────────────┤
│ cua-bench         评测：OSWorld 子集+自建任务集     │
│ cua-web           Web UI：屏幕直播/接管/任务管理   │
└─────────────────────────────────────────────────┘
```

关键 crate 选型：`enigo`（跨平台键鼠注入，RustDesk 也在用）、`xcap`（截屏）、
`tokio`（异步调度）、`axum`（Web UI）、`serde`（动作协议）。

## 4. 仓库结构（workspace）

```
computer-use-agent/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ cua-types/              # 动作/观测/任务的统一数据结构
│  ├─ cua-runtime/            # 截屏+键鼠+窗口+安全围栏
│  ├─ cua-brain/              # Brain trait + 各 provider 实现
│  ├─ cua-orchestrator/       # 任务编排、重试、检查点
│  ├─ cua-bench/              # 评测集与打分
│  └─ cua-web/                # Web UI（直播/接管）
├─ docs/                      # 设计文档、各 provider 对接笔记
├─ scripts/
│  ├─ install-rust.sh         # 云端一键装 Rust 工具链
│  └─ restore.sh              # VM 重置后一键恢复（含对接现有 restore-remote-access.sh）
└─ .github/workflows/ci.yml   # fmt/clippy/test/build
```

## 5. 分阶段计划

### Phase 0 — 立项（1~2 天）
- 建 GitHub 私有仓（或用户指定位置），推初始 workspace 骨架
- 云端装 Rust（rustup stable），CI 跑通 fmt/clippy/test
- 输出：`docs/ARCHITECTURE.md`
- 验收：`cargo build --workspace` 在云端一次通过

### Phase 1 — 执行核心 cua-runtime（1~2 周）
- 动作原语：click / type / scroll / drag / key / screenshot
- 安全围栏：命令 allowlist、高危动作人工确认、dry-run 模式、全量审计日志
- 验收：在 XFCE 桌面上完成"打开浏览器→访问指定页→截图"无人值守闭环

### Phase 2 — 大脑抽象 + 首批 provider（2~3 周）
- 定义 `Brain` trait：`observe(screenshot) → plan → Vec<Action>`
- 先接：Anthropic Computer Use API → OpenAI CUA → Gemini（按 API 可用性排序）
- UI-TARS provider 写成 stub（接口对齐，等 GPU 机器）
- 验收：同一任务（"在表格里填 5 行数据"）可切换 2 个 provider 跑通

### Phase 3 — 编排与 Web UI（2 周）
- 多步任务、失败重试、检查点续跑、任务队列
- Web UI：屏幕直播、远程接管、任务启停
- 验收：从手机/电脑浏览器可看、可接管正在跑的任务

### Phase 4 — 评测 cua-bench（1~2 周）
- OSWorld 开源子集 + 10 个自建中文任务，输出各 provider 对比报告
- 这份报告就是"各家优缺点"的实测版

### Phase 5 — 交钥匙 hardening（1 周）
- 单二进制发行、`systemd` 服务、VM 重置恢复脚本、完整 README
- 交付：仓库 + 部署文档 + 演示视频（录屏）

> 以上时间为粗估；实际按每阶段验收结果滚动调整。

## 6. 安全红线（必须做）

1. 所有执行默认跑在云端 VM 沙箱，不碰用户本地电脑
2. 删文件/付款/发消息类动作必须人工确认（Web UI 点一下）
3. Provider 提示词注入防护：不执行屏幕上"意外出现"的指令
4. API key 走环境变量/Secure Vault，**永不进仓库**

## 7. 费用预估（粗估）

- 开发期 API 调用：Anthropic/OpenAI/Gemini 按量，评测阶段为主，预计几十美元量级
- 云端 VM：现有资源，零新增
- 域名/GPU：暂无，UI-TARS 本地化以后再说

## 8. 需要用户配合的事项（已压到最少）

1. **确认开工**（本规划 → 回复即可）
2. **仓库位置**：默认建 GitHub 私有仓 `computer-use-agent`（用已连的 GitHub）；如有别的要求请说
3. **API key**：Phase 2 开始前，你去各家官网开 key，走 Secure Vault 给我（我不经手明文）
4. **最终验收**：每个 Phase 结束你看一次演示，点"过"或提修改

其余编译、调试、写文档、跑评测，全部在云端完成，不打扰你。
