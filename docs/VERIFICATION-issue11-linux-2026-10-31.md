# #11 第 6/7/8 项 · 真机 A/B 验证方案（静态准备，2026-10-31）

- **给谁**：有 Debian 13 + Xfce 真图形会话的会话/云机队。实现侧（Windows）与 CI runner 均无法
  替代本文件所列内容，这是 #11 关单前最后一道实证。
- **权威**：issue #11 正文八项清单；实现状态见 PR #28（第 6 项）、#29（第 7 项）、#30+#32（第 8 项）
  的 body——本文件只做"怎么在真机上验"，不重复那里的代码论证。
- **定位**：静态准备（HANDOFF-2026-10-31 §2 的分工）。本文档不含代码变更；执行中发现的
  行为偏差回 #11 挂账，不要就地改代码。

## 0. CI 图形作业已证清单（真机**不要重复做**）

以下断言已在 CI 图形作业绿过，真机只验"真实桌面 vs 夹具"的增量：

| 项 | CI 已证 | 证据 run |
|---|---|---|
| 6 超时+账本 | hung 场景 5 行 PASS（含 `[elapsed=1.2s]` 期限、`names the part it could not read`、`reports nothing unreadable`） | `40118967046` |
| 7 capped 预算 | aligned/mismatch 两场景各一对 PASS（capped 拒、默认预算成） | `40144890504`（红：`40141547340`） |
| 8 效果三档 | aligned 两场景各 4 行 PASS（`effect=changed (toggle=on)`、`menu_item_in_tree=True` 对账、`probe-submit`→`unverifiable`、写字段→`unverifiable (this platform cannot read the field's text back)`） | `40147819661`（红：`40147535682`） |

CI 环境是 Xvfb + openbox + at-spi2 + GTK 夹具（`scripts/linux-atspi-runtime.py`，
`workflow_dispatch` 可随时重跑，产物 `linux-atspi-evidence`）。

## 1. 环境前置

1. Debian 13 + Xfce **真桌面会话**（登录进 Xfce，不是 Xvfb 虚拟会话）。
2. at-spi2 栈在位：`at-spi2-core` 已装、Xfce 的辅助技术支持已启用
   （`gsettings set org.gnome.desktop.interface toolkit-accessibility true`，注销重登生效）。
3. 被测二进制：从待验收分支构建（各 PR body 的"验收配方"有确切步骤与期望测试数：
   #28→126、#29→129、#30→139 passed / 1 ignored）。**同一二进制跑完三组实验**，
   不要中途换构建。
4. 基线（A 侧）：可用同一台机在**修复前 commit** 上重跑关键步骤做 A/B；没有条件时
   用各 PR body 里引用的红 run 日志做 A 侧替代（引用处已注明 run id）。

## 2. 实验 1（第 6 项）：真实桌面上装死应用不钉死、不静默

**目标**：把"遍历无超时 + 全静默吞错"的验证从 CI 小夹具搬到真实多应用桌面。

**步骤**：

1. 正常桌面先记基线：`accessibility_tree` 全量一次——**期望**：树完整，文本尾部
   **没有** `! parts of this tree were not read:` 段；`window_state` 的 `accessibility`
   里**没有** `unreadable` 键。
2. 挑一个真实应用（如 `xfce4-terminal`），`kill -STOP <pid>` 装死。
3. 立即再跑 `accessibility_tree` / `window_state`——**期望**：
   - 调用在**约 1.2s 量级**返回（不是 25s/调用地把工具钉死）；
   - 树照常返回（其余应用的节点都在）；
   - 文本尾部出现 `! parts of this tree were not read:`，点名装死应用的 bus/路径
     与哪几个属性（name/role/children/extents）没答；`window_state.accessibility`
     条件性出现 `unreadable` 键。
4. 同一装死目标连续跑 3 次 `accessibility_tree`——**期望**：三次都被期限兜住且
   不劣化（WEDGED_COOLDOWN：每个装死应用每轮最多挂一条线程）。
5. `kill -CONT <pid>` 复原，再跑一次——**期望**：装死期间挂起的读恢复，账本消失，
   树回到第 1 步的基线。

**负控**（判"绿是真的"）：在 CI probe 里已实测过的两条可照搬——把 `timed()` 的
`recv_timeout(CALL_TIMEOUT)` 换成无限 `recv()`，hung 断言红；把 `unanswered()` 里的
`"unknownmethod"` 删掉，账本断言红（PR #28 body 附原文）。真机上不必重做，判读时
知道"红长什么样"即可。

**判读**：任一期望不成立（钉死、静默少节点、账本点名错对象、CONT 后不恢复）→
回 #11 贴 JSON + 截图 + 时间戳，第 6 项不关。

## 3. 实验 2（第 7 项）：真实大桌面下的预算参数

**目标**：CI 夹具只有小树；真机要验的是**真实大树**下 capped 参数生效、默认 600/10
够用、截断报 `Err` 而不是 `None` 误读。

**步骤**：

1. 把桌面铺大：开文件管理器（多标签）、面板、桌面图标层，`accessibility_tree`
   记录总节点数（应显著大于 CI 夹具）。
2. 默认预算正路：`invoke_control` 一个深层目标（如文件管理器菜单项）——**期望**：
   正常执行，`ActionReport` 带裁决，不因默认预算误伤。
3. capped 反路：同一目标用 `max_nodes=1` / `max_depth=1` 调 `invoke_control` /
   `set_control_text`——**期望**：`Err`，错误内容包含"走了多少节点/深度、
   `max_nodes` 多少、树仍截断"，且**以 `accessibility lookup failed:` 开头**、
   **不含** `no such pattern` 字样（文案钉子，PR #29 `5321df7`）。
4. capped 后紧跟一次默认预算成功调用（PR #29 刻意的红绿归因对）——**期望**：成功。
   这一对必须成对出现在证据里。

**判读**：`Ok(None)` 若在任何"没走到"的场景出现 → 即第 7 项要防的误读复活，回 #11。
`find_control` 的 400/8 写死是**已知边界**（PR #29"已知边界 1"，ROADMAP 在案），
真机遇到不算本项失败，单独挂账即可。

## 4. 实验 3（第 8 项）：`unchanged` 的真机样本（本方案的核心增量）

**背景**：`effect` 三档中，`changed`/`unverifiable` 两档 CI 与 Windows 真机都有样本；
**`unchanged` 两平台都没有真机样本**（PR #30"已知边界 6"）——CI 的 GTK `MenuButton`
夹具被 `DoAction` 真的开了菜单（→`changed`），不复现审计在 Xfce 面板撞到的形状。
唯一诚实的补样本路径就是真实 Xfce 面板控件。

**步骤**：

1. `window_state` 定位 Xfce 面板的 Applications 按钮（审计时矩形约 (0,0,102,26)，
   真机以实际树为准），记录其 role 与状态位。
2. `capture_screen` 记基线图。
3. `invoke_control` 该按钮——**期望与判读**（两种都是有效结果，如实记录即可）：
   - 若控件**发布** toggle/expanded 且菜单未开 → `effect=unchanged`（这才补上
     "答了但没动"的真机样本；期望同时附 `toggle=`/`expanded=` 读回值）；
   - 若控件**没有**可读状态位 → `effect=unverifiable`，此时**不得**报 `unchanged`
     （"普通按钮不得冒充 unchanged"是本裁决的红线之一）；
   - 无论哪档，**不得**出现旧的无条件 `invoked ... via accessibility pattern` 无裁决答复。
4. `capture_screen` 对比：菜单是否真的开了。菜单若真开了而裁决不是 `changed` →
   裁决错误，回 #11。
5. 对照组（CI 已证，真机复证一遍即可）：checkbox 翻转 → `effect=changed (toggle=on)`；
   无状态位普通按钮 → `effect=unverifiable (no readable state on this control)`。
6. `set_control_text` 对照：Linux `value` 未接 Text 接口，写字段应诚实报
   `unverifiable (this platform cannot read the field's text back)`，**不**拿
   `SetTextContents` 的 true 冒充写成功。

**判读**：拿到 `unchanged` 样本 → 第 8 项三档全有真机证据，可关；拿不到（面板控件
根本不发布状态位）→ 如实记录"语义已定义、真机未观测"（现状口径），第 8 项凭
`changed`/`unverifiable` 两档真机样本 + 单测钉住的语义判收，边界写回 #11。

## 5. 证据归档

- 每次 MCP 调用的原始响应（JSON）+ 关键时刻截图，按 `linux-atspi-evidence`
  artifact 的目录习惯打包：一次实验一个目录，`metadata.json`（commit SHA、二进制
  `--version`、日期、会话标识）+ 分步 JSON/截图。
- PASS/FAIL 行照 CI probe 格式原文贴（那是 #28/#29/#30 的对账口径）。
- 完成后把三组结果以一条评论挂 #11（引用本文件路径与各实验小节号），
  由验收方复核对账；实现侧不自行改代码。

## 6. 明确不在本方案范围

- `find_control` 400/8 写死的口径（ROADMAP 在案，独立定契约）。
- `atspi.rs` 其余站点仍走 `Error::Capture` 的改名清扫（PR #29"没有顺手做完的部分"）。
- 性能对比（#30"已知边界 2"：要量就同机、release、交错 A/B，不属本验证）。
- zbus 6.x 异步化重构（dependabot 忽略中，排 #11 关单之后）。
