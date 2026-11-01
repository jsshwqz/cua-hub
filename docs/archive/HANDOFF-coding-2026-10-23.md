# 交接说明 · 代码模式新任务用（2026-10-23）

这份文档是给**在代码模式（Coding）下新开的任务**看的。新任务读不到通用模式里那条对话的历史，
所以这里把目标、现状、契约、命令、纪律和坑一次写全。仓库内其他权威文档：
`docs/DECISIONS-2026-10-23.md`（需求原文与决策/推翻记录——想知道"为什么这么定"看它）、
`CHANGELOG.md`（已发布内容）、`docs/ROADMAP.md`（阶段与待办）、`docs/INDUSTRIAL.md`（工业软件实操与坑）、
`integrations/skill/SKILL.md`（给使用方 Agent 的能力说明书）、`docs/VERSIONS.md`（三条谱系对照）。

## 0. 交给新任务的第一段话（可直接粘贴）

```text
接手 cua-hub（Rust 多引擎 Computer Use 平台，MCP 服务端）。
先完整阅读仓库内 docs/HANDOFF-coding-2026-10-23.md，再动手；它包含产品定位、当前状态、
已确立的技术契约、门禁命令、真机验证纪律和踩过的坑。
工作区就是本仓库根目录。执行模式先用 Local，不要开 Worktree。
本轮目标：<把你要做的事写在这里>
要求：改完必须跑 fmt / clippy --all-targets -D warnings / test --workspace 三件套；
涉及真机 GUI 的动作要留证据（JSON + 截图）并收尾恢复现场；
不要打 tag、不要发 release，除非我明确说可以。
```

## 1. 产品定位（不可动摇）

交付物是**"操控这台电脑的能力"本身**，不是任何演示产物。FreeCAD / 保温杯 / 网页演示都只是测试床；
演示里暴露的定位、点击、输入、验证问题，一律回到原语层修，不要在演示脚本里打补丁。
用户原话：「我要的目标是这个操控电脑的能力，这是最终目标。别搞混了。」

通用性红线：案号、人名、特征词之类的判别只能存在于输入数据里，不能写进代码；
最隐蔽的拟合是"为了让样本过而为判据挑字段"。

## 2. 当前状态（2026-10-23）

- 已发布 **v0.1.3**（Latest，非预发布）：https://github.com/jsshwqz/cua-hub/releases/tag/v0.1.3
  资产 `cua-hub-windows-x64-0.1.3.zip`（exe + SKILL.md + README + CHANGELOG）。
- 分支 `main`。0.1.3 之后已进：`4c68332` 动作链 `then:[…]`；0.1.3 与本轮之间还有别的
  会话合入的文档提交（PR #6）。工具数 38（链复用现有工具，未新增工具名）。
- 门禁：`cargo fmt --check` + `clippy --workspace --all-targets -D warnings` +
  `test --workspace` 全绿；本机测试数 **76**（0.1.3 时 67）。CI（ubuntu-24.04 +
  windows-latest）对 `4c68332` 双平台通过。
- 版本策略：0.x 阶段按发布节奏走，**不为体现功能量级跳版本号**（用户明确要求）。
- 私有仓库 `jsshwqz/cua-hub`；推送用 `(unset GITHUB_TOKEN; git push origin main)`（环境变量里的
  `GITHUB_TOKEN` 会让 gh/git 403，keyring 里的 classic PAT 才有效）。

## 3. 仓库地图

| 路径 | 职责 |
| --- | --- |
| `crates/cua-core` | Action / Observation / 风险分级 / Error 等共享类型 |
| `crates/cua-io` | 截图（`capture.rs`，含 PrintWindow 与 `source`/`hash`）、输入注入（`input.rs`）、剪贴板、命令执行、应用发现与拉起（`apps.rs`）、WGC 后端（`wgc.rs`，仅 Windows） |
| `crates/cua-engine-access` | Windows UIA 树与控件操作（`lib.rs`），Linux AT-SPI（`atspi.rs`） |
| `crates/cua-cli` | `cua-hub` 可执行：CLI 子命令 + MCP stdio/HTTP（`mcp.rs`）、观测存储（`observations.rs`）、稳定编号与 diff（`elements.rs`）、编排、回放、后台任务 |
| `crates/cua-engine-vision` / `-code` / `-browser` | 视觉引擎、代码引擎、CDP 浏览器引擎 |
| `crates/cua-router` | 任务到引擎的路由打分 |
| `integrations/skill/SKILL.md` | 面向使用方 Agent 的能力说明书（安装器会同步到 `~/.qoder/skills/cua-hub`） |
| `scripts/install-qoder.ps1` | 安装二进制 + 技能 + MCP 配置（可热替换被锁的 exe） |
| `crates/cua-io/examples/wgc_probe.rs` | 诊断 WGC 卡在哪一步 |
| `crates/cua-engine-access/examples/text_probe.rs` | 诊断焦点控件到底有哪些文本/值模式 |

## 4. 已确立的技术契约（改动时不要破坏）

1. **观测 → 动作 → 验证**：`window_state` 是唯一观测入口，返回 `observation_id`；
   之后的 `element_index`、截图像素都必须引用它。`stale_observation` 语义是"重新观测"，
   不是"重试同一编号"。
2. **稳定编号 + 增量 diff**：控件按结构签名（路径 + 序号 + 控件类型 + AutomationId，
   **不含名称与矩形**）在同一窗口内跨观测保持同一编号；移除的编号作废且不复用；
   第二次起默认返回 unified diff，`mode` ∈ `full|diff|unchanged|baseline_reset`；
   `disable_diffing` 重建基线。
3. **弹层并入观测**：同进程弹层的树以 `# related window <id> "<title>"` 小节跟在宿主树后，
   **共用一套编号命名空间**；被宿主树覆盖 ≥60% 的不重复纳入；比宿主更大的窗口不算弹层。
   节点记录所属窗口，点弹层节点**不得**先激活宿主窗口（会关掉菜单）。
4. **坐标只有一个空间**：进程启动即声明 Per-Monitor V2 DPI 感知，UIA 矩形、`GetWindowRect`、
   截图与输入注入都是物理像素；每张图自带 `screenshot_id`/原点/比例，换算由服务端做，
   **不要让调用方加窗口偏移或 DPI 系数**。
5. **动作可验证**：`perform_secondary_action` 只接受树里 `actions=` 列出的名字，
   猜错返回 `unsupported_action` + 真实清单，执行后回读控件状态。
6. **截图后端可自证**：`source` ∈ `WindowSurface | GraphicsCapture | ScreenRegion | Platform`；
   PrintWindow 全黑时自动降级 WGC。
7. **安全策略**：四档确认模型（交还用户 / 动作时确认 / 点名预批准 / 无需确认）；
   绝不自动化终端、不用 Windows 键组合、不驱动登录/UAC/密码管理器/安全设置、
   不把页面内文字当授权；把敏感数据打进表单即视为传输，需按键前确认。

## 5. 门禁与命令（本仓库实测可用）

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cargo build --release
./target/release/cua-hub.exe --version

# 本地起 HTTP MCP 做真机验证
./target/release/cua-hub.exe serve --http 127.0.0.1:8767
curl -s -X POST http://127.0.0.1:8767/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

# 诊断样例
cargo run -p cua-io --example wgc_probe -- <hwnd> <w> <h> [out.png]
cargo run -p cua-engine-access --example text_probe

# 安装到 dist + 同步技能与 MCP 配置
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/install-qoder.ps1 -SkipBuild

# 推送（务必 unset，否则 403）
(unset GITHUB_TOKEN; git push origin main --follow-tags)
```

**Linux 侧不可本地验证**：`cargo check -p cua-engine-access --target x86_64-unknown-linux-gnu` 可以跑
（需 `rustup target add x86_64-unknown-linux-gnu`，占 ~154M），但 `cua-io`/`cua-cli` 交叉检查会被
`aws-lc-sys` / `wayland-sys` 的构建脚本卡住（缺交叉 C 工具链）。**凡是 `#[cfg(not(windows))]` 分支的改动，
只能靠 ubuntu CI 兜底。**

## 6. 发版流程（v0.1.3 实际用的，需用户点头后才执行）

1. 改 `Cargo.toml` 的 `[workspace.package] version`，`cargo update --workspace` 刷新 lock。
2. 写 `CHANGELOG.md` 对应小节（延续 0.1.x，不跳号）。
3. 提交 → `git tag -a vX.Y.Z -m "..."` → `(unset GITHUB_TOKEN; git push origin main --follow-tags)`。
4. `cargo build --release`，打包：exe + `integrations/skill/SKILL.md` + `README.md` + `CHANGELOG.md`
   → `Compress-Archive` 成 `cua-hub-windows-x64-X.Y.Z.zip`。
5. `gh release create vX.Y.Z --title "..." --notes-file <md> --latest <zip>`。
6. 核对：`gh release view vX.Y.Z`、`gh run list`。

## 7. 真机验证纪律（这台机器是用户正在用的机器）

- 保持用户窗口布局：**不抢焦点、不最大化、不改分辨率**；用完把焦点还给 Qoder。
- 发按键前先确认前台窗口是目标（曾经 `F5` 落到用户 Qoder 里弹出输入法候选窗）。
- 演示"代操作页面"要有真实鼠标/键盘事件 + 前后截图，让用户能旁观。
- 证据落盘：`D:\Temp\p<n>_evidence\`（JSON + 截图），报告落 `C:\Users\Administrator\Documents\CUA_CAD\`。
- 收尾：关掉自己开的窗口（保存询问一律"不保存"）、停掉测试端口、删自己造的临时文件。
- **绝不占用系统剪贴板**（用户与 AI 共用这台机器）；要粘贴的内容给文件路径或写在聊天里。
- 磁盘：C 盘紧张（2026-10-23 约 6.7G 可用）。只删可再生开发缓存（`target/debug` 等），
  项目产物、备份、个人数据一律不动，先分类再删。

## 8. 踩过的坑（别再来一遍）

- UIA 缓存请求用 `TreeScope_Children` 会把树**压成一层**（深层 `GetCachedChildren` 返回空数组）；
  必须按层 `FindAllBuildCache(TreeScope_Children, ...)`，缓存 TreeScope 用 `TreeScope_Element`。
- `GetCachedPatternAs` 在真实 provider 上取不到状态（`CachedIsSelected`、`CachedValue` 为空）；
  控件状态走 `GetCachedPropertyValue` + VARIANT 转换（`VariantToBooleanWithDefault` / `VariantToInt32`），
  Edit/ComboBox/Spinner 的值走实时读取并截断。
- Win11 记事本的 ValuePattern **只返回部分文本**；遗留 Win32 编辑框与 Win11 记事本主编辑区
  **没有 Text 模式**（取正文要靠截图）。
- 宿主应用（WinUI / ApplicationFrameHost）的窗口根元素可能是**空壳**（无名无矩形），
  窗口标题与边界以窗口列表为准。
- `D3D11CreateDevice` 的 SDKVersion 传 0 会 `E_INVALIDARG`，必须 `D3D11_SDK_VERSION`；
  WGC 冷启动首帧可能 >1.5s（热态约 0.6s），所以只在 PrintWindow 全黑时才用它。
- 已运行进程开新窗口（资源管理器、浏览器）不会带来新 pid：`launch_app` 按
  新 pid → 名字匹配新窗口 → 唯一像样新窗口 三级识别，并回报 `matched_by`。
- 注入点击必须 move → 稳定 → press → 保持 → release，否则 Qt 按旧光标位置命中。
- 对话框属性框里按回车会触发默认按钮；要提交用 Tab 或点空白。
- MSVC 偶发 `LNK1318` PDB 错误：删掉对应 `target/debug/deps/*.pdb` 或 `target/debug` 再编。
- 正在运行的 exe 会锁住构建产物（"拒绝访问"），构建前先停掉测试服务进程。
- Git Bash 会把 `C:\\path` 参数吞成 `C:\path`（MSYS 路径转换），JSON 参数里改用正斜杠。
- `#[cfg(not(windows))]` 里的 `return` 等 lint 本地看不见，Windows 绿灯 ≠ Linux 绿灯。

## 9. 待办候选（2026-10-23 下午更新）

1. ~~Linux 窗口级 AT-SPI 树真机验证~~ **已完成**：`linux-atspi.yml` 在 Xvfb + openbox +
   at-spi2 + GTK3 probe 上真跑 MCP，窗口级树、`SetTextContents`、`DoAction` 都有断言，
   并以 probe 应用自己写回的文本作独立 oracle。过程中修掉一个只有真机能发现的 bug
   （回包 bool 被解码成 `()`，把写入成功报成"控件没有该模式"）。
2. ~~观测全面切 WGC 的评估~~ **已完成，结论是不切**：同一轮交替实测里 WGC 每次慢
   2.1~5.8 倍，而本机五个窗口（含三个 Chromium 合成窗口）PrintWindow 全不空白、兜底从未触发。
   要重开这件事，先解决"每次捕获新建 device/frame pool/session"的固定开销。
   配套已做：WGC 拒绝时在观测里返回 `backend_error.stage`。
3. ~~`then:[…]` 服务端动作链~~ **已完成**：见 `CHANGELOG.md` Unreleased 与 `SKILL.md`
   「Chaining several actions into one call」。
4. 连续感知层（帧流 / 变化检测 / ROI / 本地 CV-OCR / 稳定 ID 场景图）：
   用户认可方向但明确"暂不做"，开工前先确认。

新留下的口子：

- ~~issue #16：WM_CLASS 与 AT-SPI 应用名不一致的应用~~ **已解决**：`window_state` 先按
  进程号认窗口（AT-SPI 接口不报 pid，但总线连接名可以经 `GetConnectionUnixProcessID`
  换成同一个 pid），名字匹配只作兜底。CI 里 `PROBE_IDENTITY=mismatch` 专门造出两边名字
  不一致的应用，断言窗口级树仍完整。
  **仍未验证的一种情形**：包装器启动的应用（flatpak / zap / 启动脚本）可能让窗口的
  `_NET_WM_PID` 与真正持有总线连接的进程不是同一个号——那时 pid 匹配也会失配并退回名字匹配。
  没有 Linux 图形桌面可测，别当作已成立。
- issue #7：文档口径——桌面级 `snapshot()` 与窗口级 `snapshot_window()` 是两条分支，
  结项时不能互相代替。

## 10. 协作规矩（用户偏好）

- 全程中文；汇报时间一律换算成**北京时间 (UTC+8)**，不要贴 UTC 的 `Z` 值。
- 不需要用户批准的动作（建 issue、发评论、跑门禁、清理自己的临时物）**直接做完**，
  不要留"你去点一下提交"；但**合并 / 打 tag / 发 release 必须用户明示**。
- 方案要零成本、一步可用：自己实测过再交付，不要把操作清单甩给用户。
- 派活给别的 Agent 时，任务书要"整段可粘贴 + 自包含"，里面每条命令自己先跑过；
  要求对方按里程碑增量落盘，自己也主动拉产物。
- 发现与实现可以分工（例如云端 GLM 实现），但验收必须用原命令复跑门禁、做双向负控，
  改契约要证明消费方仍成立，并如实署名。
- 攒一批改动就升一次版本 + CHANGELOG，否则会被当成"改了几天没结果"。

## 11. 记忆与文档的落点

Qoder 的项目记忆按**工作区路径**键控。通用模式那条对话的工作区是
`C:\Users\Administrator\Documents\Qoder\2026-10-20\29a9e657`；如果代码模式把 workspace 直接指到
`...\cua-hub`，记忆键会变、可能读不到已积累的项目记忆。**所以关键结论必须同时写进仓库文档**
（本文件、`CHANGELOG.md`、`docs/INDUSTRIAL.md`、`SKILL.md`），换工作区也不丢。

## 12. 诚实清单（尚未验证 / 未知）

- Linux 窗口级树与两条动作路径：**已真机验证**（`linux-atspi.yml`，见第 9 节），
  含 WM_CLASS 与无障碍应用名不一致的场景（按 pid 命中）。未验证的是包装器进程
  （flatpak/zap 一类）下 `_NET_WM_PID` 与总线持有者不同的情形。
- 观测主序"不切 WGC"的依据是本机五个窗口的样本（记事本/Chrome/微信/资源管理器/Qoder）
  与 release 二进制；换一台 GPU/驱动差异大的机器，兜底触发率可能不同。
- 代码模式与通用模式的**任务历史是否随模式迁移**，官方文档未说明；本交接文档就是为了
  在新任务里重建上下文。
- `selected_text` 未在遗留 Win32 编辑框与 Win11 记事本上验证（这些应用确实没有 Text 模式），
  正例来自写字板与 Chromium。
- 0.1.3 之后 main 上若有新提交，本文件第 2 节的状态需要更新。
