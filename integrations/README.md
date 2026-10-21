# 把 cua-hub 安装为 Qoder Skill + MCP

## 1. 构建可执行文件

```powershell
cd <仓库根目录>
cargo build --release
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\cua-hub.exe dist\cua-hub.exe -Force
```

也可以直接跑 `scripts/install-qoder.ps1`（构建、复制、装 Skill、写 MCP 配置一条龙）。

## 2. 安装 Skill

```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\.qoder\skills\cua-hub" | Out-Null
Copy-Item integrations\skill\SKILL.md "$env:USERPROFILE\.qoder\skills\cua-hub\SKILL.md" -Force
```

Claude Code 用户可复制到 `~/.claude/skills/cua-hub/SKILL.md`。

## 3. 注册 MCP（stdio）

`~/.qoder/mcp.json`：

```json
{
  "mcpServers": {
    "cua-hub": {
      "command": "C:\\\\path\\\\to\\\\cua-hub\\\\dist\\\\cua-hub.exe",
      "args": ["serve"],
      "env": {
        "CUA_HUB_MODEL": "minimax/minimax-m3.1-flash:free"
      }
    }
  }
}
```

模型密钥请放在用户环境变量里（`OPENROUTER_API_KEY` / `OPENAI_API_KEY` /
`ANTHROPIC_API_KEY`），MCP 子进程会继承，不要把密钥写进仓库或配置文件。

改完配置后重启 Qoder（或重载 MCP），新会话里应能看到 `cua-hub` 的工具。

## 4. 另一种接法：HTTP

```powershell
dist\cua-hub.exe console --bind 127.0.0.1:8787
```

- `http://127.0.0.1:8787/` 是实时画面 + 任务面板
- `http://127.0.0.1:8787/mcp` 是 MCP HTTP 端点（Agent 连这个）
- 设置 `CUA_HUB_MCP_TOKEN` 后，页面与 `/mcp` 都要带 `?token=` 或 Bearer
