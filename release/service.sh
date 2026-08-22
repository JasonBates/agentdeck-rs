#!/usr/bin/env bash
# Safe per-user service lifecycle for macOS launchd and Linux systemd-user.
set -euo pipefail

mode=''
binary="${AGENTDECK_BINARY:-${HOME}/.local/bin/agentdeck}"
config="${AGENTDECK_CONFIG:-${XDG_CONFIG_HOME:-${HOME}/.config}/agentdeck/config.toml}"
receipt="${AGENTDECK_SERVICE_RECEIPT:-${XDG_STATE_HOME:-${HOME}/.local/state}/agentdeck/service/receipt}"
render_path=''
dry_run=false
label='com.agentdeck.agentdeck'

usage() {
  cat <<'EOF'
Usage: service.sh install|uninstall [OPTIONS]

Create or remove only an AgentDeck-owned per-user service. Existing service files
are refused unless the service receipt and generated-definition hash match.

Options:
  --binary PATH     AgentDeck executable (default: ~/.local/bin/agentdeck)
  --config PATH     AgentDeck config path
  --receipt PATH    service ownership receipt (advanced/testing)
  --render PATH     generate and validate a definition without installing it
  --dry-run         print intended lifecycle commands without changing state
  -h, --help        show this help
EOF
}
die() { echo "error: $*" >&2; exit 1; }
while (($#)); do
  case "$1" in
    install|uninstall) [[ -z "$mode" ]] || die 'provide one lifecycle action'; mode="$1"; shift ;;
    --binary) binary="${2:-}"; shift 2 ;;
    --config) config="${2:-}"; shift 2 ;;
    --receipt) receipt="${2:-}"; shift 2 ;;
    --render) render_path="${2:-}"; shift 2 ;;
    --dry-run) dry_run=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done
[[ -n "$mode" ]] || { usage >&2; exit 64; }
for path in "$binary" "$config" "$receipt" "$render_path"; do [[ "$path" != *$'\n'* && "$path" != *$'\r'* ]] || die 'paths may not contain newlines'; done
if [[ "$mode" == install ]]; then
  [[ -x "$binary" ]] || die "AgentDeck executable is not executable: ${binary}"
fi

if command -v sha256sum >/dev/null 2>&1; then hash_command=(sha256sum); elif command -v shasum >/dev/null 2>&1; then hash_command=(shasum -a 256); else die 'sha256sum or shasum is required'; fi
hash_file() { "${hash_command[@]}" "$1" | awk '{print $1}'; }
receipt_value() { awk -F= -v key="$1" '$1 == key { value = substr($0, length(key) + 2) } END { print value }' "$receipt"; }
write_receipt() {
  receipt_dir="$(dirname "$receipt")"
  mkdir -p "$receipt_dir"
  chmod 700 "$receipt_dir"
  temporary_receipt="${receipt_dir}/.receipt.new.$$"
  (
    umask 077
    printf 'schema=1\nplatform=%s\nservice_path=%s\ndefinition_sha256=%s\nbinary=%s\nconfig=%s\n' \
      "$platform" "$service_path" "$(hash_file "$service_path")" "$binary" "$config" > "$temporary_receipt"
  )
  chmod 600 "$temporary_receipt"
  mv -f "$temporary_receipt" "$receipt"
}
receipt_matches_definition() {
  [[ -f "$receipt" ]] || return 1
  [[ "$(receipt_value schema)" == '1' && "$(receipt_value platform)" == "$platform" && "$(receipt_value service_path)" == "$service_path" ]] || return 1
  expected_hash="$(receipt_value definition_sha256)"
  [[ "$expected_hash" =~ ^[0-9a-f]{64}$ && -f "$service_path" && "$(hash_file "$service_path")" == "$expected_hash" ]]
}

platform=''
case "$(uname -s)" in
  Darwin)
    platform='macos'
    service_path="${HOME}/Library/LaunchAgents/${label}.plist"
    log_dir="${HOME}/Library/Logs/AgentDeck"
    generate_definition() {
      output="$1"
      command -v plutil >/dev/null 2>&1 || die 'plutil is required to create a LaunchAgent definition'
      plutil -create xml1 "$output"
      plutil -insert Label -string "$label" "$output"
      plutil -insert ProgramArguments -array "$output"
      plutil -insert ProgramArguments.0 -string "$binary" "$output"
      plutil -insert ProgramArguments.1 -string serve "$output"
      plutil -insert ProgramArguments.2 -string --config "$output"
      plutil -insert ProgramArguments.3 -string "$config" "$output"
      plutil -insert RunAtLoad -bool true "$output"
      plutil -insert KeepAlive -bool true "$output"
      plutil -insert StandardOutPath -string "${log_dir}/agentdeck.log" "$output"
      plutil -insert StandardErrorPath -string "${log_dir}/agentdeck.log" "$output"
      plutil -lint "$output" >/dev/null
      [[ "$(plutil -extract Label raw "$output")" == "$label" ]] || die 'generated LaunchAgent label did not validate'
      [[ "$(plutil -extract ProgramArguments.0 raw "$output")" == "$binary" ]] || die 'generated LaunchAgent binary did not validate'
    }
    install_service() {
      if [[ -n "$render_path" ]]; then generate_definition "$render_path"; printf 'Rendered validated LaunchAgent: %s\n' "$render_path"; return; fi
      if [[ -e "$service_path" && ! -f "$service_path" ]]; then die "service path is not a regular file: ${service_path}"; fi
      if [[ -f "$service_path" ]] && ! receipt_matches_definition; then die "refusing to replace foreign or modified LaunchAgent: ${service_path}"; fi
      if "$dry_run"; then printf 'Would create %s and run launchctl bootstrap gui/%s %s\n' "$service_path" "$(id -u)" "$service_path"; return; fi
      mkdir -p "$(dirname "$service_path")" "$log_dir"
      chmod 700 "$log_dir"
      temporary="$(mktemp "${service_path}.new.XXXXXX")"
      generate_definition "$temporary"
      mv -f "$temporary" "$service_path"
      launchctl bootout "gui/$(id -u)" "$service_path" >/dev/null 2>&1 || true
      launchctl bootstrap "gui/$(id -u)" "$service_path"
      write_receipt
      printf 'Installed and started AgentDeck LaunchAgent: %s\n' "$service_path"
    }
    uninstall_service() {
      receipt_matches_definition || die 'LaunchAgent receipt or definition proof does not match; refusing to remove it'
      [[ "$(plutil -extract Label raw "$service_path")" == "$label" ]] || die 'LaunchAgent label proof does not match; refusing to remove it'
      if "$dry_run"; then printf 'Would run launchctl bootout gui/%s %s and remove receipt-proven service\n' "$(id -u)" "$service_path"; return; fi
      launchctl bootout "gui/$(id -u)" "$service_path" >/dev/null 2>&1 || true
      rm -f -- "$service_path" "$receipt"
      printf 'Removed AgentDeck LaunchAgent; retained binary, config, state, and logs.\n'
    }
    ;;
  Linux)
    platform='linux'
    service_path="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user/agentdeck.service"
    systemd_quote() {
      value="$1"
      [[ "$value" != *$'\n'* && "$value" != *$'\r'* ]] || die 'systemd arguments may not contain newlines'
      value=${value//\\/\\\\}
      value=${value//\"/\\\"}
      value=${value//\$/\$\$}
      value=${value//%/%%}
      printf '"%s"' "$value"
    }
    generate_definition() {
      output="$1"
      {
        printf '[Unit]\nDescription=AgentDeck browser bridge\nAfter=default.target\n\n[Service]\nType=simple\n'
        printf 'ExecStart=%s serve --config %s\n' "$(systemd_quote "$binary")" "$(systemd_quote "$config")"
        printf 'Restart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n'
      } > "$output"
      command -v systemd-analyze >/dev/null 2>&1 && systemd-analyze verify "$output" >/dev/null
      grep -Fqx "ExecStart=$(systemd_quote "$binary") serve --config $(systemd_quote "$config")" "$output" || die 'generated systemd unit did not validate'
    }
    install_service() {
      if [[ -n "$render_path" ]]; then generate_definition "$render_path"; printf 'Rendered validated systemd user unit: %s\n' "$render_path"; return; fi
      command -v systemctl >/dev/null 2>&1 || die 'systemctl is required for the Linux user service; use foreground serve instead'
      if [[ -e "$service_path" && ! -f "$service_path" ]]; then die "service path is not a regular file: ${service_path}"; fi
      if [[ -f "$service_path" ]] && ! receipt_matches_definition; then die "refusing to replace foreign or modified systemd unit: ${service_path}"; fi
      if "$dry_run"; then printf 'Would create %s and run systemctl --user enable --now agentdeck\n' "$service_path"; return; fi
      mkdir -p "$(dirname "$service_path")"
      temporary="$(mktemp "${service_path}.new.XXXXXX")"
      generate_definition "$temporary"
      mv -f "$temporary" "$service_path"
      systemctl --user daemon-reload
      systemctl --user enable --now agentdeck
      write_receipt
      printf 'Installed and started AgentDeck systemd user service: %s\n' "$service_path"
    }
    uninstall_service() {
      receipt_matches_definition || die 'systemd receipt or unit proof does not match; refusing to remove it'
      if "$dry_run"; then printf 'Would run systemctl --user disable --now agentdeck and remove receipt-proven unit\n'; return; fi
      systemctl --user disable --now agentdeck
      rm -f -- "$service_path" "$receipt"
      systemctl --user daemon-reload
      printf 'Removed AgentDeck systemd user service; retained binary, config, state, and logs.\n'
    }
    ;;
  *) die "unsupported service platform: $(uname -s)" ;;
esac

if [[ "$mode" == install ]]; then install_service; else uninstall_service; fi
