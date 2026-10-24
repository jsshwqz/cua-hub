# 变更日志

本文件记录 cua-hub 的可见行为变化。版本号遵循语义化版本；0.x 阶段以发布节奏为主，
不为了体现功能量级而跳版本。发版（tag / GitHub release）需要维护者明确点头，
合入 main 不等于发版。

## Unreleased — 已合入分支，尚未发版

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
- **端到端红/绿排在 CI 图形作业里**：新阶段起两个 probe，`SIGSTOP` 其中一个，再取桌面树，
  三条断言——① 活着那个应用仍在树里（不是整次失败）；② 输出点名读不到的部分；
  ③ 整体耗时 <10s。提交 `ac75429` 只有断言，应当红在 ②③。
  **①在修复前就是通的**，这不是我写错的断言，而是这个 bug 的形状：观测"成功"返回，
  只是静默少了东西。

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
