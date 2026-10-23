#!/usr/bin/env python3
"""Verify the Linux AT-SPI backend against a real graphical session.

Nothing here is mocked: the checks drive the MCP server over HTTP against an
Xvfb display, a live GNOME accessibility bus and a GTK3 window, and the oracle
for the two action paths is the probe app itself — text set through
EditableText is read back only after a DoAction fired the submit button, so a
report of success without that file is a failure.

Run inside a session bus with DISPLAY set:

    Xvfb :99 -screen 0 1280x800x24 &
    DISPLAY=:99 dbus-run-session -- python3 scripts/linux-atspi-runtime.py
"""

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("CUA_HUB_BIN", ROOT / "target" / "release" / "cua-hub"))
PORT = int(os.environ.get("CUA_HUB_PORT", "8799"))
BASE = f"http://127.0.0.1:{PORT}/mcp"
EVIDENCE = Path(os.environ.get("ATSPI_EVIDENCE", ROOT / "atspi-evidence"))
PROBE_APP = "cua-atspi-probe"
PROBE_TITLE = "cua-atspi-probe"

rpc_id = 0
failures: list[str] = []


def call(tool: str, arguments: dict) -> dict:
    """One MCP tools/call, returned as the parsed JSON the server sent back."""
    global rpc_id
    rpc_id += 1
    body = {
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    }
    request = urllib.request.Request(
        BASE,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    started = time.time()
    with urllib.request.urlopen(request, timeout=60) as response:
        reply = json.loads(response.read())
    elapsed_ms = int((time.time() - started) * 1000)
    parts = reply.get("result", {}).get("content", [])
    texts = [p.get("text", "") for p in parts if p.get("type") == "text"]
    images = [p for p in parts if p.get("type") == "image"]
    parsed = None
    for text in texts:
        try:
            parsed = json.loads(text)
            break
        except (json.JSONDecodeError, TypeError):
            continue
    record = {
        "tool": tool,
        "arguments": {k: v for k, v in arguments.items() if k != "then"},
        "ms": elapsed_ms,
        "is_error": bool(reply.get("result", {}).get("isError")),
        "text": "\n".join(texts)[:4000],
        "images": len(images),
        "json": parsed,
    }
    (EVIDENCE / f"{rpc_id:02d}-{tool}.json").write_text(json.dumps(record, indent=2))
    if images:
        import base64

        (EVIDENCE / f"{rpc_id:02d}-{tool}.jpg").write_bytes(base64.b64decode(images[0]["data"]))
    return record


def check(label: str, ok: bool, detail: str = "") -> None:
    print(("  PASS  " if ok else "  FAIL  ") + label + (f"  [{detail}]" if detail else ""))
    if not ok:
        failures.append(label)


def wait_for_server(seconds: float) -> bool:
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=2) as probe:
                if probe.read().strip() == b"ok":
                    return True
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            time.sleep(0.4)
    return False


def main() -> int:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    result_file = EVIDENCE / "probe_result.txt"
    if result_file.exists():
        result_file.unlink()

    print(f"== starting {BIN} on port {PORT}")
    server = subprocess.Popen(
        [str(BIN), "serve", "--http", f"127.0.0.1:{PORT}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    # The probe's own output is the difference between "the backend cannot see a
    # window" and "there was never a window to see", so it goes to a file the
    # failure path can print rather than into a pipe nobody drains.
    probe_log = open(EVIDENCE / "probe.log", "w", encoding="utf-8")
    probe = subprocess.Popen(
        [sys.executable, str(ROOT / "tests" / "linux_atspi" / "probe_gtk.py"), str(result_file)],
        stdout=probe_log,
        stderr=subprocess.STDOUT,
    )
    try:
        if not wait_for_server(30):
            print("the MCP server never answered /health")
            return 1
        print("  server up")

        # --- the window list sees the app -----------------------------------
        window_id = None
        deadline = time.time() + 40
        while time.time() < deadline and window_id is None:
            listed = call("list_windows", {"query": PROBE_TITLE})
            windows = (listed["json"] or {}).get("windows", [])
            if windows:
                window_id = windows[0]["id"]
            else:
                time.sleep(1.0)
        if window_id is None:
            probe_log.flush()
            print("--- the probe application said ---")
            print(Path(probe_log.name).read_text(encoding="utf-8", errors="replace")[:2000])
        check("list_windows sees the GTK window", window_id is not None, f"id={window_id}")
        if window_id is None:
            return 1

        # --- the window-level AT-SPI tree -----------------------------------
        state = call("window_state", {"window": window_id, "include_text": True, "max_nodes": 80})
        tree = ((state["json"] or {}).get("accessibility") or {})
        body = tree.get("tree") or tree.get("diff") or ""
        node_count = tree.get("node_count", 0)
        check(
            "window_state returns a window-level accessibility tree",
            node_count >= 4 and body != "",
            f"nodes={node_count} mode={tree.get('mode')}",
        )
        for control in ("probe-entry", "probe-submit", "probe-toggle"):
            check(f"tree contains {control!r}", control in body)
        check(
            "the tree is scoped to this window, not the whole desktop",
            "type into the entry below" in body,
            "the label only exists inside the probe frame",
        )
        # The contract being checked is that a missing image is announced, not
        # that Xvfb can always produce one: window_state must not silently drop
        # the half of the observation that failed.
        captured = state["images"] >= 1
        announced = bool((state["json"] or {}).get("screenshot_error"))
        check(
            "the observation reports its screenshot or says why it cannot",
            captured or announced,
            f"images={state['images']} announced={announced}",
        )

        # --- write through EditableText, then fire the action ---------------
        written = call("set_control_text", {"name": "probe-entry", "text": "AT-SPI-WRITES-HERE"})
        check("set_control_text reaches the entry", not written["is_error"], written["text"][:120])

        invoked = call("invoke_control", {"name": "probe-submit"})
        check("invoke_control fires DoAction on the button", not invoked["is_error"], invoked["text"][:160])

        try:
            probe.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pass
        landed = result_file.read_text() if result_file.exists() else ""
        check(
            "the text set through accessibility really reached the control",
            landed == "AT-SPI-WRITES-HERE",
            f"probe app reported {landed!r}",
        )

        gone = call("list_windows", {"query": PROBE_TITLE})
        check(
            "the window is gone after the invoked action",
            (gone["json"] or {}).get("count", 1) == 0,
            f"count={((gone['json'] or {}).get('count'))}",
        )
    finally:
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
        if probe.poll() is None:
            probe.kill()
        probe_log.close()

    print(f"\n{len(failures)} failed check(s); evidence in {EVIDENCE}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
