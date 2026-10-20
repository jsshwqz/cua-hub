# Roadmap

## Phase 1 — Vision MVP (current)

- [x] Workspace, CI on Windows + Linux
- [x] Cross-platform capture (xcap) and input (enigo)
- [x] Anthropic Computer Use loop with official tool protocol
- [x] Rule router, risk gate, dry-run, JSONL run log
- [x] OpenAI-compatible vision provider (OpenRouter / OpenAI / Ollama / vLLM)
- [x] First end-to-end task on Windows (Start menu, visually verified)
- [x] DPI-aware coordinate mapping (model -> physical -> logical input)
- [ ] First end-to-end task on Linux
- [ ] Model adapter trait extracted from the Anthropic provider

## Phase 2 — Multi-engine

- [ ] `cua-engine-access`: Windows UIA and Linux AT-SPI2 tree reading
- [ ] `cua-engine-code`: generated PowerShell/bash with allow-list execution
- [ ] OpenAI computer-use provider; local UI-TARS via Ollama
- [ ] Router scoring: route by app, task shape, and engine health
- [ ] Engine fallback on repeated failure

## Phase 3 — Browser and orchestration

- [ ] `cua-engine-browser` over CDP with DOM + screenshot hybrid
- [ ] Cross-engine plans (vision discovers, code fixes, browser verifies)
- [ ] Persistent task state, resume after crash
- [ ] MCP server exposing `run_task` / `capture` to other agents

## Phase 4 — Product

- [ ] HTTP API + web console with live screen
- [ ] Recording and replay
- [ ] Fine-tune pipeline for a local model (replace API calls for hot paths)
- [ ] SDK for Rust, Python, TypeScript
