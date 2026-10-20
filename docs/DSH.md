# cua-mcp 接入 DeepSeek Harness（dsh）

dsh 通过官方插件 `@deepseek-ai/dsh-mcp-client` 桥接 MCP：
一个插件实例 = 一个 MCP server，挂在 profile 的 cordis patch 里。
本 server 已用官方 TS 版 MCP SDK 实测互通（连接、工具发现、截图图块、
点击/按键执行全部通过）——dsh 的客户端正是 TS 实现。

依据：官方文档《MCP Integration》
https://deepseekdocs.com/en/docs/features/mcp

## 步骤

1. 下载 Release 里的 `cua-mcp-windows-x64.zip`，解压到 `C:\cua-mcp`。
2. 先启动一次 `dsh web` 让它生成配置目录，然后关掉。
3. 打开 web profile 的 patch 文件：
   `C:\Users\<用户名>\.dsh\profiles\web\cordis.patch.yml`
   （macOS/Linux 为 `~/.dsh/profiles/web/cordis.patch.yml`）
4. 把压缩包里的 `dsh-cordis.patch.yml` 内容并入该文件。
   注意：新条目必须挂在 `insert:` 段下；文件里若已有 `insert:`，
   只把 `mcp-cua` 一项加进它已有的列表，不要写第二个 `insert:` 键。
5. 重启 `dsh web`，新开会话使用。

## 验证

```powershell
dsh web --dump-config | Select-String mcp
```

能看到 `dsh-mcp-client` 实例即挂载成功。会话里工具名为
`mcp__cua__screenshot`、`mcp__cua__click` 等（serverName=cua）。
让 agent 先调用 `mcp__cua__screenshot`：能回出屏幕图片即全线打通。

## 已知边界（都出自 dsh 官方文档）

- **图片回传有前提**：截图以 MCP image 内容返回；dsh 侧需要
  `ctx.attachments` 已挂载、且当前模型路由声明支持图片输入，
  图片才会进入模型上下文。看不到图先查这两处，不是 server 的问题。
- **单次工具调用默认超时 60 秒**：示例配置已把 `toolCallTimeoutMs`
  提到 120000；server 的 `wait` 工具单次封顶 50 秒，超时分多次调用。
- **启动失败默认不拦 dsh**：`failOnStartupError` 默认 false，
  exe 路径写错时工具会静默缺席。会话里找不到 `mcp__cua__*`
  就先用上面的 dump-config 命令查挂载，再查路径。
- 有第三方实测称内置 MCP 客户端在 npx 安装版下可能不可用、
  需要源码构建版（`pnpm dsh`）。若按本文挂载后始终看不到工具，
  优先换源码版验证，不要怀疑 server 或配置本身。
