# Security policy

## Reporting a vulnerability

Please report vulnerabilities through the
[AgentDeck Security tab](https://github.com/JasonBates/agentdeck-rs/security). If GitHub private
vulnerability reporting is enabled, use that form. If it is not available, do not post exploit
details, proof-of-concept code, tokens, transcripts, or private paths in a public issue; use a
minimal public issue only to report that a private channel is needed. No alternative confidential
reporting channel is currently published; wait for a maintainer to provide one before sharing
sensitive details.

Include the affected version/commit, platform, concise reproduction conditions, impact, and any
safe mitigation. Redact all credentials, bearer tokens, cookies, prompts, replies, transcript or
screen text, session/pane/workspace IDs, and local paths.

## Supported versions

Once releases exist, the latest released AgentDeck version is the supported version for security
fixes. Older releases may receive guidance but are not guaranteed fixes. Before the first release,
the repository's current development revision is the only version under active maintenance.

## Security boundaries

AgentDeck listens on `127.0.0.1` by default. A non-loopback listener requires an explicit bearer
token and exact allowed origins, and should be placed behind TLS. Optional integrations are local:
AgentDeck does not install, start, authenticate, or download Herdr, Ollama, CodexBar, or Copilot.

The public release process does not yet sign or notarize artifacts. Verify release checksums and
inspect installers before use; do not treat an unsigned artifact as equivalent to a signed,
notarized distribution.

## Disclosure

Please allow maintainers time to reproduce, patch, and publish a coordinated fix before sharing
technical details publicly. We will acknowledge a report after triage when a private reporting
channel is available.
