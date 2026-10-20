# cua-mcp：Computer Use 的 MCP server

`cua-mcp` 只做"手"，不做"脑"：它把 cua-runtime 的截屏/键鼠能力包装成
MCP（Model Context Protocol）工具，任何 MCP 客户端里的模型——Claude Code、
Qoder、Cursor 等——都能直接看屏幕、点鼠标、敲键盘。

**好处：不需要为本程序单独开通任何模型 API key。** 大脑用的是 agent
自己的模型与额度；本 server 只负责执行与安全审计。独立 API provider
接入仍然保留在规划里（见 PLAN.md Phase 2 后半段），适用于无人值守场景。

## 构建与运行

```bash
cargo build --release -p cua-mcp
DISPLAY=:0 ./target/release/cua-mcp [选项]
```

server 走 stdio 传输，一般不手动运行，而是由 MCP 客户端按配置拉起。
它需要一个 X11 会话（`DISPLAY` 环境变量）；日志写 stderr，stdout 只走协议。

## 客户端配置示例

见 `examples/mcp-server.json`。要点：

```json
{
  "mcpServers": {
    "cua": {
      "command": "/abs/path/to/cua-mcp",
      "args": ["--mode", "autonomous", "--allow-launch",
               "--audit", "/abs/path/to/audit.jsonl",
               "--shots", "/abs/path/to/screenshots"],
      "env": { "DISPLAY": ":0" }
    }
  }
}
```

Claude Code 也可以用命令注册：

```bash
claude mcp add cua -- /abs/path/to/cua-mcp --mode autonomous --allow-launch
```

## 工具清单

| 工具 | 参数 | 说明 |
|---|---|---|
| `screenshot` | — | 截屏，返回 PNG 图片内容 + 屏幕尺寸 |
| `mouse_move` | `x, y` | 移动鼠标到绝对坐标 |
| `click` | `x?, y?, button?` | 点击；坐标成对给出或都省略（当前位置） |
| `double_click` | `x, y` | 双击 |
| `scroll` | `dx?, dy?` | 滚动（dy 正数向下） |
| `drag` | `from_x, from_y, to_x, to_y` | 按住左键拖拽 |
| `type_text` | `text` | 在当前焦点输入文本 |
| `key_press` | `combo` | 按键/组合键，如 `Return`、`ctrl+l`、`alt+F4` |
| `wait` | `ms` | 等待毫秒 |
| `launch_app` | `program, args?` | 启动程序（高风险，需 `--allow-launch`） |

坐标系：绝对像素，(0,0) 为左上角。建议 agent 每步先 `screenshot` 看屏幕再行动。

## 安全模式（--mode）

所有工具调用都先经 cua-runtime 的安全围栏并写入审计日志（JSONL）。

| 模式 | 行为 |
|---|---|
| `dry-run` | 全部只审计、不执行 |
| `supervised`（默认） | Safe 动作（截屏/移动/等待）放行；Input/Risky 动作（点击/输入/启动程序）一律拒绝——stdio 被协议占用，无法做交互确认，所以默认从严 |
| `autonomous` | 全部放行（仅应在沙箱/虚拟机等隔离环境使用）；`launch_app` 还必须额外加 `--allow-launch` 才解锁 |

## 冒烟测试

```bash
Xvfb :99 -screen 0 1280x800x24 &          # 若还没有 X 会话
DISPLAY=:99 python3 scripts/mcp_smoke.py  # 真实 MCP 客户端全流程验证
```

覆盖：initialize 握手、tools/list、screenshot 返回真 PNG、
launch_app 开 xterm、点击聚焦、type_text + Return 真实执行命令并落盘、
审计日志生成。全部通过输出 `MCP SMOKE: ALL PASS`。
