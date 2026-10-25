# Design

## Data flow

```
runner loop (cua-cli)
  ├─ cua-io::observe        capture screen, downscale, JPEG-encode, PNG archive
  ├─ Engine::next_action    vision engine asks the model what to do next
  ├─ risk gate              medium/high risk needs --yes or a TTY confirmation
  ├─ cua-io::apply          enigo injects mouse/keyboard
  └─ StepRecord             appended to steps.jsonl, fed back to the engine
```

## Coordinate spaces

There are three spaces, and getting them wrong sends clicks to the wrong place:

* **model** — the screenshot after downscaling to `--max-edge` (default 1568).
  Engine coordinates live here.
* **physical** — what `xcap` captures (e.g. 1920x1080 on a 125% scaled display).
  `Frame.scale` = physical / model.
* **logical / input** — what `enigo` addresses on Windows and macOS. With DPI
  scaling 125%, logical is 1536x864 while physical stays 1920x1080, so
  `Frame.input_scale` = `Frame.scale / dpi`.

The runner multiplies every engine `Point` by `input_scale` before injection.
On a 1280-wide model screenshot of a 1920-physical / 1536-logical screen the
factor is 1.5 / 1.25 = 1.2. Verified on hardware: a click at model (19,703)
becomes logical (22, 843), which opens the Start menu.

The DPI factor comes from `xcap`'s monitor `scale_factor()`. Linux/X11 normally
reports 1.0 and input coordinates are physical, so the factor is 1.0 there.

UIA rectangles are a fourth case: because this process is not per-monitor
DPI-aware, Windows virtualizes them to **logical** pixels (taskbar at
`y=824` on a 1920x1080 @125% screen). That is exactly the space enigo accepts,
so `cua-hub invoke` passes the rectangle center through unscaled. Scaling it
again sent clicks into the open Start menu instead of the Start button — the
kind of bug that only a visual check catches.

### Linux (AT-SPI2)

The Linux backend in `cua-engine-access::atspi` speaks D-Bus directly through
`zbus` (no C headers or `libatspi-dev` needed):

1. Open the session bus and call `org.a11y.Bus.GetAddress`.
2. Connect to that accessibility bus and read the registry root at
   `/org/a11y/atspi/accessible/root`; its children are applications.
3. Each child is a `(bus_name, object_path)` pair, so one `Proxy` per node is
   created with that destination.

Two protocol details cost real debugging time:

* **Role is a method, not a property.** `GetRole()` / `GetRoleName()` are
  D-Bus methods; reading it as a property returns `UnknownProperty`, which
  silently mapped every node to role 0 ("Invalid").
* **The GTK bridge must be enabled.** In WSL, the `org.a11y.Status` schema is
  absent; setting `org.gnome.desktop.interface toolkit-accessibility=true` and
  exporting `GTK_A11Y=atspi` makes GTK3 register with the registry. Without it
  the registry root has no children.

Verified in WSL Ubuntu 24.04 with a 420x180 GTK3 window:

```
Application "-"
  Frame "CUA AT-SPI Demo"
    Panel
      Button "Close"
      Label "CUA AT-SPI Demo"
    Text
      Button "Click Me"
```

`find("Click Me")` returns the button rect and center. Reproduce with
`dbus-run-session -- bash scripts/a11y_smoke.sh <atspi_tree-binary>`.

Limitations: no action invocation yet (click-by-center works through enigo);
D-Bus calls currently have no per-call timeout; the full CLI has only been
built on Windows, so Linux CI should cover it next.

### Windows

Control lookup uses a server-side `FindFirst` with a combined condition
(`Name == X AND ControlType == Button/MenuItem/ListItem`) instead of walking the
tree. Preferring actionable control types matters: with the Start menu open,
both the menu (`List`) and the button (`Button`) are named "开始".

## Providers

Two vision backends ship today:

* **Anthropic** — native computer tool (`computer_20250124`), stateful tool loop.
* **OpenAI-compatible** — chat completions with an image URL and a strict JSON
  action schema; works with OpenRouter, OpenAI, Ollama `/v1` and vLLM. Some
  reasoning models spend the token budget on `reasoning` and return empty
  `content`; the provider falls back to extracting the action object from the
  reasoning text.

## Anthropic computer tool loop

The provider speaks the official Messages API protocol rather than a
hand-rolled prompt format:

1. First turn: `user` = task text + screenshot.
2. Assistant returns a `tool_use` block (`computer` tool, `computer_20250124`).
3. We execute it locally and answer with a `user` turn containing a
   `tool_result` block that carries a text note plus a fresh screenshot.
4. Completion is signalled by the model replying without tools using the
   markers `TASK_COMPLETE:` / `TASK_FAILED:`.

Only one tool call per turn is executed. Extra calls in the same assistant
message are acknowledged as `skipped` so the transcript stays valid for the
next request.

## Code engine

`cua-engine-code` asks the model for one command per turn and returns
`Action::RunCommand`. The runner executes it with a 120s timeout, feeds
`exit code + stdout + stderr` back as the step note, and stops on
`{"done":true}`. Every command is rated `Medium` risk by default; a deny-list
(`rm -rf /`, `format`, `diskpart`, `shutdown`, `bcdedit`, registry deletion, …)
raises it to `High`, which requires `--yes` or an interactive confirmation.

## Accessibility snapshot

Windows UIA exposes properties through cross-process COM calls. Reading five
properties per node with `CurrentX()` took ~36s for 40 nodes. The implementation
now builds one `IUIAutomationCacheRequest` (name, control type, automation id,
rect, offscreen) and uses `GetCachedChildren`, cutting the same walk to ~0.7s at
depth 1. Cost grows with depth, so per-step hybrid context uses depth 1 and
`cua-hub tree` exposes deeper walks for debugging (depth 5 ≈ 4s).

## Browser engine

`cua-hub browser` (and `run` for URL-shaped tasks) launches a Chromium browser
with `--remote-debugging-port`, a temporary `--user-data-dir`, and headless mode
unless `--headed` is given. It then talks CDP directly over WebSocket:

* requests are matched by numeric id; events are ignored, pings answered;
  each command has a 30s timeout
* navigation polls `document.readyState` until `complete` (20s budget)
* actions are `navigate`, `click` (CSS selector or `text=Visible label`),
  `type`, `press`, `eval`, `wait`, `scroll`, `wait_for`, `tab_list`,
  `tab_new`, `tab_switch`, `tab_close`, `done`, `fail`
* selectors also search same-origin iframes (`contentDocument`); cross-origin
  frames are not reached without per-frame execution contexts
* every step sends the model URL, title, a flattened CDP accessibility tree
  (`Accessibility.getFullAXTree`, role/name, ≤120 lines) and 2k chars of text,
  plus the screenshot
* `wait_for` polls for a selector instead of sleeping a fixed time; `scroll`
  dispatches a real mouse-wheel event at the viewport centre
* tabs are managed over the DevTools HTTP endpoints and a fresh WebSocket per
  target; closing the current tab reconnects to the first remaining page

Security: `eval` executes model-generated JavaScript. The temporary profile has
no user cookies or saved logins. Each step now also passes the shared
confirmation gate (`cua_core::policy::confirm`) before it is fired: `eval` is
High and needs `allow_high`, a cross-site `navigate`, a click on a submit-looking
target and any `press Enter` are Medium and need `yes` — the prompt's ban on
submitting forms was always a policy rather than a sandbox, and the tiers are
where that policy stops being only words. Treat the browser engine as executing
untrusted code on the model's behalf, with a permission switch per tier.

Browser actions live in their own enum rather than `cua_core::Action`: DOM
selectors and page evaluation do not map onto mouse coordinates. They carry a
`risk()` of their own instead, so the one gate serves both action spaces. The
router dispatches URL tasks to this engine and maps its outcome back into the
shared `RunReport`.

## Replay

Every run already writes `steps.jsonl`. `cua-hub runs` summarises them and
`cua-hub replay <dir>` re-executes the recorded actions in order:

* each `StepRecord` now stores `input_scale` (model→input multiplier), so a
  replay lands on the same pixels even if `--max-edge` or DPI changes later
  (older runs without the field fall back to 1.0);
* `Wait` sleeps, `Done`/`Fail`/`Screenshot` are skipped, and every mutating
  action goes through the same risk gate as a live run (`--yes`,
  `--allow-high`);
* `RunCommand` is re-executed with its recorded shell and its output printed;
* `--dry-run` prints the sequence without touching the machine.

Vision recordings remain screen-layout dependent: replaying clicks is only
meaningful on a desktop in the same state. The browser engine's own
`browser-step-*.jpg`/`steps.jsonl` are not replayed by this command yet.

## Cross-engine orchestration

`cua-hub orchestrate` turns one goal into an ordered plan, executes each step on
its own engine, and passes outputs forward:

```json
{"steps": [
  {"engine": "browser", "goal": "open example.com and read document.title", "save_as": "title"},
  {"engine": "code",    "goal": "Write-Output \"{{title}}\""}
]}
```

* The planner is an OpenAI-compatible model call with a strict JSON schema;
  plans are capped at `--max-plan-steps` (default 6).
* `save_as` stores the step's cleaned summary; later goals substitute
  `{{name}}`. Substitution is plain string replacement, so a failed step that
  skipped `save_as` leaves a visible placeholder instead of silently guessing.
* Each step runs with `--max-steps` (default 10) on its own engine: browser via
  the CDP loop, vision/code through the desktop runner (access maps to vision
  until control invocation lands).
* Execution stops at the first failed step. Per-step artifacts live under
  `.runs/orchestrate-*/step-N-engine/`, with `plan.json` and `results.json` at
  the top level.
* `--dry-run` prints the plan and touches nothing.

Failure recovery is layered:

1. **Retry** (`--retries`, default 1): rerun the same engine with the failure
   appended to the goal (`Previous attempt failed with: …`). Retries get
   `+3` steps of budget headroom, because a starved run usually failed before
   it could report `done`.
2. **Re-plan** (`--max-replans`, default 1): send the original task, previous
   plan, completed results, failed step and available variables to the planner
   and ask for a plan covering only the remaining work, explicitly telling it
   to change approach/engine. Re-planned steps get `+2` budget headroom.
   Revisions are stored as `plan-revN.json`.
3. Engine/provider errors count as failed attempts rather than aborting the
   whole run, so they flow through the same recovery path.

This is still sequential hand-off: no parallel steps, no verification step
unless the planner writes one, and variable substitution remains plain text.

Both paths were verified on hardware: with a 1-step budget the browser step
fails, a retry with headroom finishes it; with retries disabled, the re-plan
switches to `Invoke-WebRequest` in PowerShell and still returns the title.

## MCP server

`cua-hub serve` speaks newline-delimited JSON-RPC 2.0 on stdio. Three
non-obvious constraints shaped the implementation:

1. **stdout is protocol only.** Every human-facing message (routing, provider,
   step log, summary) goes to stderr. A stray `println!` corrupts the stream.
2. **stdin is read on a dedicated blocking thread.** With `tokio::io::stdin()`
   on Windows, a message arriving after the first read while the pipe stayed
   open never woke the task: requests 1-2 were answered, request 3 hung until
   the client closed stdin. A `std::thread` reading `std::io::stdin().lines()`
   into an mpsc channel is reliable.
3. **Every response needs the envelope.** `tools/call` originally returned the
   tool result object (`{"content":…}`) without `jsonrpc`/`id`/`result`, so
   well-behaved clients ignored it and timed out. The server now wraps tool
   results in a normal JSON-RPC response; tool failures use `isError: true`
   inside the result, not a protocol error.

High-risk actions are never enabled by `run_task`'s default `yes`; they need
`allow_high: true`, mirroring the CLI's `--allow-high` flag.

Over stdio the server can instead ask the client: a high-risk action triggers an
`elicitation/create` request (a boolean `approve` field) and waits up to 60s.
Only `action: "accept"` with `approve: true` lets it run; decline, timeout or a
client that ignores elicitation all count as refusal. Responses are dispatched
on the stdin reader thread straight to the waiting oneshot — routing them
through the request loop would deadlock while a tool call is awaiting approval.
The same hook is wired into `replay_run`.

HTTP has no reverse channel, so `serve --http` keeps the hard block and callers
must pass `allow_high: true` explicitly.

### Web console

`cua-hub console` serves a single-process control panel:

* `GET /` — embedded HTML/JS (dark, two-column), polls `/frame.jpg` every
  800ms and `/state` every 1s; no WebSocket/SSE yet
* `GET /frame.jpg` — 1280px JPEG of the primary display
* `GET /state` — screen size/DPI/cursor plus the job list
* `POST /run` — start a task (same option parser as MCP `run_task`)
* `POST /jobs/{id}/cancel` — abort a job
* `POST /mcp` — the same MCP JSON-RPC handler, hosted in the console process so
  agent-started jobs appear in the UI (jobs are in-process state; a separate
  `serve --http` process has its own empty registry)

Auth is the same `CUA_HUB_MCP_TOKEN` (query or bearer); loopback binds need no
token. The console is view + start + cancel, not screen takeover: there is no
input forwarding and no pause of an in-flight action.

### Async jobs

`run_task` blocks for the whole run; `start_task` instead spawns it in the
background and returns a `job_id` immediately. `task_status` reports
running/completed/failed/cancelled, counts steps from the job's `steps.jsonl`,
and returns the summary plus artifact path when done. `task_cancel` aborts the
Tokio task.

Jobs live in a process-wide `HashMap` for the lifetime of the server — they are
not persisted across restarts. Abort is immediate: an action already handed to
the input thread may still finish, and the browser engine relies on session Drop
to kill Chromium. Use `run_task` when the caller can wait; use `start_task` for
anything that may exceed a client's tool-call timeout.

### HTTP transport

`serve --http 127.0.0.1:8765` reuses the same `handle_request` function as
stdio, so tool behaviour cannot drift between transports. `POST /mcp` accepts a
single JSON-RPC object and replies `application/json` (notifications get
`202 Accepted`); `GET /health` returns `ok` for probes. This is the JSON mode of
streamable HTTP, not SSE.

`CUA_HUB_MCP_TOKEN` enables a single bearer-token check on `/mcp` (`/health`
stays open). Non-loopback binds are refused unless a token is set, or
`CUA_HUB_ALLOW_REMOTE=1` acknowledges that an authenticating proxy sits in
front. Without one, every tool here can control the machine.

## Action verification

A mutating desktop action can "succeed" while doing nothing (clicked a blank
area, keystroke went nowhere). After each action the runner captures an 8x8
average hash of the screen (one cheap capture + resize) and compares it with the
pre-action hash. If they match, the step note becomes
`executed, but the screen did not change; try a different approach`, which both
vision engines feed back to the model on the next turn.

It is deliberately coarse and advisory: animations, cursor-only moves and
non-visual side effects can produce false negatives/positives, and code/browser
runs skip it entirely. It exists to break the common "repeat the same dead
click" loop, not to be a correctness oracle.

## Accessibility actions

`cua-engine-access` no longer stops at rectangles:

* **Windows UIA**: `invoke` uses `InvokePattern`, `set_value` uses
  `ValuePattern`, and `state` reads enabled/offscreen plus Toggle, Value,
  ExpandCollapse and RangeValue patterns. `invoke_control` falls back to a
  center click when the control exposes no pattern.
* **Linux AT-SPI**: `Action.DoAction(0)` for invoke and
  `EditableText.SetTextContents` for set-value. Implemented and compiled in CI,
  but not yet exercised on a live Linux desktop.
* Controls are addressed by a **target**: `name` (case-insensitive substring, a
  `re:` pattern, or `id:<AutomationId>`) plus optional `role`, `container` and
  `nth` (0-based). A target that matches more than one control is an error that
  lists the candidates. Taking the first match -- or the most "actionable" role
  -- is how a slightly wrong guess used to become a click on a control nobody
  named. A bare `name` remains shorthand for `{"target": {"name": ...}}`, and
  sending both `target` and `name` is refused.
* Resolution and action use the same node. AT-SPI acts on the `(bus name,
  object path)` pair the walk read; UIA acts on the element RawViewWalker
  returned. There is deliberately no second lookup by name after resolving,
  because that re-applies the question to a tree that may have changed.
* The id form is required when the accessible name is dynamic or empty (Win32
  edit `15`, Calculator `num1Button`, display `CalculatorResults`). It goes
  through the same matcher, so it is exact and an ambiguous id is refused too.
* Cost, measured rather than assumed: a UIA action now walks the tree instead
  of `FindFirst`. On a 2,326-node desktop a lookup takes about 15s; the
  control-view snapshot of the same desktop takes about 14s, so the walk is
  priced by tree size, not by the walker.

Verified on Windows: `set_control_text(id:15)` filled Notepad through
ValuePattern (readback via `control_state`), and `invoke_control(id:num1Button)`
changed the Calculator display to 1 without moving the pointer.

## Input state and clipboard

`mouse_down` / `key_down` only make sense if the button or key stays held
between calls, so `cua-io` now keeps one process-wide `Enigo` behind a mutex
instead of constructing a fresh instance per action. Releases unwind chords in
reverse order.

Clipboard access uses one process-wide `arboard::Clipboard`. On X11 the
clipboard contents are owned by the living process; a per-call instance would
make the copied text vanish as soon as the tool returned.

MCP pointing tools validate coordinates against the primary display before
injecting input, so an out-of-bounds model answer turns into a tool error
instead of a stray click. These four pieces (clipboard, hold/release,
screen_info, bounds checks) were ported from the `muse-vm-phase0-1` branch; see
`docs/VERSIONS.md`.

## Safety

* Every action carries a `Risk` label. `Type` text matching a destructive
  allow-list (`rm -rf`, `format`, `drop table`, `shutdown`, …) is `High`.
* `--dry-run` executes a single planning step and never touches input.
* `--yes` (or `CUA_HUB_YES=1`) is required for medium/high risk actions in
  non-interactive shells.
* Every run writes `steps.jsonl`, per-step PNGs and `outcome.json` under
  `.runs/<timestamp>/`, so a bad run can be inspected and replayed.

## Extending

An engine is anything implementing `cua_core::Engine`:

```rust
#[async_trait]
impl Engine for MyEngine {
    fn name(&self) -> &'static str { "my-engine" }
    fn kind(&self) -> EngineKind { EngineKind::Access }
    async fn next_action(&mut self, task: &str, obs: &Observation,
                         history: &[StepRecord]) -> Result<Action> { ... }
}
```

`cua-router` chooses between kinds; adding a new engine means registering it
in the CLI and extending the router rules, not changing the runner.
