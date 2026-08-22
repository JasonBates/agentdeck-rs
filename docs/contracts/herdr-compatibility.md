# Herdr compatibility contract

AgentDeck treats the Herdr snapshot as authoritative. Events only invalidate the current
view and request another snapshot; they are never folded into a second state model.

## Supported baseline

| Platform | Minimum Herdr | Protocol | Status |
|---|---:|---:|---|
| macOS/Linux | 0.8.0 | 19 | Accepted baseline |
| Windows | 0.8.2 | 20 | Required baseline; Herdr Windows remains beta |

Sanitized schema-header, consumed-shape, and snapshot fixtures for protocols 19 and 20
live under `Tests/fixtures/herdr/`. AgentDeck reports both the Herdr client/schema version
and the live snapshot version/protocol. A future protocol may continue with a warning only
when every required consumed field still decodes.

## CLI boundary

AgentDeck uses argv-only Herdr CLI calls:

```text
herdr api snapshot
herdr agent focus <pane-id>
herdr workspace focus <workspace-id>
herdr tab create --workspace <workspace-id> --focus
herdr tab rename <tab-id> <title>
herdr agent read <pane-id> --source visible --lines <16-or-40> --format text
```

Unknown JSON fields and unknown agent kinds are preserved or ignored safely. A missing
required field fails the feed rather than producing invented state. Every subprocess has
a timeout, bounded stdout/stderr, concurrent stream draining, a concurrency limit, and a
supervisor that terminates and reaps the child on cancellation or limit breach.

| Command class | Timeout | stdout cap | stderr cap | Concurrency |
|---|---:|---:|---:|---:|
| Version | 2 s | 64 KiB | 64 KiB | bounded diagnostic |
| Schema | 5 s | 2 MiB | 256 KiB | bounded diagnostic |
| Snapshot | 12 s | 4 MiB | 256 KiB | serialized |
| Mutations | 12 s | 512 KiB | 256 KiB | 4 |
| Visible reads | 12 s | 512 KiB | 256 KiB | 8 |

## Event subscription

Raw local IPC is used only for `events.subscribe`. Requests and responses are newline-
delimited JSON. AgentDeck subscribes to the unparameterized workspace, worktree, tab, and
pane events that can change the consumed snapshot; it does not subscribe to layouts or
high-volume parameterized output/scroll streams.

A matching `subscription_started` acknowledgement is required within five seconds. The
reader accepts split/coalesced frames, rejects invalid UTF-8 and overlong unterminated
frames, and reconnects with bounded exponential backoff plus jitter. Polling continues
throughout event-channel failure, so a missed event cannot become permanent deck state.

Invalidations use a fixed 30 ms coalescing window. Snapshot reconciliations do not overlap;
an event arriving during a poll records one dirty follow-up.

## Routing

Routing precedence is:

1. Explicit `herdr.session`.
2. Explicit `herdr.socket`.
3. Inherited `HERDR_SOCKET_PATH`.
4. Inherited `HERDR_SESSION`.
5. Herdr's platform default.

Configured session and socket values are mutually exclusive and conservatively validated.
AgentDeck never creates, removes, or rewrites Herdr routing markers.

macOS/Linux use Unix local sockets. Windows uses Herdr's namespaced local-socket mapping
backed by a named pipe and matches Herdr's `interprocess` name conversion. Native Windows
testing against a live Herdr 0.8.2+ session remains a release gate; Unix mocks and protocol
fixtures are necessary but not sufficient evidence.
