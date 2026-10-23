# 变更日志

本文件记录 cua-hub 的可见行为变化。版本号遵循语义化版本；
发版（tag / GitHub release）需要维护者明确点头，合入 main 不等于发版。

## 0.2.0 — 2026-10-23

观测契约与可复用操控能力的一轮实质升级（P0 定位可靠性 + P1 观测效率）。

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
