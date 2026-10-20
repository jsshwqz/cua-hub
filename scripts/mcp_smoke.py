#!/usr/bin/env python3
"""cua-mcp 冒烟测试：以真实 MCP 客户端流程驱动 server。

流程：initialize 握手 -> tools/list -> screenshot（校验 PNG）->
launch_app xterm -> type_text 写文件 -> key_press Return -> 校验文件落地。

用法（需已有一个 X 会话，如 Xvfb :99）：
    DISPLAY=:99 python3 scripts/mcp_smoke.py [path-to-cua-mcp-binary]

退出码 0 = 全部通过。
"""
import base64
import json
import os
import subprocess
import sys
import time

BIN = sys.argv[1] if len(sys.argv) > 1 else "target/debug/cua-mcp"
SMOKE_FILE = "/tmp/cua_mcp_smoke.txt"
AUDIT = "/tmp/cua_mcp_audit.jsonl"
SHOTS = "/tmp/cua_mcp_shots"

for p in (SMOKE_FILE, AUDIT):
    if os.path.exists(p):
        os.remove(p)

proc = subprocess.Popen(
    [BIN, "--mode", "autonomous", "--allow-launch",
     "--audit", AUDIT, "--shots", SHOTS],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL, text=True,
    env={**os.environ, "DISPLAY": os.environ.get("DISPLAY", ":99")},
)

_id = 0


def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


def request(method, params=None):
    global _id
    _id += 1
    send({"jsonrpc": "2.0", "id": _id, "method": method, "params": params or {}})
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError(f"server closed stdout waiting for {method}")
        msg = json.loads(line)
        if msg.get("id") == _id:
            if "error" in msg:
                raise RuntimeError(f"{method} -> error: {msg['error']}")
            return msg["result"]


def call_tool(name, arguments=None):
    return request("tools/call", {"name": name, "arguments": arguments or {}})


def check(label, cond):
    print(("PASS " if cond else "FAIL ") + label)
    if not cond:
        proc.kill()
        sys.exit(1)


try:
    init = request("initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "mcp-smoke", "version": "0.1"},
    })
    check("initialize: serverInfo.name == cua-mcp",
          init["serverInfo"]["name"] == "cua-mcp")
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    tools = [t["name"] for t in request("tools/list")["tools"]]
    print("tools:", ", ".join(tools))
    check("tools/list includes core tools",
          {"screenshot", "click", "type_text", "key_press", "launch_app"} <= set(tools))
    check("tools/list has all 17 tools", len(tools) == 17)

    shot = call_tool("screenshot")
    img = next(c for c in shot["content"] if c["type"] == "image")
    raw = base64.b64decode(img["data"])
    check("screenshot returns a real PNG", raw[:8] == b"\x89PNG\r\n\x1a\n" and len(raw) > 1000)
    txt = next(c["text"] for c in shot["content"] if c["type"] == "text")
    print("screenshot text:", txt)

    r = call_tool("launch_app", {"program": "xterm", "args": []})
    check("launch_app xterm executed", not r["isError"])
    call_tool("wait", {"ms": 2000})

    # 裸 Xvfb 无窗口管理器，新窗口不一定有键盘焦点；先点一下窗口再打字
    # （真实 agent 也是这么做的）。
    r = call_tool("click", {"x": 400, "y": 300})
    check("click into xterm executed", not r["isError"])
    call_tool("wait", {"ms": 300})

    r = call_tool("type_text", {"text": f"echo cua-mcp-ok > {SMOKE_FILE}"})
    check("type_text executed", not r["isError"])
    r = call_tool("key_press", {"combo": "Return"})
    check("key_press Return executed", not r["isError"])

    # v0.1.2 新增工具：屏幕信息 / 剪贴板往返 / 按下-抬起
    r = call_tool("screen_info")
    info = next(c["text"] for c in r["content"] if c["type"] == "text")
    print("screen_info:", info)
    check("screen_info reports screen + cursor",
          not r["isError"] and "screen 1280x800" in info and "cursor at" in info)

    marker = "cua-clip-12345"
    r = call_tool("clipboard_set", {"text": marker})
    check("clipboard_set executed", not r["isError"])
    r = call_tool("clipboard_get")
    got = next(c["text"] for c in r["content"] if c["type"] == "text")
    check("clipboard roundtrip", not r["isError"] and got == marker)

    for name, args in [("key_down", {"combo": "shift"}), ("key_up", {"combo": "shift"}),
                       ("mouse_down", {}), ("mouse_up", {})]:
        r = call_tool(name, args)
        check(f"{name} executed", not r["isError"])

    ok = False
    for _ in range(50):
        if os.path.exists(SMOKE_FILE) and "cua-mcp-ok" in open(SMOKE_FILE).read():
            ok = True
            break
        time.sleep(0.1)
    check("typed command actually ran in xterm (file written)", ok)
    check("audit log written", os.path.exists(AUDIT) and os.path.getsize(AUDIT) > 0)
    print("MCP SMOKE: ALL PASS")
finally:
    proc.kill()
