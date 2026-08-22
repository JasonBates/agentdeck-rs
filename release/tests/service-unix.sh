#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/agentdeck-service-test.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
binary="$scratch/agentdeck & binary"
config="$scratch/config % \$dollar.toml"
printf '#!/usr/bin/env bash\nexit 0\n' > "$binary"
chmod 755 "$binary"
touch "$config"
case "$(uname -s)" in
  Darwin)
    output="$scratch/agentdeck.plist"
    bash "$root/release/service.sh" install --binary "$binary" --config "$config" --render "$output"
    plutil -lint "$output"
    [[ "$(plutil -extract ProgramArguments.0 raw "$output")" == "$binary" ]]
    [[ "$(plutil -extract ProgramArguments.3 raw "$output")" == "$config" ]]
    ;;
  Linux)
    output="$scratch/agentdeck.service"
    bash "$root/release/service.sh" install --binary "$binary" --config "$config" --render "$output"
    command -v systemd-analyze >/dev/null && systemd-analyze verify "$output"
    grep -Fq 'ExecStart=' "$output"
    grep -Fq '$$dollar' "$output"
    grep -Fq '%%' "$output"
    ;;
  *) echo 'unsupported test host' >&2; exit 1 ;;
esac

# A matching service path without an AgentDeck service receipt must never be claimed.
home="$scratch/service collision home"
service_receipt="$scratch/service-receipt"
case "$(uname -s)" in
  Darwin)
    foreign_service="$home/Library/LaunchAgents/com.agentdeck.agentdeck.plist"
    ;;
  Linux)
    foreign_service="$home/config/systemd/user/agentdeck.service"
    ;;
esac
mkdir -p "$(dirname "$foreign_service")"
printf foreign > "$foreign_service"
if HOME="$home" XDG_CONFIG_HOME="$home/config" bash "$root/release/service.sh" install --binary "$binary" --config "$config" --receipt "$service_receipt" --dry-run; then
  echo 'foreign service path was accepted without a receipt' >&2; exit 1
fi

# Service removal is receipt-proven, not dependent on a binary that may already be gone.
removal_home="$scratch/service removal home"
removal_receipt="$scratch/removal-receipt"
case "$(uname -s)" in
  Darwin)
    removal_service="$removal_home/Library/LaunchAgents/com.agentdeck.agentdeck.plist"
    removal_platform='macos'
    ;;
  Linux)
    removal_service="$removal_home/config/systemd/user/agentdeck.service"
    removal_platform='linux'
    ;;
esac
mkdir -p "$(dirname "$removal_service")"
if [[ "$removal_platform" == macos ]]; then
  HOME="$removal_home" bash "$root/release/service.sh" install --binary "$binary" --config "$config" --render "$removal_service"
else
  HOME="$removal_home" XDG_CONFIG_HOME="$removal_home/config" bash "$root/release/service.sh" install --binary "$binary" --config "$config" --render "$removal_service"
fi
removal_hash="$(shasum -a 256 "$removal_service" | awk '{print $1}')"
mkdir -p "$(dirname "$removal_receipt")"
printf 'schema=1\nplatform=%s\nservice_path=%s\ndefinition_sha256=%s\nbinary=%s\nconfig=%s\n' \
  "$removal_platform" "$removal_service" "$removal_hash" "$binary" "$config" > "$removal_receipt"
rm -f -- "$binary"
if [[ "$(uname -s)" == Darwin ]]; then
  HOME="$removal_home" bash "$root/release/service.sh" uninstall --binary "$binary" --config "$config" --receipt "$removal_receipt" --dry-run >/dev/null
else
  HOME="$removal_home" XDG_CONFIG_HOME="$removal_home/config" bash "$root/release/service.sh" uninstall --binary "$binary" --config "$config" --receipt "$removal_receipt" --dry-run >/dev/null
fi
