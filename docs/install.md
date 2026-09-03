# Install, services, upgrade, and rollback

Herdr is the only required runtime dependency. AgentDeck's installers do not install,
configure, start, or authenticate Herdr, Ollama, CodexBar, proxies, or other services.

Install Herdr using its [official platform instructions](https://herdr.dev/docs/install/),
then run `herdr` once to launch or attach the session AgentDeck will display. Native Windows
Herdr is currently a preview beta; AgentDeck's Windows support carries the same limitation.

## Install from source (current method)

No packaged release exists yet, so building from source is the supported installation
today. Install Rust 1.85 or newer through [rustup](https://rustup.rs/), then:

```bash
git clone https://github.com/JasonBates/agentdeck-rs.git
cd agentdeck-rs
cargo install --path crates/agentdeck --locked
```

`cargo install` builds the release profile and places the `agentdeck` binary in
`~/.cargo/bin` on macOS/Linux or `%USERPROFILE%\.cargo\bin` on Windows. rustup adds that
directory to `PATH`; open a new shell if `agentdeck` is not found. Only the `agentdeck`
binary is installed. The test-helper binary in the same crate is behind a feature flag and
is never built by this command.

Keep the checkout. The service scripts below run from its `release/` directory, and later
updates reuse it.

```bash
agentdeck config init
agentdeck doctor
agentdeck serve
```

Open `http://127.0.0.1:9798`.

A source install has no installation receipt, so the release `uninstall.sh` and
`uninstall.ps1` scripts do not apply to it. Manage it with Cargo and Git instead:

| Task | Command |
|---|---|
| Upgrade | `git pull` then `cargo install --path crates/agentdeck --locked --force` |
| Roll back | `git checkout <commit>` then the same `cargo install ... --force` |
| Remove | stop the service (below), then `cargo uninstall agentdeck` |

`--force` is required because the crate version does not change between commits.
Configuration, state, caches, and logs are never removed by these commands.

## Prebuilt releases (not yet published)

**There is no GitHub release yet.** Everything in this section describes the intended
flow for the first tagged release and fails today, because the installers look up
release assets that do not exist. Use the source install above until a tag is published.

Once a release exists: download the platform installer, review it if desired, then run it.
It downloads the matching archive and `SHA256SUMS`, refuses a mismatched checksum,
atomically replaces the receipt-owned binary, and writes a private ownership receipt.

The checksum detects an archive that does not match the published manifest. Release
artifacts and `SHA256SUMS` are not yet signed, and macOS artifacts are not notarized, so
checksum verification is not publisher authentication. Pin a release tag, inspect the
installer, and see the [security policy](../SECURITY.md) before using an unsigned release.

macOS/Linux (`v0.1.0` stands for the first tag once it exists):

```bash
version=v0.1.0
curl -fLo install.sh "https://raw.githubusercontent.com/JasonBates/agentdeck-rs/$version/release/install.sh"
bash install.sh --version "$version"
```

Windows PowerShell:

```powershell
$Version = 'v0.1.0'
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/JasonBates/agentdeck-rs/$Version/release/install.ps1" -OutFile .\install.ps1
.\install.ps1 -Version $Version
```

The default binary directories are `~/.local/bin` on macOS/Linux and
`%LOCALAPPDATA%\AgentDeck\bin` on Windows. Add the former to `PATH` if it is not already
there.

Windows is a beta lane because Herdr's native Windows support is beta. The CI workflow is
configured to prove a native Rust build/test, PowerShell install/uninstall and archive-layout
checks, and Task Scheduler lifecycle coverage. It does not yet prove a live Windows Herdr
named-pipe/protocol session; do not treat a packaged Windows binary as that missing smoke test.

Existing same-name files are never overwritten by default. An upgrade is allowed only
when the recorded receipt and installed file hashes match; inspect any collision and use
`--force` (Unix) or `-Force` (PowerShell) only if you intentionally want to take ownership
of the replacement.

## Pinned rollback or upgrade (releases)

Without a version, the installer uses the latest GitHub release. Select a specific release
tag for a repeatable upgrade or rollback:

```bash
bash install.sh --version v0.1.0
```

```powershell
.\install.ps1 -Version v0.1.0
```

Each receipt records the release version, target, archive SHA-256, installed binary hash,
and installation directory. Advanced/offline use can pass a local archive plus matching
`SHA256SUMS` through the documented installer help (`--help` / `Get-Help`).

## Foreground start and diagnostics

```bash
agentdeck config init
agentdeck doctor
agentdeck serve
```

The default listener is `127.0.0.1:9798`. Keep it on loopback unless a deliberate
TLS-protected remote configuration supplies a valid bearer token and exact allowed origins.
`agentdeck doctor --json` provides a redacted, machine-readable local diagnostic report.

## Retain the service and uninstall tools

Source installs skip this section: the same scripts are already in the checkout's
`release/` directory, so run them from there.

The binary installer deliberately installs only `agentdeck` / `agentdeck.exe`; its
temporary archive extraction is deleted when installation finishes. Service and uninstall
tools remain in the release archive so they cannot silently add themselves to the system.

Before using the sections below, download and extract the archive for the exact release and
target you installed. Keep the extracted directory until you no longer need that release:

```bash
# Replace both values with the installed receipt's version and target.
version=v0.1.0
target=aarch64-apple-darwin
curl -fLO "https://github.com/JasonBates/agentdeck-rs/releases/download/$version/agentdeck-$target.tar.gz"
curl -fLO "https://github.com/JasonBates/agentdeck-rs/releases/download/$version/SHA256SUMS"
# Compare the matching SHA256SUMS entry with: shasum -a 256 "agentdeck-$target.tar.gz"
mkdir "agentdeck-$version-$target"
tar -xzf "agentdeck-$target.tar.gz" -C "agentdeck-$version-$target"
cd "agentdeck-$version-$target"
```

```powershell
$Version = 'v0.1.0'
$Target = 'x86_64-pc-windows-msvc'
$Archive = "agentdeck-$Target.zip"
Invoke-WebRequest -Uri "https://github.com/JasonBates/agentdeck-rs/releases/download/$Version/$Archive" -OutFile $Archive
Invoke-WebRequest -Uri "https://github.com/JasonBates/agentdeck-rs/releases/download/$Version/SHA256SUMS" -OutFile SHA256SUMS
# Compare the matching SHA256SUMS entry with: Get-FileHash -Algorithm SHA256 $Archive
Expand-Archive -LiteralPath $Archive -DestinationPath "agentdeck-$Version-$Target"
Set-Location "agentdeck-$Version-$Target"
```

The installer already verified the archive it consumed. If you download it again, verify
it against that release's `SHA256SUMS` before executing an extracted script. Source-build
users can run the corresponding scripts directly under the checkout's `release/` directory.

## Per-user services

The extracted release archive contains lifecycle scripts that generate platform-native
definitions without text substitution. They validate their generated output, create the
applicable macOS or Windows log directory when needed, and write a service ownership
receipt. They refuse to replace or delete a foreign or modified service/task.

macOS and Linux (release install, then source install):

```bash
./service.sh install --binary "$HOME/.local/bin/agentdeck" --config "$HOME/.config/agentdeck/config.toml"
release/service.sh install --binary "$HOME/.cargo/bin/agentdeck"
./service.sh uninstall
```

On macOS this manages a LaunchAgent and writes logs to `~/Library/Logs/AgentDeck/`.
On Linux it manages a `systemd --user` service; use `journalctl --user -u agentdeck` for
logs. If `systemd --user` is unavailable, keep AgentDeck in the foreground instead.

Windows PowerShell (release install, then source install):

```powershell
.\service.ps1 install -Binary "$env:LOCALAPPDATA\AgentDeck\bin\agentdeck.exe" -Config "$env:APPDATA\agentdeck\config.toml"
.\release\service.ps1 install -Binary "$env:USERPROFILE\.cargo\bin\agentdeck.exe"
.\service.ps1 uninstall
```

The Windows script registers a least-privileged current-user logon task through the
ScheduledTasks API. It does not use `schtasks` string interpolation or require an
administrator. It prepares `%LOCALAPPDATA%\AgentDeck\logs`; use `-Plan` to inspect the
structured action without registering it.

`services/` holds reference layouts for the generated definitions. Do not copy, edit, or
substitute their placeholders; use the lifecycle scripts so paths containing spaces,
quotes, XML characters, `$`, or `%` are handled safely.

## Uninstall

For a source install, stop and remove the service, then run `cargo uninstall agentdeck`;
the scripts below need an installer receipt that a source install does not have.

For a release install, stop and remove the service first, then invoke the matching uninstaller from the retained
or freshly verified release-tools directory described above:

```bash
./uninstall.sh --dry-run
./uninstall.sh
```

```powershell
.\uninstall.ps1 -WhatIf
.\uninstall.ps1
```

The uninstallers remove the binary only when its receipt and recorded SHA-256 proof still
match. They retain configuration, state, caches, logs, services, and all unrelated files.
There is intentionally no automatic purge.
