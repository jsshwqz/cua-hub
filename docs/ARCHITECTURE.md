# 架构设计

## 分层

```
任务（自然语言）
  │
  ▼
cua-orchestrator ── 循环：观测 → 规划 → 执行 → 再观测
  │            │
  ▼            ▼
cua-brain    cua-runtime
（想）        （做）
Brain trait   截屏 / 键鼠 / 安全围栏 / 审计
  │
  ├─ MockBrain（测试用脚本大脑）
  ├─ Anthropic Computer Use   （Phase 2）
  ├─ OpenAI CUA               （Phase 2）
  ├─ Gemini computer use      （Phase 2）
  └─ UI-TARS（本地/远端推理）  （有 GPU 后）
```

## 动作协议（cua-types::Action）

所有大脑只允许输出这组动作，执行层只认这组动作 —— 这是"大集成"能成立的关键：
换大脑不换手，换手不换大脑。

`Screenshot / MouseMove / Click / DoubleClick / Scroll / Drag / TypeText / KeyPress / Wait / LaunchApp`

每个动作带安全等级：

- `Safe`：Screenshot、MouseMove、Wait —— 无副作用
- `Input`：Click、TypeText、KeyPress 等 —— 改变外部状态
- `Risky`：LaunchApp —— 拉起任意程序，必须经安全围栏

## 安全围栏（cua-runtime::guard）

执行任何动作前先过 `Policy::check`：

- `DryRun`：全部只记录，不执行
- `Supervised`：Safe 直接放行；Input/Risky 逐个向 Confirmer 确认
- `Autonomous`：全部放行（仅建议在沙箱 VM 内使用）

无论哪种模式，每个动作的判定与结果都写入审计日志（JSONL），事后可完整回放。

## 平台策略

Phase 1 只做 **Linux/X11**（项目主战场是云端 VM 的 XFCE 桌面）：

- 截屏：`x11rb` 直连 X11 取根窗口图像（纯 Rust、无额外系统库依赖，构建可复现）
- 键鼠：`enigo`（跨平台；其 Linux 后端走 X11/XTEST，Windows/macOS 后端现成）

后续平台扩展点：截屏后端抽成 `CaptureBackend` 时再引入 `xcap` 覆盖
Windows/macOS；动作协议与上层不变。

> 与原始规划的偏差说明：规划初稿写截屏用 `xcap`。实作时改为 x11rb，
> 原因是沙箱环境里 xcap 的系统库依赖（libxcb-image 等）不齐且版本 API 漂移大，
> x11rb 构建确定性更好。接口边界已预留，Phase 5 打包时评估切回。

## 数据流一例

```
$ cua-cli exec --file actions.json --mode autonomous
  → 解析 Vec<Action>
  → 逐个：Policy::check → Runtime::execute → AuditLog::append
  → Screenshot 动作把 PNG 落盘并在日志里记录路径与尺寸
```
