# 变更日志

本文件记录 cua-hub 的可见行为变化。版本号遵循语义化版本；0.x 阶段以发布节奏为主，
不为了体现功能量级而跳版本。发版（tag / GitHub release）需要维护者明确点头，
合入 main 不等于发版。

## Unreleased — 已合入分支，尚未发版

### 审计与路由：MCP 原子工具统一审计落盘与 Access 引擎窄提示路由（issue #14）

- **MCP 原子工具统一审计落盘**：`call_tool_single` 为每一次原子工具调用追加记录一条 JSONL
  审计记录（包含时间戳、工具名、脱敏参数、ok/failed、effect）；严格保证脱敏规则，参数值仅记录类型
  与字符/数组/对象长度，绝不落地剪贴板全文、token、密码或完整观测文本；`replay_run` 重放同样落盘
  并明示 `replay: true`；落盘错误仅 warning 记录，不阻断工具调用且不更改返回值与 MCP 字段定义。
- **Access 引擎窄提示路由**：在 Code 与 Browser 提示词之后、默认 Vision 之前新增窄 `access_hints`
  （"无障碍"、"accessibility"、"uia"、"at-spi"、"控件"），命中后主路由为 Access 且配置 fallback 为
  `[Vision]`；常规桌面任务（如"打开记事本输入 hello"）保持走 Vision，Code/Browser 提示词与显式 forced 规则优先；
  同步修正 README 引擎状态口径。
- **Fail 变更状态分类核验**：确认 `Action::Fail.is_mutating() == false` 语义与回归测试 `a_failed_run_did_not_modify_the_machine` 持续在列且通过。

## 未发布

### 浏览器 agent：输出上限可配（PR #41）

`max_tokens` 原先写死 700。现在按 **单次参数 > `CUA_HUB_BROWSER_MAX_TOKENS` > 700** 取值，
不配置时行为与之前完全一致。只限制作答长度，模型、温度与消息结构未动。

### 浏览器 agent：鼠标指示（本 PR）

点击和输入前，先把真实光标移到目标元素上停约 300ms，让守在屏幕前的人看得见工具要动哪里——
此前浏览器路径走 JS 点击与 CDP 合成事件，从头到尾不碰系统光标。

- 开关：`browser_task` 的 `indicate_mouse` 参数 > `CUA_HUB_BROWSER_INDICATE_MOUSE` > **默认开**
- 只移动光标：不按键、不抢焦点、不写进 steps、不参与回读裁决与风险判定
- `dry_run`（没有动作可指）与 headless（屏幕上没有窗口可指）自动跳过；连不上窗口几何或
  换算不出屏幕坐标时也只记一条日志，动作照常执行
- 单位边界：页面只会说 DIP，引擎也输出 DIP，乘回物理像素由提供光标移动能力的一侧负责


桌面与浏览器动作都改成"回读到什么就报什么"的一版：click/type 给 changed/unchanged/unverifiable，
type 对不上直接 FAILED；身份寻址按 target 现解析，歧义报错、绝不取第一个；滚动内化进动作，
自定义控件与 UEditor iframe 的类名/焦点协议化；另加质量门、并发保护和浏览器风险门。

### 身份寻址：歧义报错，解析到的节点就是动手的节点（PR #37）

`find_control` / `invoke_control` / `set_control_text` / `control_state` 以前按名字找，
引擎在同名里挑"更像能点的"（AT-SPI 的 `role_priority`）或按固定控件类型顺序
`FindFirst`（UIA 的 50000/50011/50007）。那等于替调用方猜：猜错时点击落在对方从未点名的
控件上，而且调用方无从得知。

现在四个工具接受 `target`：`name`（不区分大小写子串、`re:` 正则、或
`id:<AutomationId>`）加可选 `role` / `container` / `nth`（0 起）。命中多于一个**一律报错
并列出候选**，由调用方用 role/container/nth 说清楚；字符串 `name` 仍是
`{"target":{"name":...}}` 的简写，两者同时给会被拒绝。`id:` 走同一套匹配（精确、非前缀），
空名的 Win32 控件地址不变。

解析与动手是**同一个节点**：AT-SPI 用走到的 `(bus name, object path)`，UIA 用
RawViewWalker 走到的元素，解析之后不再按名二次查找（二次查找会把一个已经有歧义的问题
再问一遍可能已经变动的树）。UIA 的候选限制在 control element，与 `window_state` 展示的
集合一致；`ancestors` 最近优先，供 `container` 过滤。

- **行为回退的纠正**：`find_control {name: probe-ambiguous}` 从"返回按钮"改为报歧义；
  `{target:{name:"probe-ambiguous",role:"Button"}}` 才拿到按钮。`linux-atspi-runtime.py`
  用这两条钉住，`aligned` 与 `mismatch` 两个场景都跑。红 run
  [40208064523](https://github.com/jsshwqz/cua-hub/actions/runs/40208064523)，绿 run
  [40210705098](https://github.com/jsshwqz/cua-hub/actions/runs/40210705098)（67 PASS / 0 FAIL）。
- **连带变化**：`name` 是子串，所以 `probe-menu` 同时命中 `probe-menu-item`。运行时脚本
  那条 invoke 改为 `{target:{name:"probe-menu",role:"ToggleButton"}}`，不再依赖"精确名
  优先"这条已删除的隐含规则。真实桌面同理：`开始` 会同时命中两个 `开始学习` 链接，现在
  报歧义而不是替你选"开始"按钮。
- 截断的 walk 继续报 `did not reach`（不是"没有这个控件/没有这个模式"）；歧义与 `nth`
  越界明说重跑不会改变。两个后端共用同一套措辞。
- **代价（不藏在"更准"后面）**：Windows 动作从按名 `FindFirst` 改为每次动作走一遍树。
  本机 Windows 10、约 2300 个 control-view 节点的桌面、debug 构建、单轮测量：`find` 约 15s；
  同轮同规模的 control-view 快照约 14s——成本在树的大小，不在 walker 本身。共享机上有
  并发任务，这组数字用于说明量级，不作为基准；release 未单独取数。
- 本机 Windows workspace 测试 192 passed / 2 ignored；`cargo fmt --check` 与
  `clippy --all-targets -D warnings` 干净；CI `test (ubuntu-24.04)` / `test (windows-latest)`
  在 47ce157、f520201 与最终提交上均绿（Linux 侧计数以 CI 自己的输出为准）。

### 修复：Anthropic 会话被解析失败的 tool_use 毒化（issue #9）

一个不支持的动作原本会**让整个任务必死**。`computer_20250124` 的
`left_mouse_down` / `left_mouse_up` 等没有对应的 `Action`，走 `Err` 分支时只打一条
`warn` 就丢弃——但含它的 assistant 消息已经进了历史，Anthropic 要求
**每个 `tool_use` 必须有配对的 `tool_result`**，于是下一步的请求必然 400，
而 400 不在重试白名单里，此后每一步都带病发送。

现在把"链是否完整"变成结构不变量，而不是散在各分支里的 if：

- 一轮 assistant 内容被分成三个桶——可执行的（取第一个跑）、同轮多余的要**确认但不执行**、
  不能执行的要**回错误结果**（`is_error: true` + 写明是哪个动作不支持，模型据此自我纠正）。
  三个桶覆盖全部 id，不存在"被丢掉的 tool_use"这个状态。
- 回填时截图仍然附上：默认挂在被执行的结果上，若这一轮什么都没跑成则挂在第一条错误结果上——
  否则模型是"闭着眼睛"重复同一个错误。
- **被拒绝的动作优先于 `TASK_COMPLETE`**：同轮里既有被拒的 tool_use 又有完成文本时，
  先把历史补合法，不能直接结束任务留下残缺链。
- `take_ack` 现在只要"有东西要答"就触发（原来必须有已执行的动作才回填，
  整轮全是解析失败时连触发点都没有）。

### 测试与验证

- Anthropic 侧 `parse_tool_input` **此前零测试**（OpenAI 侧有 4 个），补 7 项：
  各动作映射（含 `double_click`→`double`、scroll 方向与符号）、**缺参数必须拒**
  （`left_click` 无坐标不能退化成点屏幕左上角）、不支持的动作报错里要带动作名、
  以及上面那条链完整性不变量的三种情形（混轮 / 整轮全拒 / 被拒与 TASK_COMPLETE 同轮）。
- workspace 测试 **95 → 102**。
- **端到端复现不需要 API key**：`tests/anthropic-pairing/stub.cjs` 是一个本地桩，
  它按官方规则校验每个请求——请求里有未配对的 `tool_use` 就回 400。
  `./tests/anthropic-pairing/check.sh <cua-hub 路径>` 会跑一次任务并打印每轮的配对情况与裁决，
  两向都实测过：

  | 传给 check.sh 的二进制 | 结果 |
  |--------|------|
  | 已发布的 0.1.5（`dist/cua-hub.exe`） | exit 1：`WARN ignoring unparsable tool call` → 下一步 `HTTP 400 … each tool_use block must have a corresponding tool_result`，桩日志 `unanswered=["tu_down_1"]`，裁决 "issue #9 present" |
  | 本次修复 | exit 0：`WARN no executable tool call this turn; answering the rejected ones next step` → 第二个请求 `paired=["tu_down_1"] unanswered=[]`，任务完成，裁决 "issue #9 fixed" |

  **真实模型那一格仍未验证**（本机无 `ANTHROPIC_API_KEY`）；桩只保证消息形状符合 API 的校验规则，
  不能证明真实模型在同一情形下的行为完全一致。
- #15（关键路径测试缺口）里的"vision 侧解析零覆盖"这一项随之关闭。

### 修复：浏览器 launch 失败与正常收尾都残留进程和 profile（issue #10）

`launch()` 在 `spawn` 之后任何一步失败（devtools 端口没起来、找不到 page target、
websocket 连不上）都是直接 `?` 返回。此时 `Child` 还没进 `BrowserSession`，而唯一的
清理写在 `BrowserSession::Drop` 里——失败路径根本构造不出 session；`std::process::Child`
drop 时又不杀进程，于是浏览器进程和它的 profile 目录双双留下。

- **所有权提前**：spawn 成功的一刻就把进程与目录交给 `PendingBrowser` 守卫，只有
  `commit()` 能把它们移交给 session；没 commit 就 drop，则杀掉、reap（不 reap 在 Unix 上
  留僵尸）、删目录。`launch` 里不存在"已经 spawn 但没人负责"的窗口了。
- **profile 不可能再被复用**：旧名 `cua-hub-browser-{pid}-{port}` 既可预测、又被
  `create_dir_all` 静默复用——上一次泄漏的目录（连着页面状态）会直接给下一次运行用，
  `lib.rs` 声称的 isolated profile 在失败路径上并不成立。现在名字是
  pid + 进程内计数 + 亚秒时钟，且用 `create_dir` 创建：名字已存在就换下一个，绝不接手。
- **`Child::kill()` 杀不到真正的浏览器**（真机量出来的，issue 里没写）：Edge 的 spawn
  目标会 re-exec 出浏览器主进程、自己退出。本机一次**正常收尾**的 smoke 之后仍剩
  8 个 msedge 进程和 1 个 717 文件的目录——我们杀的是那个已经退出的中间进程，
  `remove_dir_all` 因目录被占用而失败，又被 `let _` 吞掉。收尾因此改为：按
  `--user-data-dir` 反查"仍在用这个 profile 的浏览器进程"并杀掉（这个路径只有我们会
  生成，碰不到用户自己的 Chrome），再带重试删目录；实在删不掉就 `warn` 写明它留在
  临时目录，不再假装清理成功。Linux 用 `/proc/*/cmdline` 反查，不引入新依赖。
- 三条失败路径（devtools 没起来、找不到 page target、websocket 连不上）的错误现在都带上
  被杀进程的 pid（如 `debugger never came up for pid N`）。
  没有它，"`CUA_HUB_BROWSER` 指错路径"和"浏览器起来了但没响应"长得一模一样。

### 测试与验证（issue #10）

新增 8 条测试（7 条随 `cargo test` 跑，1 条要本机有浏览器、默认 `--ignored`），
workspace 执行数 **102 → 109**。

**进程存活以操作系统为 oracle**：`Child` 句柄会被 move 进守卫并被 Drop 消费掉，之后
没有任何类型还能告诉我们它死没死，所以直接查进程表（Windows `tasklist /FI "PID eq N"`，
Unix `kill -0`）——不是"句柄没了"这种同义反复。

| 改动 | 结果 |
|------|------|
| 把守卫的 `Drop` 改成空实现 | `a_dropped_guard_kills_a_live_child_and_removes_the_profile` 与 `a_failed_launch_leaves_no_profile_behind` 同时红：`pid 18232 outlived the guard that owned it`、临时目录列表里多出条目 |
| 把 `claim_profile_dir` 退回 `create_dir_all` | `a_stale_profile_is_never_adopted` 红：`an occupied profile directory was handed out again` |
| 把收尾改回"只 kill 句柄 + 删目录" | 真机那条 `--ignored` 红：`profile "D:\\Temp\\cua-hub-browser-10920-0-222119000" survived the session drop` |
| 修复后 | `cargo test -p cua-engine-browser` 13 过 1 ignore；`-- --ignored` 真机过；`browser_smoke`（走真实 session 收尾）前后对照 **8 进程 + 717 文件目录 → 0 + 0** |

已知边界：CI 上没有浏览器，`a_real_browser_session_leaves_nothing_behind` 只能真机跑
（`cargo test -p cua-engine-browser -- --ignored`）；Windows 上的反查每次收尾起一个
PowerShell 进程（约 0.2~0.5 秒），只发生在 session 结束时；macOS 没有反查手段，
仍只有 `Child::kill()`。profile 目录若始终删不掉，现在只警告不谎报，残留目录需要人工
或后续的统一清理来收——本次没做自动清扫旧目录，避免误删。

### 修复：Linux 无障碍匹配的三处静默错误（issue #11 第一批）

审计里的 Linux 问题不是一条，这批先做**不需要真图形会话也能判对错**的两条，再把两条
"静默缺位"改成有信号的；剩下要真机的（`GetState`、遍历超时、invoke 效果校验）留在下一批。

- **同名控件选错**（`atspi.rs` 的 `find`）：精确命中即 `return true` 结束整棵树，先到的
  同名控件胜出。GTK 的按钮自带一个同名的 label 子节点，于是 `find_control` 完全可能返回
  一个**没有动作的 Label**，调用方照着它的位置去点才碰得到按钮。Windows 侧按
  [Button, MenuItem, ListItem] 的角色优先级选，两侧行为不一致。现在把决定权收进 `pick()`：
  遍历到的节点**全部看完**再判，精确优先于部分匹配，同桶内更可执行的角色胜出，
  并列时取先见到的（编号不会因两次观测顺序抖动而换人）。
- **空名字的应用匹配任何查询**（`app_matches`）：反向包含那条腿问的是 `want.contains(c)`，
  而 `c` 是空串时对任何 `want` 都成立——没设置程序名的应用（Python 脚本、部分 Electron）
  因此命中每一次查询，`snapshot_window` 可能选错窗口。空候选现在直接跳过。
- **`state()` 在 Linux 恒 `Ok(None)`**：`None` 在契约里的意思是"这个控件没有状态可读"，
  真实情况却是"我们还没去读"。混起来会让依赖它的调用方错误降级成坐标点击。改为返回新增的
  `Error::Unsupported`；原注释里"AT-SPI 不把状态暴露为属性"的说法也是错的
  （`org.a11y.atspi.Accessible.GetState` 读得到），一并改掉——实现留到能真机验证的下一批。
- **`focused_element` 同理**：非 Windows 从 `Ok(None)` 改为 `Err(Unsupported)`。
  `window_state` 把它当参考信息，那里仍是 null，但**多带一个 `focused_element_note`**
  写明"读不到"而不是"没有控件持有焦点"；README 与 SKILL 的相应口径同时改掉。

### 测试与验证（issue #11 第一批）

- 新增 5 条纯函数测试（`pick` 的四种选择情形 + `app_matches` 的空名）。`atspi.rs` 在每个
  宿主上都编译（只有非 Windows 才会被调用），所以这些断言本机也跑；workspace 执行数
  **109 → 114**。
  **两处负控实测**：把早停塞回 `pick` 的循环 → `the_button_wins_over_the_label_listed_before_it`
  红（`left: "Label" / right: "Button"`）；去掉空名守卫 → `a_nameless_application_matches_no_query`
  红（`assertion failed: !app_matches("", "", "thunar")`）。
- **端到端的那条控制在 CI 图形作业**：GTK probe 现在放了一个裸 Label 和一个**同名** Button，
  且 Label 先被深度优先遍历到；`linux-atspi-runtime.py` 断言 `find_control` 返回
  `control_type == "Button"`。纯函数测试锁的是 `pick` 内部的决策，而修复前的早停发生在
  `walk` 的回调里——那一层只有真树能暴露，所以两边都要。PR 的提交顺序就是
  "先加断言（应当红）→ 再修（应当绿）"，CI 记录即证据。

### 修复：Linux 上"找不到浏览器"（issue #11 第 1 项）

`find_browser()` 的 Linux 候选是**相对名**（`chromium`、`google-chrome`…），命中判断用的却是
`Path::exists()`——相对名的 `exists()` 查的是**当前工作目录**，不是 PATH。审计的真机实证写在
issue 里：机器上装着 `/usr/bin/chromium`，不设 `CUA_HUB_BROWSER` 时 launch 直接报
"no Chrome/Edge found"，设上之后 `browser_smoke` 全套通过。引擎从头到尾没问题，卡在发现环节，
Linux 开箱不可用。

- 现在按 shell 找可执行文件的方式找：绝对路径直接判，裸名到 `PATH` 的每个目录里找，
  命中后返回**绝对路径**（这样 spawn 用的是真路径，报错里给的名字也对得上）。
- "存在"改成"可启动"：必须是文件，且在 Unix 上还必须有执行位。`CUA_HUB_BROWSER` 指到一个
  目录以前算命中（目录的 `exists()` 也是真），现在不算——并且会 `warn` 写明"这个变量设了
  但不可启动，已忽略"，不会让人对着 "no Chrome/Edge found" 去猜自己打错了路径。
- `PATH` 里的空项等价于"当前目录"，直接跳过——那正是这次要替换掉的语义。

### 测试与验证（issue #11 第 1 项）

`a_bare_browser_name_is_found_through_path` 在自己的临时目录里种一个假浏览器，把这个目录当作
唯一的 PATH 传给 `resolve_candidate`——**不碰进程环境**，所以并行安全。断言三件事：裸名能找到、
找到的是绝对路径、同名目录不算可执行。

两处负控分别把实现退回：

| 退回 | 变红 |
|---|---|
| 只对候选本身 `exists()`（= 修复前语义） | `a bare name listed in PATH must resolve to the planted file` |
| PATH 遍历改用 `exists()` 而不是可执行判断 | `assertion left == right failed: a directory is not an executable`，`left: Some("...cua-hub-lookalike.exe") right: None` |

**Linux 真机那一格要由验收方复现**：本机是 Windows，那条命令跑不了。在一台装了 chromium 的
Linux 上**不设** `CUA_HUB_BROWSER` 直接
`cargo run -p cua-engine-browser --example browser_smoke`——修复前报 "no Chrome/Edge found"
（issue 里已实证），修复后应 `BROWSER_SMOKE_OK`。

### 新增：Linux 也能读控件状态（issue #11 第 3 项）

`control_state` 原先只有 Windows 有实现。Linux 侧先是用 `Ok(None)` 蒙（等于宣称"这个控件
没有状态"），#25 里改成明确的 `Unsupported`，这一批接到
`org.a11y.atspi.Accessible.GetState` 上。

- **签名**：`GetState` 无入参，回 `au`——一串 u32 位掩码，按 `AtspiStateType` 的**序号**排。
  于是序号直接当位下标用，32 及以上落在第二个字（`INDETERMINATE=32` 就是词 1 位 0）。
  序号表是照 at-spi2-core `atspi-constants.h` 的枚举顺序**数**出来的：第一次抓文档给的
  VISIBLE / HORIZONTAL 各差一位，改用原始头文件重数才对上。错位不会报错，只会把一个状态
  读成另一个（把 collapsing 读成 checked 这种），所以这份表必须有独立证据撑着。
- **字段口径与 Windows 对齐**：`toggle` 是 `on` / `off` / `indeterminate`，`expanded` 是
  `expanded` / `collapsed` / `partially_expanded` / `leaf`，全小写——调用方在两平台看到同一套
  字符串，不用分支。
- **没观察到的事不写进结果**：`toggle` 只在控件自称可勾选时才出现（置了 CHECKABLE、已置
  CHECKED、或角色本身就是 CheckBox/RadioButton）；否则每个按钮都会带一个 `off`，那是在陈述
  一件我们没读到的事。`value` 与 `range_value` 仍是 `None`：文本在 Text 接口后面，滑块的数值
  在 Value 接口后面，这批都没查。
- `enabled` 取 SENSITIVE（"用户能操作它"），`offscreen` 取 `not (VISIBLE and SHOWING)`。
  两者刻意分开：滚出可视区的控件依然 enabled，把 offscreen 当 disabled 用是另一个错。
- 读状态**不再要求控件有非零矩形**——offscreen 恰恰是这种控件要报的答案。

### 测试与验证（issue #11 第 3 项）

- 5 条纯函数测试：位序号解码、`toggle` / `expanded` 的门控、offscreen 不等于 disabled、
  INDETERMINATE 落在第二个字、回包字数不足时是 miss 而不是 panic。这些本机就绿
  （`atspi.rs` 在每个宿主上都编译）。
  写夹具时我自己错过一次：把 CHECKABLE 塞进了按钮的状态集，"按钮不该有 toggle"当场红——
  红的是夹具不是实现；已改成两条都断言：不自称 CHECKABLE 的按钮没有 `toggle`，
  自称 CHECKABLE 的按钮**要**被采信。
- **红/绿排在 CI 的历史里**：提交 `c138cee` 只有断言、没有实现，`Linux AT-SPI runtime`
  （run `40108824986`）应当红在 `control_state reads the check box as off`；实现提交转绿。
- **独立 oracle**：probe 应用把自己每次 `toggled` 信号（GTK 的直接事件，不经过无障碍总线）
  写进第二个文件，脚本断言它恰好是 `["1"]`。少了这一条，"读回来的 toggle 变了"完全可能是
  我把序号解错、恰好命中另一个被置位的状态——控件自己的说法和总线的数字必须分开证。

### 新增：读不到的部分要说出读不到（issue #11 第 6 项）

`atspi.rs` 的遍历原先把所有 zbus 错误咽掉：每个节点串行发 4~5 个 blocking 调用、不设超时
（走 D-Bus 默认的 ~25s），失败就留空字段继续走。后果比"慢"更糟——一个卡死的应用会让树
**看起来是完整的**，只是少了它的子树，调用方据此得出"这个应用里没有那个控件"。

按定下的契约改成**带诊断的部分成功**：

- `UiSnapshot` 多一本 `unreadable` 账：每个读不到的子树记下 bus 名、AT-SPI 对象路径
  （就是索引注册表用的那个标识）以及是哪几个读没应答（`name` / `role` / `children` /
  `extents`）。节点仍按能读到的部分返回，少掉的部分不再无声。
- **把"没人答"和"这个控件没这个接口"分开记**：`UnknownInterface` / `UnknownProperty` /
  `NotSupported` 不记账——没有这条区分，每个 panel 都会变成故障，真卡死的应用反倒被淹掉。
  分类器写成"去空格再比"，因为同一次失败有两种措辞：
  `org.freedesktop.DBus.Error.UnknownInterface` 和 `No such interface ... on object`。
- **一次读最多等 1200ms** —— 但**不是**靠 `method_timeout` 做到的。zbus 5 的 blocking API
  没有客户端应答超时：`Proxy::call` 与 `blocking::Connection::call_method` 都只是
  `block_on` 那个 async 调用，而 `Builder::method_timeout` 只在 async `Connection::call`
  里被读取，blocking 类型不暴露那个方法。真机实测（run `40118056194`）：对端被 `SIGSTOP`
  之后，设了 `method_timeout(1200ms)` 的连接**连 180s 都没回来**。
  所以期限改在这里实现：每次节点读跑在一个持有连接克隆的工作线程上，调用方只等
  `CALL_TIMEOUT`，超时就把这棵子树记成读不到并继续。**代价说清楚**：那次跑着的线程没法取消，
  会停在 socket 上；因此加了一个 30s 的"这家人聋过"名单，同一 bus 在窗口内不再敲门——
  把一个装死应用的成本从"每次观测漏一个线程"压到"每 30 秒最多一个"。
- 整棵树的遍历另有一个 6s 的总预算（`WALK_BUDGET`），超了就停下并点名第一个**没被尝试**的节点。
  单次期限管不住"200 个节点 × 每节点若干次调用"，总预算才管得住。
- 输出口：`accessibility_tree` 的文本尾部加 `! parts of this tree were not read:`
  （最多点名 6 条，其余只给条数）；`window_state` 的 `accessibility` 里多一个
  **只在有情况时**出现的 `unreadable` 键。

Windows（UIA）那侧**没有**同一本账：`unreadable` 恒空，代码里写明原因（审计这条的范围就是
AT-SPI，UIA 的失败面要另立一条），不假装两平台一致。

### 测试与验证（issue #11 第 6 项）

- 5 条测试（每个宿主都编译）：两种"接口缺失"措辞都不该进账、`NoReply`/timeout/
  `connection closed` 必须进账且长度受限、`shorten` 只留一行截到 160、账本点名 6 条后
  仍报剩余条数、以及**永不返回的读会被期限截下且同一 bus 在冷却窗口内不再重试**
  （这条不要真总线，用一个死循环闭包就能证）。
- 分类器第一版把 `UnknownMethod` 漏了，CI 的健康树因此出现误报（`[:1.19] extents:
  UnknownMethod: Method "GetExtents" … does not exist`——那是没有 Component 接口的应用根节点，
  而同一行输出里 `[:1.20] no reply within 1200ms` 才是真装死的那个）。补 `UnknownMethod` /
  `UnknownObject`，并加一条**反向断言**：健康的窗口必须什么都不报。缺了它，一个喊狼来的
  分类器照样全绿。夹具用的是那条真回复的原文。
- 动作路径（`invoke_control` / `set_control_text` / `control_state`）当时**仍未包期限**，
  对装死的应用会挂住不返回——不在"加了超时"这句话里混过去。下面一节把它闭合了。
- **端到端红/绿排在 CI 图形作业里**：新阶段起两个 probe，`SIGSTOP` 其中一个，再取桌面树，
  断言① 活着那个应用仍在树里（不是整次失败）、② 输出点名读不到的部分、③ 整体耗时 <10s、
  ④ 先用纯 X11 的 `list_windows` 证明服务器还活着。
  **这两条"红"都不是我原先预测的样子，记录如下**：只放断言的那次（run `40115985180`）
  不是断言红而是**崩在 180s 读超时**——原因是 harness 自己从不读服务器 stdout 管道，
  日志填满 64KB 后把服务器堵在写操作上，跟 AT-SPI 无关；修好管道之后，用了
  `method_timeout` 的那次（run `40118056194`）**同样 180s 不回来**，才暴露出 zbus 5
  blocking 路径根本没有客户端超时。换成工作线程期限后（run `40119297316`）五条断言全绿、
  耗时 1.2s，但同一份输出里冒出一条误报（见上）。①在所有修复前版本上都是**通的**——
  这不是断言写错，而是这个 bug 的形状：观测"成功"返回，只是静默少了东西。

### 修复：没走到 ≠ 没有那个模式（issue #11 第 7 项）

`snapshot` / `find` 用调用方传的节点上限，`invoke` 与 `set_value` 却各自写死 600/10；
而公共文档把 `Ok(None)` 讲成"这个控件没有该模式"，调用方拿到 `None` 就降级成坐标点击。
两者合起来的结果是：**桌面太大、还没搜到目标** 和 **控件真的不支持该模式** 共用同一个答案，
前者会被点到错地方。

- 新增 `invoke_with` / `set_value_with`：上限交给调用方；默认仍是 600/10，且常量收敛到
  `DEFAULT_ACTION_NODES` / `DEFAULT_ACTION_DEPTH` 一处。`invoke_control` 与 `set_control_text`
  两个工具接受可选 `max_nodes` / `max_depth`。
- **搜遍没找到、而且遍历是被截断的 → 返回 `Err`**，里面写清走了几个节点、深度多少、
  `max_nodes` 多少、树仍然截断，并直接点名要防的那次误读："这不是'控件没有该模式'"。
- `Ok(None)` 的意思因此收窄成它本该表达的：走到了那个控件，它没有该模式。
- Windows 的 UIA 遍历不按节点预算走，`_with` 的数字在那边**被忽略**而不是假装生效，代码写明。
- 这条错误改用它自己的 `Error::Lookup`。第一版借的是 `Error::Capture`，CI 真机输出因此长成
  `capture failed: did not reach set_value on "probe-entry" …`——内容对、帽子错，
  读的人会以为截图环节出了问题。断言现在同时钉住这个前缀。
  （这个类型是第 6 项的后续提交 `f2bdf76` 为"没人答"新建的：两种误读最后要的是同一顶帽子，
  本支只是把 `capped()` 接上去。）
- 文档把三种答案一次写清（`README.md` 的工具表 + `integrations/skill/SKILL.md`）：
  `did not reach` = 预算不够、`did not get an answer` = 那个应用没回话、
  只有走到控件后的 `None` = 真的没有该模式。前两种都明写了"不是降级成坐标点击的理由"——
  第 7 项要防的误读发生在调用方读到的那句话上，光改返回值不够。
- 验证：本机 1 条测文案（必须含 `did not reach` / `after 1 node(s)` / `max_nodes=1`，
  且不得再出现旧措辞）；CI 真树每场景两条——预算为 1 的 `set_control_text` 与
  `invoke_control` 必须报"没走到"，紧跟其后的正常调用必须成功。这一对是刻意配的，
  它证明红绿来自**预算**而不是来自控件本身。修复前这两个参数被直接忽略：文字照样写进去、
  按钮真被按下，两条都红。

### 补：动作路径也在期限内，并且不再把沉默当成"不支持"（#28 自己声明的边界）

第 6 项合入前我在 PR 里留了一条边界：**只有树被期限保护**，`invoke_control` /
`set_control_text` / `control_state` 对装死的应用仍会挂住。这条现在闭合了，
过程里又挖出同一族的两个问题：

- **沉默、"没这个接口"、"答了"是三件事**。旧代码里 `DoAction` / `SetTextContents` 的
  失败和"这个控件没有 Action / EditableText 接口"走同一个分支，最终都变成 `Ok(None)`——
  而 `None` 的文档含义是"控件没有该模式"，调用方据此降级成坐标点击。现在统一走
  `classify()` 三分类，沉默返回 `Error::Lookup`，消息里直接写明
  "This is not \"the control has no invoke pattern\""。
- **`DoAction` 的回包 bool 之前被无视**：旧写法是 `.is_ok()`，也就是一把拿"工具包明确回答
  我拒绝了"也算触发成功。现在只有 `Answered(true)` 才算。这跟当年 `SetTextContents`
  把 bool 解成 `()` 是同一类错误，只是这次方向相反。
- **证明放在能确定性触发的地方**：三分类由本机两条单测钉住（`classify` 拆成纯函数，
  喂死循环 / `UnknownInterface` / `NoReply` 三种输入）。CI 里那三条"动作也要有界限"
  的断言**只是不变量守卫，不是这条修复的证据**——因为树已经先跳过了聋掉的应用，
  动作只会打到活着的那个，修复前那三条同样会绿。这一点写在脚本注释里，
  免得有人把它们当成红/绿对照。
  负控：把 `classify` 的 `NoReply` 分支折回 `NoPattern`（= 修复前的形状），
  `a_silent_toolkit_is_not_reported_as_a_missing_pattern` 立刻红。

### 新增：invoke 之后回读控件状态，给出效果裁决（issue #11 第 8 项）

审计的实证是 Xfce 面板的 Applications 按钮：`DoAction` 答了、菜单没开、树里没有菜单项，
而工具无条件报 `invoked ... via accessibility pattern`。**"模式答了"和"控件做了事"是两件事**，
只把前者当成功交付，依赖成功信号的 agent 就会在这类控件上静默失败。

- `invoke` / `set_value` 现在返回 `ActionReport { node, effect, state }`，
  `effect` 三档：**`changed`**（控件自己某个可读状态动了）、**`unchanged`**（发布了 toggle /
  expanded 的控件前后一模一样）、**`unverifiable`**（读不回来，或这控件本来就没有可动的状态位）。
  裁决来自 `judge_effect(before, after)` 这个纯函数，两平台共用一套口径。
- **`unchanged` 只在有状态可读时才说**：普通按钮没有 toggle / expanded，若把它判成
  "没变"，那等于每次成功按按钮都报一次假阴性，而一个经常错的判据会被调用方忽略——
  原来那个 bug 就是这么被信任下来的。这种情况报 `unverifiable`，让调用方去看画面或树。
- **回读失败不是错误**：动作已经答了才去读状态，读不到就记 `unverifiable`；
  把整次调用判失败会让人以为动作没做。
- `set_control_text` 的判据是"字段里最终是不是我要的那段文字"（`judge_text_effect`），
  不是"值变了"。Linux 侧 `value` 还没接 Text 接口，所以那边诚实报
  `unverifiable (this platform cannot read the field's text back)`，不借用
  `SetTextContents` 回了 true 这件事冒充"改好了"。
- 顺带关掉 #29 记录的那条缺口：**"沉默 → `Err`" 的接线本身没有钉子**。判定抽成
  `action_miss(name, action, silence, walked, ...)` 纯函数（沉默优先于截断、走完且没找到
  才是合法的 `Ok(None)`），三条本机单测钉住；原来这两段判断在 `invoke_with` /
  `set_value_with` 里各写一遍。
- 期限与冷却（1200ms / 30s）不变；每次回读同样在期限内。`unchanged` 时最多再等 200ms 读一次
  （`EFFECT_SETTLE_MS`），因为下一帧才更新的工具包会被立刻读成"没变"。

### 测试与验证（issue #11 第 8 项）

- 本机新增 10 条纯函数单测：动了→`changed`、没动→`unchanged`、读不回→`unverifiable`、
  普通按钮不得冒充 `unchanged`（flood guard）、改名/改类型不算效果、NaN 滑杆不算效果、
  文字赋值只认"是否等于要的那段"，以及 `action_miss` 的三种次序。workspace **139 passed / 1 ignored**。
- **Windows 真机自证**（本机，不靠 CI）：一个 WinForms 探针窗口写自己的日志当独立 oracle，
  三个按钮分别是"只写日志"、"改别的控件"、"把自己禁用"。实测：
  `cua-flip-self` → `effect=changed`（日志 `flipped:disabled` 独立印证）；
  `cua-noop` → `effect=unverifiable` 而日志 `noop:clicked` 证明动作确实发生了——
  这正是审计那种"答了但看不到效果"的形状，现在不再冒充已验证；
  `cua-toggle-target` → `unverifiable`（效果落在别的控件上，按钮自己读不出来）。
  `unchanged` 在 Windows 上没有真机样本：普通按钮不发布 toggle / expanded，
  本机够不到的控件才有；Linux 那一格实际走到了 `changed`（见下一条），所以 **`unchanged` 两平台
  都没有真机样本**，只有本机单测钉住语义。
- Linux 真机（CI 图形作业）：夹具新增 `Gtk.MenuButton`（accessible name `probe-menu`），
  断言不是"跟自己的回复比"，而是**跟树比对**：`effect=changed` 当且仅当菜单项真的进了树。
  红的那次（run `40147535682`）8 条失败全是新断言，原文点名旧回复里没有裁决：
  `invoked ToggleButton "probe-menu" via accessibility pattern`（`menu_item_in_tree=True`，
  也就是"菜单开了但我们报不出这件事"）。绿的那次（run `40147819661`）真机原文：

  ```
  PASS  invoke reports the effect it read back            [CheckBox "probe-toggle" ...; effect=changed (toggle=on)]
  PASS  the verdict agrees with whether the menu is on screen [ToggleButton "probe-menu" ...; effect=changed (toggle=on), menu_item_in_tree=True]
  PASS  invoke_control fires DoAction on the button        [Button "probe-submit" ...; effect=unverifiable (no readable state on this control)]
  PASS  a text write says it could not be read back here   [Text "probe-entry" ...; effect=unverifiable (this platform cannot read the field's text back)]
  ```

  **跟预期不一样的地方，如实记下来**：GTK 的 `MenuButton` 被 `DoAction` 真的打开了菜单，
  所以那一格走 `changed`——审计在 Xfce 面板上撞到的"答了但什么都没出现"，在这两个 CI 的夹具上
  都不复现。因此 **`unchanged` 至今没有真机样本**（Windows 没有、Linux 也没有），只有本机
  `an_answered_action_that_moves_nothing_says_unchanged` 钉住它的语义；在那一格拿到真实控件样本之前，
  它应当被读成"语义已定义、真机未观测"。我没有把夹具改成"故意不开菜单"去凑一个红——那是在造
  一个我想让它红的样本，而不是在测真实工具包。真实面板控件（普通按钮）走的是 `unverifiable`，
  那一格两平台都有真机样本（`probe-submit`、`cua-noop`）。

### 修复：窗口相对坐标不再能逃出窗口，无鉴权绑定会大声说（issue #12 第 3、4 项）

- **`window_input` 的 `x`/`y` 之前完全不校验**：只做 `as i32` 截断 + 与窗口原点
  `saturating_add` 就注入。偏移足够大就能点到屏幕任意位置，而工具自述"绝不落到无关应用"。
  现在窗口相对点必须落在**该窗口自己的矩形内**，越界返回
  `offset_outside_window`（带上窗口矩形与建议），只给 `x` 不给 `y` 也拒绝——
  旧代码会**静默忽略**半个点，照样回 `ok:true`。算术全程 i64，
  因为 `as i32` 会把 2^32+50 折回 50（正好又"在窗口内"），那是要防的形状而不是表面那个越界。
- **`invoke_control` 的矩形回退点击之前不查退化矩形**：工具包对没布局过的控件回
  `(0,0,0,0)`，它的"中心"就是屏幕左上角那个像素——名字匹配上却点到别处。
  现在所有回退点击都走同一个 `control_center()`，`invoke_element` 与 `click {element_index}`
  也一并改用它，检查不会因为下一个调用点而再被忘记。
- **`CUA_HUB_ALLOW_REMOTE=1` 放行时以前不打字**：拒绝非回环绑定的文案写得很清楚，
  穿过这道 opt-in 之后只有一行普通 `listening`。现在非回环 + 无 token 时打印醒目横幅并
  `tracing::warn!`，明写"任何能连到这个端口的人都能操控这台桌面，含 `run_task`（可执行命令）"；
  回环或已设 token 则不打（把假警告刷在每次正常启动上，会让人跳过真警告那条）。

### 测试与验证（issue #12 第 3、4 项）

- **修复前的真机复现（本机 Windows，红）**：两个无关窗口 A(159,150,507x291) 与
  B(1259,875,507x291)，B 的 TextBox 每次变化把自身内容写进自己的文件（独立 oracle）。
  对 A 发 `window_input {x:1241, y:800, text:"ESCAPED-INTO-B"}`，修复前的二进制答
  `{"ok":true,"foreground":true,"typed":true,"window":{...cua-window-A...}}`，
  而 B 的日志逐字记下：`B received: E` … `B received: ESCAPED-INTO-B`。
  **文字确实进了那个无关应用，而答复声称落在 A。**
- **修复后的同一发调用（绿）**：返回
  `{"ok":false,"error":{"code":"offset_outside_window",...}}`，A/B 的日志都没有新内容；
  紧接着合法的 `{x:60,y:60}` 仍然成功，A 自己的日志写进 `INSIDE-A-OK`——
  拒绝不是把这条工具一并禁掉。
- **坐标校验改到聚焦之前**：第一版把校验放在 `focus_window` 后面，CI 暴露两件事——
  一次"注定被拒"的调用已经改了用户看到的前景窗口；而且 Linux 上 `focus_window` 未实现，
  校验永远走不到，红/绿样本两边都失败在 `focus_failed` 上（那不构成这条修复的证据）。
  现在校验先行，两平台给出同一个拒绝。
- **顺带查出来的另一条缺口（不在本 PR 修，已记 ROADMAP）**：`window_input` 在 Linux 上
  **整条不可用**——`focus_window` 只对 Windows 实现，任何一次调用都停在 `focus_failed`。
  审计第 3 项在 Linux 上当时不可达：洞在代码里，但被前面那一步挡住了。
- 本机新增 8 条单测（窗口内/越界/半个点/i32 回绕/退化矩形/警告的三种组合），
  workspace **147 passed / 1 ignored**。这一格没有"改坏就红"的老测试可退回——
  旧代码根本没有这条判据，所以红/绿靠上面那两次 Windows 真机运行。
- Linux CI 图形作业里新增的三条 `window_input` 断言**只当绿色守卫**：钉住"越界与半个点
  会被正确拒绝、且校验先于聚焦"，但**不能**当红/绿对照——未修复的代码在 Linux 上走不到
  那一步（第一次试图用它做红样本时，失败原因两边都是 `focus_failed`）。这点写在脚本注释里。
- 没做的：`x`/`y` 仍按窗口左上角算，不含 DPI 逻辑/物理换算（现有窗口矩形就是这个口径，
  与 `click {element_index}` 一致）；`window_state` 的观测坐标另有一套 `point_in` 校验，未动。

### 新增：浏览器动作也过同一道确认门（issue #12 第 1 项）

审计那条"模型动作全自动执行"是真的：四个入口（`run_task` 路由到浏览器、`orchestrate`、
MCP `browser_task`、CLI `cua-hub browser`）都在 `confirm()` 执行门**之前**就返回，
`BrowserAction::Eval` 是任意 JS。system prompt 里"未经任务明确要求不提交表单/不登录"
是软约束——DESIGN.md 自己就写着"that is a policy, not a sandbox"。

- 浏览器动作**不**并入 `cua_core::Action`（契约认了 DESIGN.md 的理由），改为在自家 enum 上
  加 `risk(page_url)`，并在每一步执行前过**同一个** `cua_core::policy::confirm`。
  档位按契约：`Eval` 一律 High；`Navigate` 跨站 Medium、同站 Low；`Click` 指向提交控件
  Medium、其余 Low；`Type`/`Press` Low 但 **Enter 一律 Medium**；`Wait`/`Scroll`/`Tab` 类 Low；
  判不出来按 Medium。
- `yes` / `allow_high` / `approver` 由 `BrowserRunOptions` 传进去，四个入口全部接上；
  MCP 侧 `browser_task` 新增这两个参数，CLI 侧 `cua-hub browser` 新增 `--yes` / `--allow-high`。
  默认全是关的——和 #8 那次默认档反转一个口径。
- **dry-run 打印档位**：`[00] eval document.cookie [risk=high]`，规划阶段就能看见要付什么许可。
- 被拦时**整轮停下**而不是跳过这一步：下一步的计划是建立在"这个页面我没动过"之上的。
  `steps.jsonl` 里记 `blocked before execution: ...`。
- 执行门本体从 `cua-cli/runner.rs` 提到 `cua-core::policy::confirm`，两边共用一套梯子
  （重复一份梯子正是两个入口对"`yes` 能买到什么"说法不一的方式）。`Approver` 现在带
  `(Risk, String)`：只收到描述的连接串无法按档位措辞，而"approve this?"就是这样变成回车键的。
- 一处我没按契约字面执行，**请指一句**：契约把 `Tab` 类列为 Low，但 `TabNew` 带 url 做的
  就是跨站跳转，我按导航判（跨站 Medium）。代码与测试都按这个走，改哪边你们定。

### 变更：从思维链提取的动作降档使用，不再与"说出来的"同价（issue #12 第 2 项）

契约是"保留回退、不拒绝，但升一档风险并打日志"。两处都做了：

- **代码引擎**：`Action::RunCommand` 新增 `from_reasoning`（`serde(default)`，
  老的 `steps.jsonl` 照样能回放），`Action::risk()` 在该位置上一档、High 封顶。
  于是思维链里那句"do not run `{"command": "cargo build"}`"不再是和"模型决定执行"同一价。
- **浏览器引擎**：`ask()` 现在返回 `(action, ActionSource)`；来源是 reasoning 时升一档并
  `warn!`（旧代码是**静默**解析思维链的）。content 与 reasoning 都拿不到动作时照旧报错。
- **可观测到的代价，说清楚**：读-only 命令从 Low 升到 Medium，也就是说
  **只在思维链里出现的 `ls` 现在也要一次决定**。这是契约"升一档"的字面后果，我没有偷偷
  给读操作开例外——开了就不是"升一档"而是另一套形状判据。
- `describe()` 会写出 `(from reasoning)`，确认提示里人看得见来源。

### 变更：`window_input` 在 Linux 明确说"这台机器没实现"（issue #12 第 3 项后半）

`focus_window` / `perform_secondary_action` 之前把"本平台没实现"标成 `Error::InvalidAction`
（意思是"引擎给了个非法动作"），于是 `window_input` 在 Linux 每答 `focus_failed` +
`retry:true` ——一个会让人反复重试的假可重试。现在改为 `Error::Unsupported`，
`window_input` 据此返回 `code:"unsupported"` + `retry:false` + 下一步该怎么做的提示。
`cua-io/apps.rs` 里 `launch_app` 有同一处误标，**本 PR 没动**（不属这条契约，单独提）。
X11 的窗口激活实现仍在 ROADMAP backlog。

### 测试与验证（issue #12 第 1、2 项）

- 本机新增 25 条（档位表 10 条、执行门 6 条、来源升档 4 条、其余为宿主比较等），
  workspace **166 passed / 1 ignored**。
- **真端到端（本机 Chrome + 本地桩模型，不要 API key）**：`tests/anthropic-pairing/stub.cjs`
  的先例做法——桩按 `STUB_ACTION` 返回固定动作，`cua-hub browser` 真起真浏览器真跑循环：

  | 桩返回 | 参数 | 结果 |
  |--------|------|------|
  | `eval document.cookie` | 无 | `blocked before execution: high-risk action blocked: ... Re-run with --allow-high` |
  | `eval document.cookie` | `--allow-high` | 执行了（页面里脚本真跑了，浏览器回的是 cookie 权限错，正说明它到了执行层） |
  | `press Enter` | 无 | `blocked before execution: medium risk action requires --yes / yes=true, or a client that can answer confirmations` |
  | `press Enter` | `--yes` | 执行 |
  | `wait 10ms` | 无 | `waited 10ms`，没要任何许可（低危不打扰） |

  每步都打印档位，如 `[00] eval document.cookie [risk=high]`。跑完没有残留浏览器进程
  （0 个 `cua-hub-browser-*`），顺带把 #10 那套收尾在真实循环里又核了一次。
- 桩脚本：`D:\Temp\2026-10-25_browser_model_stub.cjs`（要的话可以收进 `tests/`，
  现在还没入库——入库要放对目录并写清它只用于本地验证）。

### 新增：浏览器 click / type 也要回读裁决，富文本走 insertText（急件契约三项）

契约（issue #12 评论 6396246834）：**浏览器路径的"成功"必须是自己读回来的，不是动作返回的**。
三项都落在引擎层（`cdp.rs`），`mcp.rs` 一行没动。

- **click 回读裁决**：点击前后各读一次**目标元素自己的语义状态**
  （`checked` / `selected` / `aria-expanded` / `disabled` / `value`），
  答复变成 `clicked #agree [effect=changed] (checked Some(false)->Some(true))`；
  二次点击给 `checked Some(true)->Some(false)`。
  元素什么都不发布（比如 `role=button` 的 `<div>`）→ **退到像素对比**
  （同一编码器、同质量的 viewport 截图前后各一张，字节相同=没变，不同=有变化）；
  截图也拿不到 → `effect=unverifiable`。**旧答复 `OK` 在这条路径上不再可能出现**
  （有一条真机断言直接 `assert_ne!(note, "OK")`）。
  `textContent` 故意不参与比较：页面重绘会动文字，那会让"什么都没点的点击"看起来成功。
- **type 回读**：写完重新读元素自己的 `value`，**与要写的完全一致才算数**；
  不一致直接报失败（`FAILED: #upper reads "ABC", not "abc" -- ...is not reported as a success`），
  读不到 value 报 `unverifiable`，绝不冒充 ok。为了让"下一步"能看到生效，回读带最多两次
  120ms 的等待（React 下一 tick 才更新，立刻读会假报"没变"）。
- **contenteditable 富文本**：先 `selectNodeContents + collapse(false)` 把光标放到**末尾且不留选区**，
  再走 CDP `Input.insertText`（真输入事件，不整段替换、也不像 `el.value=` 那样跳过框架监听）；
  回读用**三条标准**而不是逐字相等：内容非空 + 增长长度对得上插入量 + 关键句在场
  （关键句取负载中段，不是整段）。普通 input/textarea 仍旧走"回读相等"。

`Effect`（`changed` / `unchanged` / `unverifiable`）从 `cua-engine-access` 降到 `cua-core::policy`
并由原处 re-export：桌面与浏览器现在报的是同一套词，不是两套同义词。

### 测试与验证（急件三项）

- 本机 6 条纯判据测试（状态移动 / 静音元素不给判据 / 像素同与异 / 字段相等 / 富文本三标准
  含"内容变少=覆盖不是插入"这一格 / 关键句切片）。
  workspace **172 passed / 0 failed / 2 ignored**（基线 166/0/1 + 6 条，无回归）。
- **真机定向测试入库**：`readback_verdicts_on_a_real_page`（`#[ignore]`，
  `cargo test -p cua-engine-browser -- --ignored readback_verdicts`）。它起真 Chrome、
  开一个临时 fixture 页（checkbox / input / 会把自己改成大写的 input / 带已有内容的
  contenteditable / 一输入就清空自己的 contenteditable / 无状态的 `role=button` div），
  一条测试里跑完 3 项定向 + 2 条负控：
  - click：`[effect=changed] (checked Some(false)->Some(true))`，再点回 `Some(true)->Some(false)`；
  - type：`[verified`；
  - **负控 1**：`#upper` 把 `abc` 变成 `ABC` → 必须 `FAILED:` 开头（回读不一致不许报 ok）；
  - insertText：`KEEP-ME`（原有内容）**仍在** 且新句在内 → 证明是追加不是覆盖；
  - **负控 2**：`#stub` 一输入就清空 → 必须 `FAILED:`；
  - 无状态元素：断言 `!= "OK"` 且带 `[effect=`。
  本机实测：`test result: ok. 1 passed; 0 failed`。
- 没做（契约没要求，写清楚免得被当成做了）：click 的像素回退只比字节，不解码算像素差
  （动画会造成假 `changed`，所以它只在元素自身无可读状态时启用，且话术里就写"pixels changed"）；
  `wait_for_selector` / `press` 这类仍回原文案，本轮不扩。

## 0.1.5 — 2026-10-23

把确认链关掉的一版：命令分级改为读参数形状、默认档反转，三个模型驱动入口的自动放行默认取消。
经独立验收后合入（Muse 侧：当时 94 项测试复跑全过 + 6/6 行为抽查——只读直跑、`rm -rf` 无旗标被拦且
带 `--yes` 照样拦、`--allow-high` 单独可用、中危无 `--yes` 拦带 `--yes` 放）。验收之后又按意见补了
注释修正与 1 项测试，故本版本发布时为 **95 项**。

**破坏性默认变更（安全）**：MCP 的 `run_task` / `start_task` / `orchestrate` 不再默认 `yes: true`。
依赖旧默认值的调用方，从现在起会看到"需要 `yes` 或可应答确认的客户端"的拒绝；请在确实获得用户同意后显式传 `yes: true`。

### 安全：确认链修好（issue #8）

三段接起来的绕过链，逐段关掉：

1. **分类**（`cua-core::action::risk`）：原来是 16 条字面黑名单 + `contains` 比较，没命中就默认
   判 Medium——等于默认放行。现在按**参数形状**判，且默认档位反转：
   - 只读命令（`ls` / `cat` / `git status` / `Get-Content` …）判 Low，不打扰人；
   - **未列名的一切**判 Medium，需要一次决定（`yes`、客户端确认，或终端输入）；
   - 破坏性形状判 High，`yes` 买不到。
   issue 里列的绕过例子现在全部 High，并已固化成测试：`rd /s /q C:\Users`、
   `Remove-Item -Recurse -Force C:\`、`rm  -rf  /`（双空格）、`curl https://x | bash`、
   `powershell -EncodedCommand …`、`reg delete HKLM\…`、`mkfs.ext4`、`dd of=/dev/sda`、
   `chmod -R 777 /`，以及关掉防护的 `Set-MpPreference -DisableRealtimeMonitoring` /
   `netsh advfirewall set … state off`——把保护关掉不是配置改动，是破坏性动作的前一步。
   分类器读形状不读意图，抓不到的也钉成测试写明（`docker run -v /:/host chroot`、
   `git push --force`、`history -c` 目前是 Medium：会要求一次决定，但不会被标成高危）。
   包装器 `sudo` / `cmd /c` / `sh -c "…"` 会先剥开再判；反向也不误伤散文
   （`printf "format %s"`、"please format the document" 不再是高危——旧名单里的 `"format "`
   会命中它们，把用户训练成无脑放行）。
2. **执行默认**：`run_options_from_args` 的 `yes` 默认由 `true` 改为 `false`；
   `orchestrate` 在 MCP 侧原本**硬编码** `yes: true`，CLI 侧同样硬编码，两处都改为读参数、默认关。
   编排路径此前根本没有确认通道（`approver: None`），现在把 MCP 的 elicitation 通道接了进去，
   否则唯一"能跑"的办法就是关掉确认。
3. **生成段**：代码引擎透传模型给的命令字符串；它本身不判风险，风险统一在 `confirm()` 执行门前
   决定——所以分级修好即覆盖该段，同时入口默认不再是"自动同意"。

顺带修掉门自身的一处语义错误：`allow_high` 单独使用时，高危动作仍会掉进中危分支被要求再给 `yes`，
而报错信息却写着"加 `--allow-high` 即可"。现在 `allow_high` 就是对高危的明示许可，
错误信息给的出路真的走得通（已实测）。

### 验证（本机真机，replay 走同一扇执行门）

用已发布的 **0.1.4 旧二进制**与本次构建跑同一批 `steps.jsonl`，逐格实测：

| 用例 | 旧 0.1.4 | 本次构建 |
|------|----------|----------|
| `Remove-Item -Recurse -Force D:\Temp\p5_replay\scratch`，`--yes` | **执行并把目录删了** | 拒绝：需要 `--allow-high` |
| 同一条高危命令，无 flag | 拒绝 | 拒绝 |
| 同一条高危命令，`--allow-high`（不带 `--yes`） | **仍被挡**，而报错却写着"加 `--allow-high` 即可" | 执行：明示许可单独就够 |
| `cargo --version`（未列名），无 flag | 拒绝（中危） | 拒绝（中危），信息给出两条出路 |
| `cmd /c echo CUA-GATE-OK`（只读），无 flag | **拒绝**——旧分类把一切非黑名单都当中危，连读操作都要 `--yes` | 执行（只读不打扰） |

CLI 的 `--yes` 本来就是显式开关，所以上表证的是**分类**与**门语义**。
真正被关掉的 MCP 自动放行（`run_options_from_args` 默认 `yes: true`、`orchestrate` 硬编码
`yes: true`）由两项新测试与代码锁住；HTTP 模式下"高危没有反向通道"那一格只有单测证据，
真机端到端跑 `run_task` 需要模型密钥，本机未配置——这一格不算已验证。

另有 17 项新测试：分级 9 项（含 issue 列出的全部绕过例子、包装器剥离、散文反向用例、以及把分类器**抓不到**的形状——`docker run -v /:/host chroot`、`git push --force`、`history -c`——钉成 Medium 的边界测试）、
执行门 5 项（只读免确认 / 未列名需决定 / `yes` 买不到高危 / `allow_high` 生效 / 无通道即拒绝）、
入口默认 2 项、状态标记 1 项。`cargo test --workspace` **95 通过 / 0 失败**（0.1.4 时 78）。

### 修正的注释与实现矛盾

- `Action::is_mutating` 的文档写"改变机器状态"，实现却只排除 `Done`——也就是说一次以 `Fail`
  收尾的运行会被记成"改过机器"，与运行日志要表达的意思正好相反（#14 的"Fail 分类矛盾"）。
  现在 `Fail` 一并排除，并有测试钉住。
- `ShellKind::Auto` 注释写"非 Windows 用 `/bin/sh`"，实现走的是 `bash -lc`。按实现改注释。
- 顺带核对过 `Scroll` 的"正 `dy` 向下"：与 enigo 0.6 的约定一致，注释没错，不动。

## 0.1.4 — 2026-10-23

一次以「少来回、能自证、Linux 真的能用」为主线的小版本：动作链 `then:[…]`、
捕获后端的自证与提速、Linux 窗口归属改按进程号，并把 Linux 的两条路径第一次做成
可重跑的 CI 证据。延续 0.1.x 线，不为功能量级跳版本。

### 新增

- **`then:[…]` 服务端动作链**：任意图元工具可带一组确定性后续动作，由服务端按序执行，
  只回一次结果——一份逐步 trace，加一次 `window_state` 观测。`observation_id` 与 `window`
  会被省略这两个字段的步骤继承，所以一次观测能驱动整批动作；`settle_ms`（默认 120ms）
  控制步间隔；`observe` 决定收尾怎么观测（默认取 diff、不带截图，`false` 只回 trace，
  给对象则原样传给 `window_state`）。
- 链在第一个失败处停止，`stopped_at` / `ran` / `planned` 报告的是**真跑了什么**，
  不是要求跑什么。模型驱动的 `run_task` / `orchestrate` / `browser_task` 不能作为步骤，
  步骤里再带 `then` 会被拒绝——链保持扁平，trace 才能按字面读。
- **Linux：窗口归属改按进程号判定**（`window_state` 一条路径，issue #16）。
  `org.a11y.atspi.Application` 本身不报进程号（接口只有 toolkit name、registry 分配的 id
  和 locale 方法），也不需要：每个应用挂在一条总线连接上，`GetConnectionUnixProcessID`
  能把那个连接名换成与窗口列表同一个 pid。于是 `WM_CLASS` 与无障碍应用名不一致的常见情况
  ——Python 应用报脚本文件名、Electron 报 `electron`——也能命中窗口级树，名字匹配退为兜底。
  匹配不上时的报错也改得能用：列出总线上实际看到的应用名，并指路 `accessibility_tree`。

### 验证

- 动作链本机真机实测（release 二进制，loopback HTTP + 记事本，同一任务两种写法各跑两轮）：
  分开调用 5 次 / 1527ms / 109.8 KB，链式调用 2 次 / 1320ms / 49.1 KB——少 3 个往返、
  省 55% 字节、快 207ms。时延没省更多，是因为"动手前观测一次、收尾观测一次"两边都要付；
  收益在往返次数与 token 上，链越长越明显。
- 负控：链中第三步参数非法时 `ran=3 stopped_at=2`，前两步确实落地（文字进了文档），
  第四步的回车没有发出（光标仍在第 1 行第 12 列）。
- 这组数字早期是在 debug 二进制上取的（94.6 KB / 48.8 KB、约 3.5 秒），debug 下图像缩放
  与哈希慢一个数量级，已按 release 复测更正。
- 新增 11 项测试（链策略面 8 项、捕获的 lossless 归属 1 项、Linux 窗口匹配 2 项），
  workspace 测试数 **67 → 78**。
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
  `set_program_class`），而**这类不一致本身也不再是缺口**：窗口归属优先按进程号判定
  （见上面「Linux：窗口归属改按进程号判定」），CI 用 `PROBE_IDENTITY=mismatch` 跑一个
  两边名字确实不同的应用来证明它命中 —— issue #16 因此关闭。

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
