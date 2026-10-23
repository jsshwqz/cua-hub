# 变更日志

本文件记录 cua-hub 的可见行为变化。版本号遵循语义化版本；0.x 阶段以发布节奏为主，
不为了体现功能量级而跳版本。发版（tag / GitHub release）需要维护者明确点头，
合入 main 不等于发版。

## Unreleased — 已合入 main，尚未发版

### 新增

- **`then:[…]` 服务端动作链**：任意图元工具可带一组确定性后续动作，由服务端按序执行，
  只回一次结果——一份逐步 trace，加一次 `window_state` 观测。`observation_id` 与 `window`
  会被省略这两个字段的步骤继承，所以一次观测能驱动整批动作；`settle_ms`（默认 120ms）
  控制步间隔；`observe` 决定收尾怎么观测（默认取 diff、不带截图，`false` 只回 trace，
  给对象则原样传给 `window_state`）。
- 链在第一个失败处停止，`stopped_at` / `ran` / `planned` 报告的是**真跑了什么**，
  不是要求跑什么。模型驱动的 `run_task` / `orchestrate` / `browser_task` 不能作为步骤，
  步骤里再带 `then` 会被拒绝——链保持扁平，trace 才能按字面读。

### 验证

- 动作链本机真机实测（release 二进制，loopback HTTP + 记事本，同一任务两种写法各跑两轮）：
  分开调用 5 次 / 1527ms / 109.8 KB，链式调用 2 次 / 1320ms / 49.1 KB——少 3 个往返、
  省 55% 字节、快 207ms。时延没省更多，是因为"动手前观测一次、收尾观测一次"两边都要付；
  收益在往返次数与 token 上，链越长越明显。
- 负控：链中第三步参数非法时 `ran=3 stopped_at=2`，前两步确实落地（文字进了文档），
  第四步的回车没有发出（光标仍在第 1 行第 12 列）。
- 这组数字早期是在 debug 二进制上取的（94.6 KB / 48.8 KB、约 3.5 秒），debug 下图像缩放
  与哈希慢一个数量级，已按 release 复测更正。
- 新增 9 项测试（链策略面 8 项、捕获的 lossless 归属 1 项）。
- Linux 窗口级无障碍路径**真机结项**（`linux-atspi.yml`：Xvfb + openbox + at-spi2-core +
  GTK3 probe 窗口，MCP 服务在作业里真跑）。通过的断言：`list_windows` 看到窗口、
  `window_state` 返回窗口级树 6 个节点且三个命名控件都在树内、帧内标签证明不是桌面级整树、
  带回一张截图、`set_control_text` 写入 18 字符、`invoke_control` 触发按钮，
  并以 probe 应用自己写回的文本作为独立 oracle 确认真的落地。
  CI 产物：`linux-atspi-evidence`（每次调用的原始响应 + probe 输出）。

### 修复

- Linux：`atspi::set_value` 把 `SetTextContents` 的回包按"无返回值"解码。AT-SPI 实际返回
  一个 bool，于是**应用侧已经写入成功**的调用被 zbus 报成解码错误，工具回过来说
  "控件没有 Value/EditableText 模式"——方向完全相反，等于把可用控件判成不可用。
  现在按 bool 解码，只有 toolkit 说成功才算成功。（这个 bug 只有真机能发现，
  是上面那个 CI 作业跑出来的第一个成果。）
- Linux：CI probe 应用此前只在 AT-SPI 侧声明身份，X11 的 WM_CLASS 仍是脚本文件名，
  `snapshot_window` 的 app 匹配因此落空。现在两侧统一（`set_prgname` +
  `set_program_class`）。顺带记下真实缺口：WM_CLASS 与无障碍应用名不一致的应用
  （Python 脚本应用、Electron 等）今天在 Linux 上只有桌面级树可用 —— issue #16。

### 性能与诊断

- 观测路径不再无条件编 PNG。`frame_from_image` 每次都把未缩放原图编码一份 PNG，
  而只有视觉循环的运行日志会写这个文件。交错对照（同一台机器、同一时段、两个二进制
  交替各测 4 轮，取中位数）：Chrome 298→264ms、微信 127→114ms、Qoder 243→231ms、
  记事本 87→72ms，约省 5~11%。补了一条测试锁住回归：带 `lossless` 的帧必须以 PNG 魔数
  开头，不带的必须为空。
- 观测里新增 `backend_error`：WGC 被尝试又拒绝时返回**失败阶段**
  （`d3d11 device` / `capture item for window` / `no frame within 3000ms` 等）。
  此前 `capture_or_error` 已经写明阶段，调用方 `capture()` 却 `.ok()` 丢成 `None`，
  模块注释承诺的"能指出卡在哪一步"没有兑现——强制 WGC 后 `source` 仍是 `WindowSurface`
  却说不出原因，就是这个缺口造成的。
- 观测主序**不切** WGC，实测数据与理由记在 `docs/ROADMAP.md`。

### 文档口径

- 「Linux AT-SPI 窗口树真机验证」先改称「桌面级」，再按上面的结项补回窗口级。
  区分点是原语层的两条分支：`cua-hub tree` / `find` 走 `snapshot()`（registry 根下全部应用），
  而 `window_state` 走 `snapshot_window()`——按 app 与 title 匹配单个 frame，同一应用多个
  顶层帧且标题都对不上时故意报错而不是猜。前一次记录只验了前者就勾掉了后者的待办，
  口径与粒度对不上；现在两者分别有各自的证据（云机一次 + 可重跑的 CI 作业）。
  另：这条记录与 issue #7 是同一件事的两头——#7 提的是口径问题，本条是补上的证据。

## 0.1.3 — 2026-10-23

观测契约与可复用操控能力的一轮实质升级（P0 定位可靠性 + P1 观测效率 + P2 弹层与文本）。

### 新增

- `window_state`：一次返回窗口截图与窗口内无障碍树，作为唯一观测原语；
  返回 `observation_id`，之后的 `element_index` 与截图像素都必须引用它。
- 稳定元素编号：同一窗口内的控件按结构签名（路径 + 序号 + 控件类型 +
  AutomationId）获得跨观测保持不变的编号；被移除的编号作废且不会被复用。
- 无障碍树增量 diff：第二次起默认返回 unified diff（`mode: diff`），
  `unchanged` 表示映射仍有效，`disable_diffing: true` 或溢出时返回
  `baseline_reset` 并明确告知旧编号全部失效。实测资源管理器一次界面变化
  从 18.5KB 全树降到 947B diff。
- 树内直接给出控件可用动作（`actions=`）、当前文本值（`value=`）与焦点标记
  （`FOCUSED`），模型无需二次查询即可选择目标。
- `perform_secondary_action`：Expand / Collapse / Raise / Scroll Up·Down·Left·Right /
  Toggle / Select / Focus / Invoke；只接受树里列出的动作名，猜错返回
  `unsupported_action` 与真实清单；执行后回读控件状态供验证。
- `list_apps` / `launch_app`：应用发现与拉起。`list_apps` 按进程分组并给出可启动
  路径，`include_all` 追加开始菜单中未运行的程序；`launch_app` 等待窗口出现，
  已运行的直接返回现有窗口，并用 `matched_by` 说明识别依据。
- 弹层坐标空间：`window_state` 的每张图（目标窗口 + 菜单/对话框等弹层）带自己的
  `screenshot_id` 与原点；`click` 新增 `screenshot_id`，点弹层图片里的像素不再需要
  手工换算，且不会先去激活宿主窗口（那会关掉菜单）。
- 菜单与对话框进入窗口树：菜单栏项、菜单项、模态框按钮都有 `element_index`，
  可以按编号展开菜单、点菜单项、点"不保存"。
- 弹层元素树合并：同进程的弹层若未被宿主逻辑树包含，会作为独立小节
  （`# related window <id> "<title>"`）并入同一次观测，共用一套编号；
  已被覆盖或比宿主更大的窗口不再重复纳入。实测观测记事本菜单：菜单项只出现一次、
  无多余小节。
- `accessibility` 新增 `selected_text` / `document_text` / `selected_elements`：
  焦点元素、窗口根、再到树内文本类控件依次探测 Text 模式；选中的列表项/选项卡
  在树里带 `SELECTED` 标记并单列出来。实测写字板输入后全选，
  `selected_text` 与 `document_text` 均正确返回。
- 截图新增 Windows.Graphics.Capture 后端：PrintWindow 渲染成全黑的硬件合成/受保护
  表面自动改走合成器捕获，`source=GraphicsCapture`；`CUA_HUB_CAPTURE_BACKEND=wgc`
  可强制用于诊断。实测同一窗口两条后端得到相同平均哈希（内容一致、坐标契约不变）。
- Linux（AT-SPI）窗口级无障碍树：按应用名与窗口标题定位 frame 后只走该子树，
  多个顶层且标题不匹配时报错而不是猜。
- 诊断样例：`cargo run -p cua-io --example wgc_probe -- <hwnd> <w> <h>` 打印 WGC
  失败在哪一步；`cargo run -p cua-engine-access --example text_probe` 打印焦点控件
  到底有没有 Text/Value 模式。

### 修复

- 控件状态读取整体走错接口：`GetCachedPatternAs` 在真实 provider 上返回空，
  导致选中状态恒为假、展开状态不准。改为读缓存属性（VARIANT 转换），
  实测资源管理器点击文件后 `SELECTED` 与 `selected_elements` 正确出现。
- 动作标注不再"猜不准就全给"：Expand/Collapse 按当前展开状态只给可用方向，
  Scroll 按横/纵可滚动标志给出，不再对不可滚动的编辑框谎报四个方向。

- 无障碍树只有一层深度的缺陷：UIA 缓存请求使用 `TreeScope_Children` 时深层节点
  的 `GetCachedChildren` 返回空数组，导致菜单栏/资源管理器内容不可见；改为按层
  `FindAllBuildCache`，深度恢复且窗口级观测约 0.7s。
- 遮挡窗口截图只返回可见碎片的缺陷（P0）：改用 `PrintWindow(PW_RENDERFULLCONTENT)`
  抓窗口自身表面，并以 xcap 的物理矩形为基准，返回 `source` 自证。
- 坐标空间分裂（P0）：进程启动即声明 Per-Monitor V2 DPI 感知，UIA 矩形、
  `GetWindowRect`、截图与输入注入统一到同一物理像素空间。
- 点击时序：move → 稳定 → press → 保持 → release，避免 Qt 等框架按旧光标位置命中。
- 部分控件缓存 Value 模式返回空串（Win11 记事本编辑框），改为实时读取并截断，
  输入内容重新在树与 diff 中可见。
- `launch_app` 对"已运行进程开新窗口"（资源管理器、浏览器）识别不到窗口的问题：
  按新 pid → 名字匹配的新窗口 → 唯一像样新窗口三级识别。

### 约定

- `stale_observation` 一律表示"重新观测"，而不是重试同一编号。
- 安全策略对齐四档确认模型：交还用户 / 动作时确认 / 点名预批准 / 无需确认；
  把敏感数据打进表单本身即视为传输，需在按键前确认并说明数据与去向。

## 0.1.0 — 2026-10-20

首个可用版本：多引擎任务执行（视觉 / 代码 / 浏览器 / 无障碍）、MCP stdio 与
HTTP 服务、窗口截图与输入注入、运行历史与回放、跨引擎编排。
