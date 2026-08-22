# AgentDeck fixture contract

Fixtures are public contract evidence, not debug dumps. Every committed fixture
must be synthetic or manually redacted before it enters the repository.

## Layout

- `deck/`: browser payloads representing meaningful states.
- `sse/`: exact SSE frame bytes built from sanitized deck payloads.
- `../fixtures/contract/`: synthetic payloads exercising the public wire contract.
- `Tests/fixtures/herdr/`: sanitized protocol 19 and 20 schema/snapshot subsets.
- Future `Tests/fixtures/transcript/`: minimal Claude, Codex, and Pi JSONL records.

Do not copy raw output from Herdr, CodexBar, Ollama, a
transcript file, or a terminal screen into this tree. Inspect live services only
to learn shape and behavior, then construct a synthetic equivalent.

## Forbidden content

- Real prompt, response, transcript, terminal-screen, session, pane, workspace,
  repository, branch, remote, provider-account, or model data.
- Real usernames, home paths, email addresses, hostnames, IP addresses, Tailscale
  names, machine identifiers, timestamps tied to activity, or repository remotes.
- API keys, bearer tokens, cookies, authorization headers, environment dumps,
  signing identities, or credential-shaped placeholders.

Use obvious neutral values such as `pane-1`, `workspace-1`,
`/workspace/example-project`, `example-model`, and fixed Unix timestamps. A fixture
must contain only the fields needed to establish its behavior.

## Format rules

- JSON and JSONL are UTF-8. JSON fixtures parse without extensions or comments.
- Absent optionals are omitted; they are not represented as JSON `null`.
- SSE message fixtures contain `data: `, exactly one compact JSON line, and two
  trailing LF bytes. Liveness is a repeated data event, not a comment.
- Arrays preserve the behaviorally relevant order.
- Numeric spelling is frozen deliberately where exact bytes matter.
- Fixture changes require review as contract changes, not snapshot churn.

Rust tests scan machine-readable fixtures for private and credential markers and
validate SSE framing; automated checks do not replace human review of meaningful text.
