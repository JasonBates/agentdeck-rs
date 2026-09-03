# Troubleshooting

| Symptom | Check | Current action |
|---|---|---|
| `agentdeck` is not found | Confirm the installer directory is on `PATH` | Run the absolute path printed by the installer, then add that directory to your shell or PowerShell `PATH`. |
| The bridge cannot see agents | Run `herdr status` and verify the selected `herdr.session` or `herdr.socket` | Herdr is required; AgentDeck does not start it. |
| `agentdeck doctor` reports a problem | Run `agentdeck doctor --json` and inspect the named check | Doctor is read-only; correct the reported Herdr, bind, optional-provider, config, or path condition, then rerun it. |
| A background service does not start | Run the matching `service.sh` or `service.ps1` command with its recorded binary/config paths | The lifecycle scripts validate generated definitions and refuse a foreign or modified service receipt. |
| The page loads remotely but says "This address is not configured on the bridge", or `/api/health` returns `403 origin_rejected` | Compare the address in the browser with `server.public_host` and `security.allowed_origins` | The bridge answers only for addresses it has been told about. Add the lines the page prints, restart AgentDeck, and reload. See [Remote access with Tailscale](remote-access.md). |
| Nothing opens remotely | Confirm AgentDeck listens on loopback, the TLS proxy targets port 9798, and the route exists (`tailscale serve status`) | AgentDeck does not create or modify proxy configuration. |
| No generated headings | Check `[headings]` and a local Ollama endpoint/model | Herdr-only fallback retains deterministic titles. For the recommended enriched experience, configure an installed model; `qwen3.5:4b` is the lightweight documented example and any installed tag remains valid. |
| No quota panel | Check `[capacity]` and whether CodexBar is already available | Capacity is optional. With `auto`, a missing provider stays hidden; AgentDeck never installs or authenticates it. A successful probe begins after one second and repeats every five minutes. |
| Copilot card has no generated headings or context | Confirm Herdr reports `agent = copilot` with a safe `kind = id` session and that `transcripts.enabled` is true | AgentDeck best-effort reads only the matching local `session-state/<id>/events.jsonl`; it never reads the Copilot DB, invokes the CLI, or uses cloud access. Invalid, missing, malformed, or changed event data remains a generic Herdr card. |
| Tab title does not change | Check `[tab_titles].enabled` and platform capability | Sync is off by default and supported on Unix only. It ignores multi-agent or manually named tabs and compares the current Herdr tab/agent identity before rename; a manual rename in the final non-CAS race is released on the next pass. |
| Windows behaves differently | Check the exact stable Herdr version and its Windows documentation | Windows Herdr support is beta. CI covers native Rust build/tests plus PowerShell installer/archive and Task Scheduler lifecycle checks, but not a live Windows Herdr named-pipe/protocol smoke. |

For bug reports, include the platform, `agentdeck version`, redacted `agentdeck config print`, and the Herdr version. Do not attach transcripts, bearer tokens, or private prompt text.
