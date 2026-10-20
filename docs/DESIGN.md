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

There are two spaces:

* **physical** — what `xcap` captures and what `enigo` addresses. On Windows
  these are the same physical pixels even when DPI scaling is on (verified:
  capture 1920x1080, `SetCursorPos` round-trips 1:1).
* **model** — the screenshot after downscaling to `--max-edge` (default 1568).
  The model's tool coordinates live here.

`Frame.scale` = physical / model. Every `Point` coming from an engine is
multiplied by `scale` before injection. This keeps token cost bounded without
introducing drift: we always send the exact dimensions the model is told about
in the tool declaration.

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
