# Remote access with Tailscale

AgentDeck listens on `127.0.0.1:9798` and stays there. To read the deck on a tablet, a
phone, or another computer, put a TLS proxy in front of that loopback listener and tell
AgentDeck the address the proxy will present it as. Tailscale Serve is the simplest such
proxy: it terminates TLS with a valid certificate, is reachable only from devices in your
tailnet, and the commands are identical on macOS, Linux, and Windows.

## Before you start

- AgentDeck runs and `http://127.0.0.1:9798` works on the bridge machine.
- Tailscale is installed and signed in on the bridge machine and on the device you will
  view from.
- MagicDNS and HTTPS certificates are enabled for your tailnet (admin console, DNS page).
  `tailscale serve` prints an error naming the missing setting if they are not.
- You know the bridge machine's tailnet name. It looks like `studio.tail1234.ts.net`:

  ```bash
  tailscale status --json | grep -m1 '"DNSName"'
  ```

  The first match is this machine; the trailing dot is not part of the name. On Windows,
  run `tailscale status --json | Select-String DNSName | Select-Object -First 1` in
  PowerShell. If `tailscale` is not found there, use the full path
  `& 'C:\Program Files\Tailscale\tailscale.exe'`. The admin console's Machines page
  shows the same name.

## 1. Tell AgentDeck its public address

The bridge answers `/api/*` and `/events` only for addresses it has been configured with:
its own loopback address, `server.public_host`, or an entry in
`security.allowed_origins`. This is what stops a page on another site from talking to the
bridge through your browser. It also means that behind a proxy the page itself loads but
every request from it is refused with `403 origin_rejected`. When that happens the page
shows **This address is not configured on the bridge** together with the exact lines to
add, which are the same as the ones below.

Edit the bridge's configuration (`~/.config/agentdeck/config.toml` on macOS/Linux,
`%APPDATA%\agentdeck\config.toml` on Windows).

For the path form in step 2, which serves the deck on the standard HTTPS port:

```toml
[server]
public_host = "studio.tail1234.ts.net"
```

For the port form in step 2, the address carries a port, and `public_host` does not accept
one. Declare the exact origin instead:

```toml
[security]
allowed_origins = ["https://studio.tail1234.ts.net:9797"]
```

Then restart AgentDeck. In the foreground, stop it with Ctrl-C and run `agentdeck serve`
again. As a service:

```bash
launchctl kickstart -k "gui/$(id -u)/com.agentdeck.agentdeck"   # macOS
systemctl --user restart agentdeck                             # Linux
```

```powershell
Stop-ScheduledTask -TaskName AgentDeck; Start-ScheduledTask -TaskName AgentDeck
```

## 2. Publish it through Tailscale Serve

Run these on the bridge machine. `--bg` keeps the route after the command returns and
across reboots.

**Path form.** The deck lives under a path on the machine's standard HTTPS port, so it
can share that port with anything else you serve:

```bash
tailscale serve --bg --set-path=/deck http://127.0.0.1:9798
```

The deck is then at `https://studio.tail1234.ts.net/deck/`. Serve strips `/deck` before
forwarding, and the page resolves its own links relative to the address it was opened at,
so no `base_path` setting is needed.

**Port form.** The deck gets a dedicated HTTPS port. It must differ from the port
AgentDeck itself listens on, because Serve binds it on the tailnet interface:

```bash
tailscale serve --bg --https=9797 http://127.0.0.1:9798
```

The deck is then at `https://studio.tail1234.ts.net:9797/`.

The commands are the same in Windows PowerShell. `tailscale serve status` lists the active
routes on every platform.

## 3. Open it from another device

Open the address from any device signed in to the same tailnet. Type the `https://`
prefix: Serve only speaks TLS on these ports, and a bare hostname sends most browsers to a
search engine instead.

On an iPad or iPhone, use Share, then **Add to Home Screen** for a full-screen view without
browser chrome.

To check the whole path without a browser:

```bash
curl -s https://studio.tail1234.ts.net/deck/api/health
```

A JSON body means the proxy and the address configuration are both right. A
`403 origin_rejected` body means step 1 does not match the address you used.

## Undo

```bash
tailscale serve --set-path=/deck off
tailscale serve --https=9797 off
```

Removing the `public_host` or `allowed_origins` lines afterwards is optional.

## What this does and does not protect

- Only devices in your tailnet can reach the deck, and the link is encrypted end to end.
- AgentDeck adds no login of its own on this path. Anyone in the tailnet can view every
  card and use the focus, workspace, and new-tab actions. That is fine for a personal
  tailnet and not fine for a shared one.
- Never expose the deck with `tailscale funnel`. Funnel publishes to the public internet,
  and card text is derived from your agents' transcripts.
- Serve adds `Tailscale-User-*` headers identifying the viewer. AgentDeck ignores them.

## Alternative: a direct listener with a token

If you would rather not run Serve, AgentDeck can listen on the machine's Tailscale IP
directly. A non-loopback listener requires a bearer token of at least 32 bytes and the
exact origin the browser will use:

```toml
[server]
listen = "100.101.102.103:9798"

[security]
auth_token = "paste-a-random-token-of-at-least-32-bytes"
allowed_origins = ["http://100.101.102.103:9798"]
```

Generate a token with `openssl rand -hex 32` on macOS/Linux, or in PowerShell with
`-join ((1..48) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })`.

The page asks for the token on first load and keeps it in tab-scoped session storage.
There is no TLS on this listener; between tailnet devices the traffic is still encrypted by
Tailscale itself, but the browser will show the address as insecure. Windows asks once
whether to allow `agentdeck.exe` through the firewall. Prefer the Serve route unless you
have a reason not to.
