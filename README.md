# AgentDeck

**A live control surface for people running several coding agents at once.**

> When I have coding agents working across several repositories, I want one place to
> see what each one is doing and which one needs me, so I can keep work moving
> without reopening every terminal.

AgentDeck turns [Herdr](https://herdr.dev/) sessions into a browser dashboard. Herdr
provides the agent and workspace state; a local model turns raw session context into
short, stable titles, current-step subtitles, and useful outcome summaries.

The dashboard runs without a model, but local-model enrichment is the recommended
experience and the feature that makes a busy deck genuinely scannable.

## Why it exists

Terminal multiplexers are good at holding sessions. They are less good at answering the
questions that appear once several agents are working simultaneously:

- Which agent is still working, finished, blocked, or waiting for me?
- What is each long-running session actually about now?
- Did an agent reply while I was looking elsewhere?
- Which repository and workspace owns the session?
- How much context and Claude/Codex capacity has been used?
- Is a background shell still running after the agent itself became idle?

AgentDeck compresses those answers into one glanceable surface. It does not replace the
terminal or the coding agent; it helps decide where attention should go next.

## What the deck shows

- Herdr workspaces and agent sessions across Claude Code, Codex, Pi, Copilot, and
  unknown/future agent kinds reported by Herdr.
- Working, idle, done, blocked, unread, focused, and background-process state.
- Local-model enrichment for a stable session title, the concrete step now underway,
  and the latest outcome or decision.
- Context consumption, recent reply age, current execution phase, and token activity
  where a safe local adapter exists.
- Optional Claude/Codex capacity from CodexBar and local host/model telemetry.
- Focus, workspace, and new-tab actions without leaving the dashboard.
- Live HTTP/SSE updates through one small Rust executable with embedded web assets.

## Two useful operating modes

| Mode | What is required | What you get |
|---|---|---|
| **Recommended** | Herdr + Ollama running on loopback + a configured installed model | Model-enriched cards for supported Claude Code, Codex, and Pi sessions: contextual titles, current-step subtitles, outcomes, and safe local context enrichment. |
| **Fallback** | Herdr only | Reliable agent/workspace/status/focus cards with deterministic labels; model-derived prose and the model panel stay absent. |

CodexBar and machine telemetry are separate enhancements. Missing integrations never
prevent the deck from starting: their empty panels are hidden and a small dismissible
message explains what installing or configuring that provider would add.

AgentDeck never downloads a model, starts Ollama, installs CodexBar, edits Herdr, or
configures provider credentials automatically.

## Model choice

No model tag is hard-coded. Every heading job accepts any model already available from
the configured Ollama endpoint.

- `gemma4:12b` is a higher-quality reference option for machines able to run it.
- A 4B-class instruct model is the practical starting point for machines that cannot
  carry a 12B model. `qwen3.5:4b` is the current lightweight documented example; it is
  not an allow-list or a claim that one model wins on every machine.
- The synthetic harness in [`Evals/local-models`](Evals/local-models/README.md) exists
  so new models can be compared without reading real transcripts or exposing private
  session data.

Model quality matters here more than general benchmark rank: concise instruction
following, correct use of a preceding reply, and resisting title repetition are the
important behaviours. Contributions with reproducible synthetic results are welcome.

## Quick start from source

The project is currently pre-release. Until the first tagged GitHub release is
published, build with Rust 1.85 or newer:

```bash
git clone https://github.com/JasonBates/agentdeck-rs.git
cd agentdeck-rs
cargo build --release --locked
./target/release/agentdeck config init
```

Install and start [Herdr](https://herdr.dev/docs/install/) before launching AgentDeck.
The default listener is `http://127.0.0.1:9798`.

`config init` creates a private file at `~/.config/agentdeck/config.toml` on
macOS/Linux or `%APPDATA%\agentdeck\config.toml` on Windows. It leaves the model tag
for you to choose:

```toml
[headings]
backend = "auto"
endpoint = "http://127.0.0.1:11434"
model = "your-installed-model-tag" # for example gemma4:12b or a smaller model
title_model = "inherit"
subtitle_model = "inherit"
outcome_model = "inherit"
activity_model = "inherit"
```

With that edit saved, start the recommended model-enriched mode:

```bash
./target/release/agentdeck doctor
./target/release/agentdeck serve
```

Open `http://127.0.0.1:9798`. The dashboard reports whether the configured model is
available and shows a short setup message when it is not. Skip the model edit only when
you deliberately want the Herdr-only fallback.

## Release installation

Once a tagged release exists, the checked-in installers download a platform archive,
verify its integrity against `SHA256SUMS`, install atomically, and record an ownership
receipt. They refuse to replace unrelated binaries unless `--force` is explicit. The
first artifacts will be unsigned; read the [security policy](SECURITY.md) before using
them and do not treat a checksum published beside an archive as publisher authentication.

macOS/Linux:

```bash
curl -fLO https://raw.githubusercontent.com/JasonBates/agentdeck-rs/main/release/install.sh
bash install.sh
agentdeck config init
agentdeck doctor
agentdeck serve
```

Windows PowerShell:

```powershell
Invoke-WebRequest -Uri https://raw.githubusercontent.com/JasonBates/agentdeck-rs/main/release/install.ps1 -OutFile .\install.ps1
.\install.ps1
& "$env:LOCALAPPDATA\AgentDeck\bin\agentdeck.exe" config init
& "$env:LOCALAPPDATA\AgentDeck\bin\agentdeck.exe" doctor
& "$env:LOCALAPPDATA\AgentDeck\bin\agentdeck.exe" serve
```

Windows support follows Herdr's native Windows beta. AgentDeck builds and tests on
Windows, including named-pipe protocol fixtures, PowerShell installation, Task
Scheduler lifecycle, and archive layout. A live Windows Herdr/Copilot smoke test is
still required before the first stable release.

## Privacy and boundaries

AgentDeck is local-first:

- The listener is loopback-only by default.
- There is no analytics, crash upload, hosted service, or cloud-model fallback.
- Claude Code, Codex, and Pi enrichment reads bounded local transcript windows.
- Copilot remains a generic Herdr card unless its validated local event data can safely
  provide reply age/context; generated Copilot headings are currently unavailable.
- Set `transcripts.enabled = false` to prevent all transcript-file reads.
- Remote listeners require a bearer token and exact allowed origins.

See [Privacy](docs/privacy.md) for the full data-flow contract.

## Commands

```text
agentdeck serve
agentdeck doctor [--json]
agentdeck config init [--stdout|--force]
agentdeck config print
agentdeck version [--json]
```

`doctor` is read-only and reports Herdr compatibility, listener readiness, model and
capacity-provider availability, and local paths without exposing transcripts or tokens.

## Project status

AgentDeck is a pre-release project undergoing cross-platform verification. The core
workspace, HTTP/SSE boundary, transcript adapters, provider fallbacks, installers, and
service lifecycle are heavily tested. Before calling the first release stable, the
project still needs clean-machine smoke tests, a native Windows Herdr/Copilot exercise,
macOS signing/notarization, and a longer live soak.

This repository contains the complete portable implementation and synthetic test assets.

## Documentation

- [Installation, services, upgrade, and rollback](docs/install.md)
- [Configuration and model setup](docs/configuration.md)
- [Privacy and local data](docs/privacy.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Cross-platform support matrix](docs/platform-support.md)
- [HTTP/SSE contract](docs/contracts/http-api.md)
- [Herdr compatibility](docs/contracts/herdr-compatibility.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)
- [MIT License](LICENSE)

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features --locked
node --test Tests/UI/*.mjs
python3 -m unittest discover -s Evals/local-models -p 'test_*.py'
bash release/tests/install-uninstall.sh
```

Rust 1.85 is the minimum supported version. Public fixtures are synthetic; never commit
real prompts, replies, session identifiers, machine paths, provider data, or credentials.
