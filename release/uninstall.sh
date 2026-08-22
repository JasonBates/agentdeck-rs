#!/usr/bin/env bash
# Remove only a binary proven to be owned by an AgentDeck receipt.
set -euo pipefail

install_dir="${AGENTDECK_INSTALL_DIR:-${HOME}/.local/bin}"
receipt="${AGENTDECK_RECEIPT_PATH:-${XDG_STATE_HOME:-${HOME}/.local/state}/agentdeck/installation/receipt}"
dry_run=false

usage() {
  cat <<'EOF'
Usage: uninstall.sh [--dir PATH] [--receipt PATH] [--dry-run]

Removes AgentDeck only when its receipt and binary SHA-256 still match.
Configuration, state, caches, logs, and service definitions are deliberately
retained.
EOF
}
die() { echo "error: $*" >&2; exit 1; }
while (($#)); do
  case "$1" in
    --dir) install_dir="${2:-}"; shift 2 ;;
    --receipt) receipt="${2:-}"; shift 2 ;;
    --dry-run) dry_run=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done
[[ -n "$install_dir" && -n "$receipt" ]] || die 'directory and receipt must be nonblank'
[[ "$install_dir" != *$'\n'* && "$install_dir" != *$'\r'* && "$receipt" != *$'\n'* && "$receipt" != *$'\r'* ]] || die 'paths may not contain newlines'
[[ -f "$receipt" && ! -L "$receipt" ]] || die "no trusted AgentDeck installation receipt at ${receipt}; refusing to remove files"

if command -v sha256sum >/dev/null 2>&1; then hash_command=(sha256sum); elif command -v shasum >/dev/null 2>&1; then hash_command=(shasum -a 256); else die 'sha256sum or shasum is required'; fi
hash_file() { "${hash_command[@]}" "$1" | awk '{print $1}'; }
receipt_value() { awk -F= -v key="$1" '$1 == key { value = substr($0, length(key) + 2) } END { print value }' "$receipt"; }

binary_path="${install_dir}/agentdeck"
expected_hash="$(receipt_value binary_sha256)"
[[ "$(receipt_value schema)" == '2' ]] || die 'receipt schema is not recognised; refusing to remove files'
[[ "$(receipt_value install_dir)" == "$install_dir" ]] || die 'receipt belongs to a different installation directory; refusing to remove files'
[[ "$expected_hash" =~ ^[0-9a-f]{64}$ && -f "$binary_path" && ! -L "$binary_path" ]] || die 'binary is absent, unsafe, or receipt hash is invalid; refusing to remove files'
[[ "$(hash_file "$binary_path")" == "$expected_hash" ]] || die 'binary no longer matches its receipt; refusing to remove files'

if "$dry_run"; then
  printf 'Would remove %s\nWould remove %s\n' "$binary_path" "$receipt"
  exit 0
fi
rm -f -- "$binary_path"
rm -f -- "$receipt"
printf 'Removed AgentDeck from %s; retained config, state, caches, logs, and services.\n' "$install_dir"
