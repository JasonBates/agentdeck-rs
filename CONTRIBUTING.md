# Contributing to AgentDeck

Thanks for helping improve AgentDeck. Small, focused issues and pull requests are easiest to
review.

## Before opening an issue

Search existing issues first and describe the observed behavior, platform, AgentDeck version,
and Herdr version. Include only redacted configuration and diagnostics. Do not attach real
transcripts, screen output, session or pane IDs, repository paths, model/provider data, bearer
tokens, cookies, or other secrets.

Use a pull request for a fix or a clearly scoped improvement. Explain the user-visible change,
the safety or compatibility impact, and the tests run. Keep unrelated formatting and refactors
out of the same pull request.

## Development checks

Rust 1.85 is the minimum supported Rust version. Before submitting Rust changes, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features --locked
bash release/tests/install-uninstall.sh
```

Run the relevant focused tests as well. Changes to Unix services or archive/install behavior
should also run `bash release/tests/service-unix.sh` and the relevant archive-layout test; use
the PowerShell equivalents on Windows. Browser changes should run `node --test Tests/UI/*.mjs`.

## Fixtures and privacy

Fixtures are public source material. Use synthetic, sanitized values and keep them free of real
prompt or reply text, transcript or screen fragments, filesystem paths, session/pane/workspace
identifiers, credentials, tokens, cookies, and model/provider account data. Do not test against
live providers, personal Herdr state, or local transcript directories. Add a focused regression
test for every privacy or parser boundary you change.

## Pull requests

Keep the branch buildable, update user-facing documentation when behavior changes, and state any
remaining platform limitation in the pull request description. Reviewers may request narrower
scope, additional redaction, or a separate security report for sensitive findings.
