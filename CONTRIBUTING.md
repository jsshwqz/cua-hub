# Contributing to cua-hub

Thanks for your interest in contributing! This document is a short guide to get you started.

## How to contribute

- **Bug reports**: Open an issue with a clear title, steps to reproduce, expected vs actual behavior, and your OS / cua-hub version (`cua-hub --version`).
- **Feature requests**: Open an issue describing the use case first — let's agree on the design before code.
- **Pull requests**: Fork, branch from `main`, keep changes focused. One PR per logical change.

## Development setup

```bash
cargo build
cargo test
```

Before submitting a PR, please run the full gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass. CI runs the same gate on Linux and Windows.

## PR guidelines

- Keep the diff minimal and focused; avoid drive-by refactors.
- Update `CHANGELOG.md` under the Unreleased section when user-facing behavior changes.
- Add or update tests for behavior changes. New `#[ignore]` tests must say why they are ignored.
- Write commit messages in the imperative mood, e.g. `fix(mcp): keep unreadable ledger on degrade`.

## Code of conduct

By participating you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Contributions are licensed under the MIT License (see [LICENSE](LICENSE)).
