# Cross-platform support matrix

This matrix distinguishes implemented behavior from remaining release evidence.

| Area | Current state | Remaining evidence |
|---|---|---|
| Core deck and local-model heading policy | Implemented and synthetic-fixture tested | Continue model comparisons as new small models appear |
| Herdr snapshots and actions | Implemented with protocol 19/20 fixtures and runtime tests | Clean-machine live Herdr smoke |
| Herdr events | Unix sockets and Windows namespaced local sockets implemented | Native Windows live named-pipe exercise |
| HTTP/SSE | Routes, security, liveness, slow-client and shutdown behavior tested | Browser accessibility/viewports and longer soak |
| Transcript/context enrichment | Claude Code, Codex, and Pi local formats; bounded Copilot event enrichment | Keep adapters versioned as providers change formats |
| Diagnostics | Read-only `doctor` human/JSON output implemented | Clean-machine usability review |
| Capacity and telemetry | CodexBar, portable basic host, and configured Ollama model panels | Detailed native telemetry remains unsupported |
| Tab-title synchronization | Guarded Unix opt-in | Windows unsupported pending native ACL/reparse validation |
| Service lifecycle | launchd, systemd-user, and Task Scheduler scripts tested | Clean-machine installation smoke |
| Release artifacts | CI builds macOS Intel/ARM, Linux x86_64, and Windows x86_64 archives | Signing/notarization, SBOM, provenance |

Supported build targets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

ARM Linux and ARM Windows require native CI and release-smoke coverage before becoming
published targets. Herdr's own Windows support remains beta.
