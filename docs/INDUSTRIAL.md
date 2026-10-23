# 工业软件适配手册

工业软件不能靠一套通用提示词硬点。每个软件按同一模板建"规则包"，先只读、再
写操作，最后才谈自动运行。优先级固定为：

**官方 API / 脚本控制台 > 键盘命令 > 命名控件 > 像素坐标**

像素坐标只有在窗口位置、分辨率、DPI、缩放都固定的前提下才可靠；任何一步失败
都要回到截图重新确认，不能盲重试。

## 适配模板

| 项 | 内容 |
|----|------|
| 启动方式 | exe 路径、快捷方式、必要参数 |
| 窗口选择 | `list_windows` 用的标题/应用子串，是否允许最小化 |
| 无障碍能力 | UIA/AT-SPI 是否可用；哪些控件有名字 |
| 主通道 | 官方 API、Python/Lisp 控制台、命令行、快捷键 |
| 只读检查 | 版本、当前文档/项目、对象树、关键数值 |
| 写操作 | 允许改什么、改前备份、改后校验、回滚方式 |
| 禁区 | 授权/激活、密码、设备启停、PLC 下载、生产参数下发 |
| 已知问题 | 输入法、DPI、弹窗、启动耗时、单实例限制 |

## FreeCAD 1.1.3（已实测）

- 路径：`E:\Program Files\FreeCAD 1.1\bin\freecad.exe`；建议 `--safe-mode` 启动，
  否则第三方工作台（如 AionBridge）会在启动时弹错。
- 窗口：标题 `FreeCAD 1.1.3`；`list_windows {query:"FreeCAD"}` 可定位。
- 观测：先 `window_state {window, include_text:true}`。Qt 应用能给出工具栏/面板/
  对话框的元素索引，就用 `click {observation_id, element_index}`，不要再用像素。
- 窗口截图：`capture_window` 走窗口自身表面，被其他窗口盖住也能拿到整窗，
  返回 `source`、`origin`、`image_scale`、`hash`；坐标换算由服务端完成，
  调用方不要再自己加 origin 或除 DPI。
- 输入：优先 `window_input` 在一次调用里完成“聚焦 → 点控制台输入行 → 输入 → 回车”，
  避免多次调用之间焦点被别的应用抢走；`type_text` 已修复 Shift 字符。
- 已踩过的坑（修复前请当作事实）：
  - 进程不声明 DPI 感知时，UIA/`GetWindowRect` 给逻辑像素、截图给物理像素，
    点击会整体偏移；现在启动即 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`。
  - 注入点击必须 move→稳定→press→保持→release，否则 Qt 会按上一次的光标位置命中。
  - 草图编辑器里点中轴线/图元可能直接退出编辑；先 `window_state` 确认状态再动作。
  - 对话框内的属性输入框里按回车可能触发默认按钮（例如“创建”），需要提交时用 Tab。
  - 菜单/对话框是独立窗口：目标窗口的像素不会因菜单弹出而变化，
    要用 `wait_for_change`（它会把同进程弹层一起算进哈希）。
  - UIA 缓存请求的 `TreeScope` 决定深度：用 `TreeScope_Children` 时只有根的直接
    子节点带缓存属性，`GetCachedChildren` 对更深层返回空数组，整棵树会被悄悄压成
    一层（菜单栏看起来没有菜单项、资源管理器只剩四个 Pane）。现在按层
    `FindAllBuildCache(TreeScope_Children, …)` 逐层取子节点，深度与速度都正常
    （资源管理器 244 节点约 0.7s）。
  - 宿主应用的根元素可能是"空壳"：WinUI/Win11 记事本的窗口句柄在 UIA 里是一个
    没有名字和矩形的占位元素，真实内容在子节点里；窗口标题与边界以窗口列表为准。
  - 部分控件的缓存 Value 模式返回空串（Win11 记事本编辑框），输入的内容在树里
    不可见、diff 也就看不出变化；Edit/ComboBox/Spinner 的值改为实时读取并截断。
  - 菜单项、对话框按钮现在都在窗口树里有 `element_index`：先对菜单栏项
    `perform_secondary_action Expand`，再按编号点菜单项；保存询问这类模态框
    同样按编号点"不保存"。
  - 启动已运行进程的新窗口（资源管理器、浏览器）不会带来新 pid：`launch_app`
    先认新 pid，再认名字匹配的新窗口，最后只在"恰好一个像样的新窗口"时兜底，
    并在结果里用 `matched_by` 说明依据。
  - **`GetCachedPatternAs` 在真实 provider 上不可靠**：资源管理器列表项的
    `CachedIsSelected`、记事本编辑框的 `CachedValue` 都取不到，导致选中状态永远
    为假、输入内容看不见。控件状态一律走 `GetCachedPropertyValue` + VARIANT 转换
    （`VariantToBooleanWithDefault` / `VariantToInt32`），值走实时读取。
  - TextPattern 的覆盖面比想象窄：遗留 Win32 EDIT（含记事本字体对话框的编辑框）
    与 Win11 记事本主编辑区都不提供；写字板、Chromium 提供。取文本优先
    `value=` / `selected_text` / `document_text`，取不到再回到截图。
  - Win11 记事本的 ValuePattern 只返回**部分**文本（实测输入 43 字符只回后半段），
    不要把它当作文档全文来校验。
  - WGC 冷启动第一帧可能慢于 1.5s（实测热态 <0.6s），所以只在 PrintWindow 渲染成全黑
    时才走它；`CUA_HUB_CAPTURE_BACKEND=wgc` 可强制用于诊断，`source` 字段会写
    `GraphicsCapture`。
  - 观测一个模态框时不要把宿主窗口的树拖进来：只有比目标窗口**小**的同进程顶层窗口
    才算弹层，否则模态框的观测会把整个父窗口树复制一遍。
- 已验证的只读命令：

```python
print(App.Version())
print("DOC", App.ActiveDocument.Title if App.ActiveDocument else "NO_DOC")
print("WB", Gui.activeWorkbench().name())
```

实测返回 FreeCAD 1.1.3、`NO_DOC`、`PartDesignWorkbench`。

- 写操作规则：只允许新建临时文档 / 临时文件，禁止覆盖用户图纸；保存前确认路径；
  关闭临时文档时如果弹出保存询问，选择不保存。设备/产线相关操作不适用于 FreeCAD。
- 2026-10-22 实测：新建未保存文档 `CUA_Probe_20261022`，添加 `Part::Box`，
  重算后对象为 `[('Box', 'Part::Box')]`，体积 `999.9999999999998`（默认 10×10×10）；
  结果写入 `D:/Temp/freecad_model_result.txt` 校验，未保存任何工程文件。
- 下一步：对象树读取、临时文档建模、导出后校验；全部在只读/临时范围内完成。

## TIA Portal V14（已安装，未探测）

- 启动可能涉及授权与项目锁；先只读打开项目，读取在线状态/变量，不做下载。
- 优先评估 TIA Openness（官方 API）；UI 自动化只用于 Openness 覆盖不到的对话框。
- 任何下载、强制、启停操作必须人工确认并具备现场联锁。

## MCGS / McgsPro（已安装，未探测）

- 先复制工程模板到临时目录，再做画面/变量表修改；禁止直接改生产工程。
- 设备下载、运行启动属于生产变更，必须人工确认。

## ANSYS 2021 R1（已安装，未探测）

- 需要许可证服务器；先只读打开已有结果，查看网格/求解信息；不提交求解。
- 大批量求解属于高成本操作，先估价/确认再运行。

## 新增一个软件

1. 用 `list_windows` 确认窗口标题，用 `capture_window` 做遮挡截图。
2. 探测 UIA/AT-SPI，记录有名字的控件；没有树就走 API/命令行。
3. 先写只读检查脚本并反复验证，再写一条最小的写操作，最后才扩展到批量。
4. 把上述信息补进本文件，并在 Skill 的 "Industrial software" 规则下执行。
