# AgentDeck contributor guide

AgentDeck is a portable local browser dashboard for Herdr. Keep changes
portable, deterministic, and safe for contributors without the maintainer's
machine, accounts, or local agent history.

## Development

- Rust 1.85 is the MSRV. Run `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and
  `cargo test --workspace --locked` before handing off Rust changes.
- Run the focused test nearest to a change first. Browser changes use
  `node --test Tests/UI/*.mjs`; Unix installer/service changes use the matching
  scripts under `release/tests/`.
- Use `apply_patch` for source edits. Preserve unrelated dirty work and do not
  reset, overwrite, or remove it.

## Local integrations and privacy

- Herdr is the only hard runtime dependency, but local-model enrichment is the
  recommended full experience. Safe initialization does not select or pull a model.
  Never add a model pull, provider installation, automatic service start, or credential
  setup to code, tests, or documentation.
- Do not run against real transcripts, screens, agent-session state, cookies,
  tokens, or provider accounts. Public fixtures must be synthetic and must not
  contain prompts, replies, paths, identifiers, or secrets from a real machine.
- The default listener is loopback. Do not change Herdr configuration, Tailscale
  routes, proxy settings, services, or a live AgentDeck process while developing
  or testing unless the task explicitly authorizes that exact mutation.

## Public repository boundary

Keep the repository portable and self-contained. Do not add personal machine
configuration, retired setup scripts, real evaluation corpora, or local project
history. Public changes must remain reproducible from synthetic fixtures and
documented dependencies.
