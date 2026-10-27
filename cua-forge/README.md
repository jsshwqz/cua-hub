# CUA-Forge（第三方实现收录）

本目录是一套独立的 Computer Use 实现，由 Gemini 生成、仓库所有者于
2026-10-20 提交收录，**代码按原样发布，未经修改**。同目录的
`build_package.py` 是它的单文件生成器（运行后会吐出本目录的工程文件）。

内容：
- `Cargo.toml` / `src/main.rs`：单 crate 工程 `cua-forge-core`，
  xcap 截屏（JPEG/base64）+ enigo 键鼠，双接口：HTTP API（axum，:3030）
  与一种 stdio JSON-RPC 模式
- `.github/workflows/build.yml`：Windows EXE 构建流水线

收录者注记（不属原代码）：
1. 本目录内的 workflow 放在子目录里不会被 GitHub Actions 触发；
   要真构建出 `.exe`，需把本目录独立成仓，或把 workflow 移到仓库根
   `.github/workflows/` 并调整构建路径。
2. 其 stdio 模式用的是自定义方法名（`take_screenshot` / `execute_action`），
   不是标准 MCP 协议（无 `initialize` / `tools/list`），Claude Code、Qoder
   等标准 MCP 客户端无法直接接入，需要再做一层协议适配（可参照本仓
   `muse-vm-phase0-1` 分支上的 `crates/cua-mcp`）。
3. 尚未在本地完成编译验证：收录时本机（Linux VM）缺 xcap 依赖的
   dbus 开发库。其 CI 目标平台是 Windows，不受 dbus 影响。
