# AgentDeck HTTP and SSE contract

AgentDeck serves one embedded browser dashboard and a small local API. The default
listener is `127.0.0.1:9798`; every route also works beneath a configured base path.

## Routes

| Request | Response |
|---|---|
| `GET /` or `GET /index.html` | Embedded dashboard, `text/html; charset=utf-8` |
| `GET /api/snapshot` | Current `DeckPayload`, `application/json`, `Cache-Control: no-store` |
| `GET /events` | Server-sent event stream with an immediate current payload |
| `GET /api/health` | Version, Herdr connectivity, capability state, adapter freshness, and redacted reasons |
| `POST /api/focus` | Focus `{"paneId":"..."}` through Herdr |
| `POST /api/workspace` | Focus `{"workspaceId":"..."}` through Herdr |
| `POST /api/tab` | Create and focus a tab for `{"workspaceId":"..."}` through Herdr |

Successful mutations return `200 {"ok":true}`. Invalid JSON, content type, or
identifiers return `400`. A known action that Herdr cannot complete returns
`503 {"ok":false,"error":"herdr_unavailable"}`. Unknown routes return `404` and
known routes with the wrong method return `405`.

Request bodies are capped at 16 KiB. Header size, count, and read time are bounded.
Identifiers reject empty, overlong, control-character, and NUL-containing values.
Herdr actions are argv-only subprocess calls; user-controlled values never pass through
a shell.

## SSE

Every event is the default unnamed `message` event:

```text
data: <one-line JSON payload>

```

There is no `event`, `id`, or `retry` field. A newly connected client immediately
receives the current state. Changed payloads publish immediately; an unchanged current
payload is republished every five seconds as the liveness signal used by the browser.

Each connection holds only the latest pending state, so a slow client loses intermediate
snapshots instead of growing memory. The server limits concurrent clients, releases
disconnects, and performs a bounded graceful shutdown. Exact framing fixtures live in
`Tests/golden/sse/`.

## Security

Loopback mode requires no token. A non-loopback listener requires:

- a trimmed bearer token of at least 32 UTF-8 bytes;
- exact HTTP(S) origins in `security.allowed_origins`;
- TLS supplied by a trusted local reverse proxy or network layer.

Tokens are accepted only in the `Authorization` header, never in URLs. The browser keeps
an entered token in tab-scoped session storage and uses authenticated fetch streaming
instead of `EventSource`, which cannot set that header.

Responses use same-origin policy by default and include CSP, `nosniff`, no-referrer, and
frame protection. Authentication failures are `401`; rejected origins are `403`.

`/api/health` contains stable codes and redacted paths, never transcript text, generated
headings, bearer tokens, provider output, session IDs, or terminal content.
