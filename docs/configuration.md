# Configuration

The executable reads TOML from `~/.config/agentdeck/config.toml` on macOS/Linux (or
`$XDG_CONFIG_HOME/agentdeck/config.toml`) and `%APPDATA%\agentdeck\config.toml` on
Windows. A missing file is valid: defaults are used. Run `agentdeck config init` to
create a secure minimal file, `agentdeck config init --stdout` to review it without a
write, and `agentdeck config print` to inspect the effective configuration (with
`security.auth_token` redacted).

`agentdeck config init` refuses to overwrite an existing file. Use `--force` only when
you intend to replace it. The generated file leaves headings in inert `auto` mode with no
model tag: the dashboard can recommend enrichment, but initialization cannot probe, pull,
or load a model until a tag is explicitly configured.
Pass a non-default path before the command when needed:

```bash
agentdeck --config /path/to/config.toml config init --stdout
agentdeck --config /path/to/config.toml config init
agentdeck --config /path/to/config.toml config init --force
```

Herdr alone is the graceful fallback. Local-model headings are the recommended full
experience because they provide the stable title, current-step subtitle, and outcome that
make several sessions scannable. `gemma4:12b` is the development reference model;
`qwen3.5:4b` is the current lightweight documented example. Neither is an allow-list:
`model` and every per-job override accept any installed Ollama tag. AgentDeck never pulls,
selects, or starts a model or provider.

```toml
[server]
listen = "127.0.0.1:9798"
base_path = "/"
reconcile_interval = "1s"

[herdr]
# session = "default" # alternatively: socket = "..."; never both

[transcripts]
enabled = true # set false to prevent all local transcript-file reads

[headings]
backend = "auto" # auto, none, or ollama
endpoint = "http://127.0.0.1:11434"
# model = "qwen3.5:4b" # lightweight example; any installed tag is valid
title_model = "inherit" # inherit, off, or another model tag
subtitle_model = "inherit"
outcome_model = "inherit"
activity_model = "inherit"
names = "fallback" # all or fallback

[capacity]
backend = "auto" # auto, off, or codexbar

[telemetry]
host = "auto" # auto, off, basic, or detailed
local_model = "auto" # auto, on, or off

[tab_titles]
enabled = false

[security]
allowed_origins = []
# auth_token = "a-random-token-at-least-32-bytes-for-a-remote-listener"
```

Capacity and telemetry are local runtime panels. Capacity refreshes after one second and then
every five minutes; host and local-model readings refresh every five seconds. A capacity result
may retain its last known value as stale, but unavailable data is never presented as zero.
AgentDeck does not start CodexBar, collect credentials, or pull a model. Portable host telemetry
is basic only; `host = "detailed"` reports unsupported. Local-model telemetry uses the configured
primary `headings.model` through the local Ollama `/api/ps` endpoint, not per-job overrides.

`tab_titles.enabled` is false by default. On Unix, enabling it allows a separate guarded worker
to synchronize only qualifying single-agent default tabs. It takes a fresh Herdr snapshot and
checks the exact tab, agent identity, and current label immediately before each rename. Herdr has
no compare-and-set rename, so a manual rename after that check is an unavoidable final race; the
next observation releases ownership. Windows title sync reports unsupported pending native ACL
and reparse-point validation.

Copilot enrichment is best-effort and read-only: only a validated Herdr Copilot session ID may
locate `session-state/<id>/events.jsonl` below the local Copilot home (or an absolute
`COPILOT_HOME`). It derives reply age and context when safe records are available; it never reads
`session-store.db`, invokes the Copilot CLI, or contacts a cloud service. Copilot transcript
content is not used for generated headings. See GitHub's [Copilot CLI configuration-directory
reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference).

Transcript enrichment is enabled by default to preserve AgentDeck's existing context, unread,
reply-age, and generated-heading behavior for supported agents. Set `transcripts.enabled = false`
to prevent all local transcript-file access; cards then use Herdr and screen-derived data only.

Precedence is command-line serve flags, then compatible `AGENTDECK_*` environment
variables, then TOML, then defaults. The implemented flags are `--port`, `--interval`,
`--model`, and `--title-model`, all under `agentdeck serve`. Implemented compatibility
environment variables are `AGENTDECK_PORT`, `AGENTDECK_INTERVAL`, `AGENTDECK_MODEL`,
`AGENTDECK_TITLE_MODEL`, `AGENTDECK_NAMES`, `AGENTDECK_TAB_TITLES`, `AGENTDECK_PUBLIC`,
and `AGENTDECK_PUBLIC_HOST`.

A non-loopback `server.listen` requires `security.auth_token`; its length must be at
least 32 bytes. Every `security.allowed_origins` entry must be an exact canonical HTTP(S)
origin without a wildcard. `server.public_dir` is intended only for loopback development;
production web assets are embedded.

## Diagnostics

`agentdeck doctor` is read-only: it does not install providers, load models,
authenticate, write state, or contact the AgentDeck HTTP service. It reports the config
path/status, Herdr executable/version/protocol/event endpoint, whether the configured
listener can bind, configured Ollama availability, CodexBar availability, and state/cache
paths. Use `agentdeck doctor --json` for automation; paths in its output are redacted
relative to the home directory and it contains no transcript or bearer-token content.
