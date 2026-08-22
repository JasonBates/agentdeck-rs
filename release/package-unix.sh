#!/usr/bin/env bash
set -euo pipefail

binary=''
target=''
output_dir=''
usage() { echo 'Usage: package-unix.sh --binary PATH --target TARGET --output-dir PATH' >&2; }
while (($#)); do
  case "$1" in
    --binary) binary="${2:-}"; shift 2 ;;
    --target) target="${2:-}"; shift 2 ;;
    --output-dir) output_dir="${2:-}"; shift 2 ;;
    *) usage; exit 64 ;;
  esac
done
[[ -x "$binary" && -n "$target" && -n "$output_dir" ]] || { usage; exit 1; }
mkdir -p "$output_dir"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/agentdeck-package.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
install -m 0755 "$binary" "$scratch/agentdeck"
cp README.md LICENSE "$scratch/"
cp release/install.sh release/uninstall.sh release/service.sh "$scratch/"
cp -R release/services "$scratch/services"
tar -C "$scratch" -czf "$output_dir/agentdeck-${target}.tar.gz" agentdeck README.md LICENSE install.sh uninstall.sh service.sh services
