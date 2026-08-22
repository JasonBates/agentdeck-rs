# Privacy and local data

AgentDeck has no analytics, telemetry upload, crash upload, or cloud-model fallback by
default. Herdr is its required local state authority; no local model is required or selected
by default.

Transcript adapters are enabled by default and may read local Claude Code, Codex, or Pi
transcript material to derive card context, headings, reply timing, and context usage. Copilot
is narrower: with a validated Herdr session ID, AgentDeck can read only local
`session-state/<id>/events.jsonl` records to derive reply timing and context usage. It does not
read Copilot's `session-store.db`, invoke the Copilot CLI, or use cloud access; invalid,
unavailable, malformed, or unsafe session data leaves the card generic. Copilot content is not
used for generated headings.

Set `transcripts.enabled = false` in AgentDeck's configuration to prevent every local
transcript-file read. Herdr state and bounded visible-screen parsing remain active so generic
cards, status, grouping, focus, phases, and background-work indicators still function.

The recommended full experience uses Ollama headings through the configured loopback endpoint;
no model is downloaded, pulled, or started automatically. Herdr-only mode remains a graceful
fallback. Capacity, host, and local-model telemetry are separate local runtime panels and do not
send data away from the machine. Host telemetry is portable basic only; detailed host telemetry
is honestly unsupported. AgentDeck does not persist heading prompts, provider responses,
transcript contents, or visible-screen contents.

Use the default loopback listener. A direct remote listener requires an explicit bearer
token and exact allowed origins, and should sit behind TLS. `/api/health` is designed not
to include prompt or session text; logs should not include transcript contents by default.
