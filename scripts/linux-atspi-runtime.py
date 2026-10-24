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
import signal
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
TAG = "aligned"


def call(tool: str, arguments: dict, timeout: int = 60) -> dict:
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
    with urllib.request.urlopen(request, timeout=timeout) as response:
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
    (EVIDENCE / f"{rpc_id:02d}-{TAG}-{tool}.json").write_text(json.dumps(record, indent=2))
    if images:
        import base64

        (EVIDENCE / f"{rpc_id:02d}-{TAG}-{tool}.jpg").write_bytes(base64.b64decode(images[0]["data"]))
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

    print(f"== starting {BIN} on port {PORT}")
    server = subprocess.Popen(
        [str(BIN), "serve", "--http", f"127.0.0.1:{PORT}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        if not wait_for_server(30):
            print("the MCP server never answered /health")
            return 1
        print("  server up")
        scenario("aligned")
        # The point of the second run: an application whose X11 class and
        # accessibility name disagree is ordinary (Python names the class after
        # the script, Electron reports `electron`), and matching those names is
        # how the window used to be found.
        scenario("mismatch")
        hung_application()
    finally:
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()

    print(f"\n{len(failures)} failed check(s); evidence in {EVIDENCE}")
    return 1 if failures else 0


def hung_application() -> None:
    """One application that stops answering, next to one that keeps working.

    This is the contract issue #11 asked for: a tree that could not be fully read
    must still arrive, and it must say which part is missing -- instead of either
    failing the whole observation or quietly returning a shorter tree that looks
    complete. A frozen process is the cheapest way to make the bus stop answering:
    SIGSTOP keeps the D-Bus socket open, so every call to that application hangs
    until the timeout, exactly like a real wedged toolkit.
    """
    global TAG

    TAG = "hung"
    print("\n== phase: one application hangs")

    results = {name: EVIDENCE / f"probe_result_{name}.txt" for name in ("hung", "good")}
    for path in results.values():
        if path.exists():
            path.unlink()

    procs = {}
    logs = []
    for name in ("hung", "good"):
        log = open(EVIDENCE / f"probe_{name}.log", "w", encoding="utf-8")
        logs.append(log)
        procs[name] = subprocess.Popen(
            [
                sys.executable,
                str(ROOT / "tests" / "linux_atspi" / "probe_gtk.py"),
                str(results[name]),
            ],
            stdout=log,
            stderr=subprocess.STDOUT,
            env={**os.environ, "PROBE_IDENTITY": "aligned"},
        )

    window = None
    deadline = time.time() + 40
    while time.time() < deadline and window is None:
        listed = call("list_windows", {"query": PROBE_TITLE})
        windows = (listed["json"] or {}).get("windows", [])
        if windows:
            window = windows[0]
        else:
            time.sleep(1.0)
    check("[hung] the probe applications are on the bus", window is not None, f"window={window}")
    if window is None:
        for proc in procs.values():
            proc.terminate()
        return

    # Both registrations have to be listed before the freeze, otherwise this phase
    # would be testing an application that never joined the walk.
    time.sleep(3.0)
    os.kill(procs["hung"].pid, signal.SIGSTOP)

    started = time.time()
    # A generous HTTP timeout on purpose: before the fix the walk burns the ~25s
    # D-Bus default per unanswered call, and that has to show up as a slow but
    # *returned* observation, not as this script crashing.
    #
    # And if it does not come back at all, that belongs in the PASS/FAIL table --
    # measured 2026-10-24 on run 40115985180, where the pre-fix read exceeded even
    # 180s and killed the harness with a socket timeout instead of reporting.
    try:
        tree = call("accessibility_tree", {"max_nodes": 200, "depth": 6}, timeout=180)
    except (urllib.error.URLError, OSError) as exc:
        elapsed = time.time() - started
        check(
            "[hung] the tree comes back at all while an application is frozen",
            False,
            f"no reply after {elapsed:.1f}s: {exc}",
        )
        tree = {"is_error": True, "json": None, "text": "", "images": 0}
    elapsed = time.time() - started
    body = tree["text"]

    check(
        "[hung] the tree still comes back, holding the responsive application",
        not tree["is_error"] and "probe-entry" in body,
        f"elapsed={elapsed:.1f}s err={tree['is_error']} {body[:150]}",
    )
    check(
        "[hung] the observation names the part it could not read",
        "parts of this tree were not read" in body,
        f"tail={body[-300:]}",
    )
    check(
        "[hung] a frozen application costs a bounded timeout, not 25s per call",
        elapsed < 10,
        f"elapsed={elapsed:.1f}s",
    )

    os.kill(procs["hung"].pid, signal.SIGCONT)
    for proc in procs.values():
        proc.terminate()
    for proc in procs.values():
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    for log in logs:
        log.close()


def scenario(identity: str) -> None:
    """Drive one probe application through observe, write and act.

    `identity` is handed to the application as `PROBE_IDENTITY`: `aligned` makes
    it declare the same name on X11 and on the accessibility bus, `mismatch`
    leaves X11 with Python's default (the script filename) so only a process id
    can connect the window to its frame.
    """
    global TAG

    TAG = identity
    print(f"\n== scenario: {identity}")
    result_file = EVIDENCE / f"probe_result_{identity}.txt"
    if result_file.exists():
        result_file.unlink()

    probe_log = open(EVIDENCE / f"probe_{identity}.log", "w", encoding="utf-8")
    probe = subprocess.Popen(
        [sys.executable, str(ROOT / "tests" / "linux_atspi" / "probe_gtk.py"), str(result_file)],
        stdout=probe_log,
        stderr=subprocess.STDOUT,
        env={**os.environ, "PROBE_IDENTITY": identity},
    )
    try:
        window = None
        deadline = time.time() + 40
        while time.time() < deadline and window is None:
            listed = call("list_windows", {"query": PROBE_TITLE})
            windows = (listed["json"] or {}).get("windows", [])
            if windows:
                window = windows[0]
            else:
                time.sleep(1.0)
        if window is None:
            probe_log.flush()
            print("--- the probe application said ---")
            print(Path(probe_log.name).read_text(encoding="utf-8", errors="replace")[:2000])
        check(f"[{identity}] list_windows sees the GTK window", window is not None, f"window={window}")
        if window is None:
            return

        reported_app = window.get("app", "")
        if identity == "mismatch":
            # Without this the second run could quietly become a repeat of the
            # first — if X11 ever reports the aligned name, this scenario is
            # testing nothing and must say so.
            check(
                "[mismatch] the window really reports a different app name",
                reported_app != PROBE_APP,
                f"window list says app={reported_app!r}, accessibility name is {PROBE_APP!r}",
            )

        state = call(
            "window_state",
            {
                "window": window["id"],
                "include_text": True,
                "max_nodes": 80,
                # Each scenario starts from a clean baseline: the second run reuses
                # the same X11 window id and the same AT-SPI object paths, so the
                # diffing layer is entitled to answer "unchanged" — which is about
                # the mapping, not about whether the frame could be matched at all.
                # This scenario tests the match, so it asks for the full tree.
                "disable_diffing": True,
            },
        )
        tree = (state["json"] or {}).get("accessibility") or {}
        body = tree.get("tree") or tree.get("diff") or ""
        node_count = tree.get("node_count", 0)
        # `disable_diffing` answers `baseline_reset`, not `full`: both mean the
        # response carried the whole tree, while `diff`/`unchanged` would mean it
        # referred back to an earlier observation instead.
        check(
            f"[{identity}] window_state returns a window-level accessibility tree",
            node_count >= 4
            and body != ""
            and tree.get("mode") in ("full", "baseline_reset"),
            f"nodes={node_count} mode={tree.get('mode')} {state['text'][:120] if node_count < 4 else ''}",
        )
        for control in ("probe-entry", "probe-submit", "probe-toggle"):
            check(f"[{identity}] tree contains {control!r}", control in body)
        check(
            f"[{identity}] the tree is scoped to this window, not the whole desktop",
            "type into the entry below" in body,
            "the label only exists inside the probe frame",
        )
        # Two controls answer to `probe-ambiguous` and the walk meets the label
        # first, so a find that stops at the first exact hit hands back something
        # that cannot be pressed. The actionable role has to win.
        found = call("find_control", {"name": "probe-ambiguous"})
        found_json = found["json"] or {}
        check(
            f"[{identity}] find_control prefers the button over a same-named label",
            found_json.get("control_type") == "Button",
            f"got {found_json.get('control_type')!r} "
            f"name={found_json.get('name')!r} {found['text'][:120]}",
        )
        # The contract being checked is that a missing image is announced, not
        # that Xvfb can always produce one: window_state must not silently drop
        # the half of the observation that failed.
        captured = state["images"] >= 1
        announced = bool((state["json"] or {}).get("screenshot_error"))
        check(
            f"[{identity}] the observation reports its screenshot or says why it cannot",
            captured or announced,
            f"images={state['images']} announced={announced}",
        )

        # State read over AT-SPI, checked against the app's own account of the
        # same widget. Before the fix the first call answers "not supported on
        # this platform", so this whole block is the red half of the control.
        toggle_log = Path(str(result_file) + ".toggles")
        if toggle_log.exists():
            toggle_log.unlink()
        before = call("control_state", {"name": "probe-toggle"})
        before_json = before["json"] or {}
        check(
            f"[{identity}] control_state reads the check box as off",
            not before["is_error"] and before_json.get("toggle") == "off",
            f"toggle={before_json.get('toggle')!r} {before['text'][:120]}",
        )

        toggled = call("invoke_control", {"name": "probe-toggle"})
        after = call("control_state", {"name": "probe-toggle"})
        after_json = after["json"] or {}
        check(
            f"[{identity}] after DoAction the state reads as on",
            not toggled["is_error"] and after_json.get("toggle") == "on",
            f"invoke={toggled['text'][:60]} toggle={after_json.get('toggle')!r}",
        )
        check(
            f"[{identity}] enabled and offscreen come from their own ordinals",
            after_json.get("enabled") is True and after_json.get("offscreen") is False,
            f"enabled={after_json.get('enabled')} offscreen={after_json.get('offscreen')}",
        )
        # The widget's own signal log is the independent oracle. Without it, a
        # wrong ordinal that happened to be set could pass for a match.
        logged = toggle_log.read_text().split() if toggle_log.exists() else []
        check(
            f"[{identity}] the check box itself reports one toggle, to checked",
            logged == ["1"],
            f"toggle log={logged}",
        )
        check(
            f"[{identity}] value stays absent until the Text interface is read",
            "value" not in after_json,
            f"value={after_json.get('value')!r}",
        )

        written = call("set_control_text", {"name": "probe-entry", "text": "AT-SPI-WRITES-HERE"})
        check(
            f"[{identity}] set_control_text reaches the entry",
            not written["is_error"],
            written["text"][:120],
        )

        invoked = call("invoke_control", {"name": "probe-submit"})
        check(
            f"[{identity}] invoke_control fires DoAction on the button",
            not invoked["is_error"],
            invoked["text"][:160],
        )

        try:
            probe.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pass
        landed = result_file.read_text() if result_file.exists() else ""
        check(
            f"[{identity}] the text set through accessibility really reached the control",
            landed == "AT-SPI-WRITES-HERE",
            f"probe app reported {landed!r}",
        )

        gone = call("list_windows", {"query": PROBE_TITLE})
        check(
            f"[{identity}] the window is gone after the invoked action",
            (gone["json"] or {}).get("count", 1) == 0,
            f"count={(gone['json'] or {}).get('count')}",
        )
    finally:
        if probe.poll() is None:
            probe.kill()
            probe.wait(timeout=10)
        probe_log.close()


if __name__ == "__main__":
    sys.exit(main())
