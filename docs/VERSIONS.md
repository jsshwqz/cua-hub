# 仓库内的版本对照

当前仓库有三个并行版本（分支），定位不同：

| 版本 | 位置 | 定位 | 主要特征 |
|------|------|------|----------|
| **cua-hub** | `main` | 主力实现（持续维护） | 五引擎（Vision/Code/Access/Browser/Code-Exec）、跨引擎编排、失败重试/重规划、MCP 22 工具（stdio + HTTP）、回放、Windows/Linux CI |
| **cua-mcp** | `muse-vm-phase0-1` | 独立谱系的运行时实现 | 常驻剪贴板、鼠标/按键按住与释放、screen_info、坐标越界校验、DryRun/Supervised/Autonomous 围栏、cua-web 与 cua-bench 骨架、Windows release 流水线 |
| **CUA-Forge** | `gemini-cua-forge` 的 `cua-forge/` | Gemini 生成的单文件迷你实现 | xcap + enigo，HTTP `:3030` 与自定义 stdio（非标准 MCP）；仅 move/click/type |

## 取长补短记录

2026-10-21 从 `muse-vm-phase0-1` 并入 `main`：

- `clipboard_get` / `clipboard_set`（arboard 常驻实例，解决 X11 内容随进程消失）
- `mouse_down` / `mouse_up`、`key_down` / `key_up`（共享 Enigo 实例，支持 Shift 多选、Ctrl 拖拽）
- `screen_info`（主屏尺寸、原点、DPI、光标位置）
- 坐标越界校验：指向类动作在执行前拒绝屏幕外坐标

未并入：

- `gemini-cua-forge` 的非标准 stdio 协议（与 MCP 不兼容，没有集成价值）
- `muse` 的 `cua-web` / `cua-bench`（仅骨架，尚未实现）
- `muse` 的三档运行模式（`main` 使用更细的风险分级 + `--allow-high`，语义等价且更严格）

## 选择建议

- 需要完整能力、跨平台与持续维护：用 `main`
- 只想要一个更小的 MCP 运行时、且偏好其围栏模型：参考 `muse-vm-phase0-1`
- 学习最小实现：看 `gemini-cua-forge`，不建议直接部署
