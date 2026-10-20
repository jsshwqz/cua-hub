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
