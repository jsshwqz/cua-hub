---
name: cua-hub
description: Drive this computer like a human via MCP tools — screenshot, click, type, scroll, clipboard, Windows UIA / Linux AT-SPI accessibility trees, precise control invocation, and a headless browser. Use when the user asks you to open/operate a desktop app, click or type in a GUI, read on-screen controls, automate a website, take a screenshot, or replay a previous session.
author: qoder
---

# cua-hub — computer use for agents

An MCP server that exposes **this machine** as tools. Prefer it over guessing
coordinates or writing OS scripts when a task involves a GUI.

## Tools (stdio `cua-hub serve`, or HTTP `POST /mcp`)

Perception

- `capture_screen` — primary display as a JPEG plus dimensions.
- `screen_info` — screen size, origin, DPI, cursor position (use to validate coordinates).
- `accessibility_tree {max_nodes?, depth?}` — UIA (Windows) / AT-SPI (Linux) tree with rects.
- `find_control {name}` — locate by accessible name or `id:<AutomationId>`, returns rectangle and centre.

Precise UI control (prefer over pixels)

- `invoke_control {name}` — Invoke pattern first, click-centre fallback.
- `set_control_text {name, text}` — Value/EditableText pattern, no keystrokes.
- `control_state {name}` — enabled/offscreen, toggle, value, expanded, range.

Raw input

- `click {x, y, right?, middle?, double?}` — physical pixels.
- `move {x, y}`, `drag {x, y, x2, y2}`, `scroll {x, y, dx?, dy?}`.
- `type_text {text, interval_ms?}` — Unicode text into the focused window.
- `key {combo}` — `Return`, `ctrl+l`, `alt+Tab`, `PageDown`, arrows, F1–F12…
- `mouse_down/up {button?}`, `key_down/up {combo}` — hold/release pairs.
- `clipboard_get`, `clipboard_set {text}`.

Tasks and browser

- `run_task {task, engine?, model?, max_steps?, dry_run?, yes?, allow_high?}` — one-shot agent (blocks).
- `start_task` / `task_status {job_id}` / `task_cancel {job_id}` — background jobs with progress.
- `browser_task {task, start_url?, headed?, max_steps?}` — isolated Chromium via CDP (navigate, click, type, scroll, wait_for, tabs).
- `orchestrate {task, dry_run?}` — plan across browser/code/vision and run the steps.
- `list_runs`, `replay_run {dir, dry_run?, yes?, allow_high?}`.

## How to work

1. **Look first**: `capture_screen` plus `accessibility_tree`; prefer accessible
   names/ids over pixel coordinates.
2. **Use precise control** when a control has a name: `invoke_control`,
   `set_control_text`, then `control_state` to verify.
3. **Fall back to pixels** only when the tree is empty (games, canvas, custom UI).
   Call `screen_info` first; coordinates are physical pixels of the primary display.
4. **Web tasks**: use `browser_task` (headless by default) instead of clicking a
   desktop browser. It has its own scroll, wait_for and tab actions.
5. **Long tasks**: `start_task` then poll `task_status`; do not block a tool call
   for minutes. Cancel with `task_cancel`.
6. **Verify**: after typing or clicking, re-capture or read `control_state`.
   A step note saying the screen did not change means try another approach.
7. **Windows text fields**: `set_control_text` targets Win32 edit `id:15`-style
   controls; for unknown controls use `find_control` first.

## Safety

- Medium-risk actions run with the default `yes: true`; destructive commands
  (delete, format, shutdown, registry) are blocked unless `allow_high: true`.
- Over stdio the server asks for approval via MCP elicitation; over HTTP there is
  no reverse channel, so high-risk calls must be explicit.
- Never enter credentials or submit payments unless the user explicitly asked.
- Clipboard is global to the user's session: do not overwrite it without reason.

## Requirements

- Model credentials in the server environment: `OPENROUTER_API_KEY` /
  `OPENAI_API_KEY` (OpenAI-compatible), or `ANTHROPIC_API_KEY`.
- Windows: UIA works out of the box. Linux: AT-SPI needs a running accessibility
  bus; X11 input via enigo.
- CLI fallback: `cua-hub run "<task>"`, `cua-hub browser "<task>"`,
  `cua-hub orchestrate "<task>"`, `cua-hub console` for a local live view.
