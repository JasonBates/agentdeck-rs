#!/usr/bin/env bash
# Install a checksum-verified AgentDeck release without changing Herdr, Ollama, CodexBar,
# Tailscale, or a background service.
set -euo pipefail

repo="${AGENTDECK_REPOSITORY:-JasonBates/agentdeck-rs}"
install_dir="${AGENTDECK_INSTALL_DIR:-${HOME}/.local/bin}"
receipt="${AGENTDECK_RECEIPT_PATH:-${XDG_STATE_HOME:-${HOME}/.local/state}/agentdeck/installation/receipt}"
version='latest'
release_base=''
archive_input=''
checksums_input=''
force=false

usage() {
  cat <<'EOF'
Usage: install.sh [OPTIONS]

Install a SHA-256 checksum-verified AgentDeck release. Existing files are refused
unless they belong to the matching AgentDeck receipt; --force is required to
replace a foreign collision.

Options:
  --version VERSION       install a specific GitHub release tag (for example v0.1.0)
  --release-base URL      exact release download directory; overrides --version lookup
  --dir PATH              binary installation directory (default: ~/.local/bin)
  --receipt PATH          ownership receipt path (advanced/testing)
  --archive PATH          use a local release archive (requires --checksums)
  --checksums PATH        use a local SHA256SUMS manifest (requires --archive)
  --force                 explicitly replace a foreign binary collision
  -h, --help              show this help
EOF
}

die() { echo "error: $*" >&2; exit 1; }

while (($#)); do
  case "$1" in
    --version) version="${2:-}"; shift 2 ;;
    --release-base) release_base="${2:-}"; shift 2 ;;
    --dir) install_dir="${2:-}"; shift 2 ;;
    --receipt) receipt="${2:-}"; shift 2 ;;
    --archive) archive_input="${2:-}"; shift 2 ;;
    --checksums) checksums_input="${2:-}"; shift 2 ;;
    --force) force=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ -n "$version" && -n "$install_dir" && -n "$receipt" ]] || die 'version, directory, and receipt must be nonblank'
[[ "$install_dir" != *$'\n'* && "$install_dir" != *$'\r'* && "$receipt" != *$'\n'* && "$receipt" != *$'\r'* ]] || die 'paths may not contain newlines'
if [[ -n "$archive_input" || -n "$checksums_input" ]]; then
  [[ -n "$archive_input" && -n "$checksums_input" ]] || die '--archive and --checksums must be supplied together'
  [[ -f "$archive_input" && -f "$checksums_input" ]] || die 'local archive and checksum manifest must be regular files'
fi

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) target='aarch64-apple-darwin' ;;
  Darwin:x86_64) target='x86_64-apple-darwin' ;;
  Linux:x86_64) target='x86_64-unknown-linux-gnu' ;;
  *) die "no prebuilt AgentDeck release for $(uname -s) $(uname -m)" ;;
esac

if [[ -z "$release_base" ]]; then
  if [[ "$version" == 'latest' ]]; then
    release_base="https://github.com/${repo}/releases/latest/download"
  else
    tag="$version"
    [[ "$tag" == v* ]] || tag="v${tag}"
    release_base="https://github.com/${repo}/releases/download/${tag}"
  fi
fi

archive="agentdeck-${target}.tar.gz"
binary_path="${install_dir}/agentdeck"

hash_command=()
if command -v sha256sum >/dev/null 2>&1; then
  hash_command=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  hash_command=(shasum -a 256)
else
  die 'sha256sum or shasum is required to verify the release'
fi
hash_file() { "${hash_command[@]}" "$1" | awk '{print $1}'; }
receipt_value() { awk -F= -v key="$1" '$1 == key { value = substr($0, length(key) + 2) } END { print value }' "$receipt"; }
canonical_archive_manifest() {
  cat <<'EOF'
- agentdeck
- README.md
- LICENSE
- install.sh
- uninstall.sh
- service.sh
d services/
- services/agentdeck-task.xml.in
- services/agentdeck.service.in
- services/com.agentdeck.agentdeck.plist.in
EOF
}
refuse_symlinked_install_dir() {
  path_to_check="$1"
  while [[ "$path_to_check" != / && "$path_to_check" == */ ]]; do path_to_check="${path_to_check%/}"; done
  [[ -n "$path_to_check" ]] || path_to_check='.'
  [[ ! -L "$path_to_check" ]] || die "${path_to_check} is a symlink; refusing to follow an installation directory"
}

owned_install_is_intact() {
  [[ -f "$receipt" && ! -L "$receipt" ]] || return 1
  [[ "$(receipt_value schema)" == '2' ]] || return 1
  [[ "$(receipt_value install_dir)" == "$install_dir" ]] || return 1
  [[ "$(receipt_value target)" == "$target" ]] || return 1
  expected_hash="$(receipt_value binary_sha256)"
  [[ "$expected_hash" =~ ^[0-9a-f]{64}$ && ! -L "$binary_path" && -f "$binary_path" ]] || return 1
  [[ "$(hash_file "$binary_path")" == "$expected_hash" ]]
}

has_collision() { [[ -e "$binary_path" || -L "$binary_path" ]]; }
destination_is_symlinked_directory() { [[ -L "$1" && -d "$1" ]]; }
remove_replacement_symlink() {
  destination="$1"
  [[ -L "$destination" ]] || return 0
  [[ ! -d "$destination" ]] || die "${destination} is a symlink to a directory; refusing to follow it"
  if ! "$managed_install" && ! "$force"; then
    die "${destination} is a foreign symlink; refusing to replace it without --force"
  fi
  # rm unlinks this path itself; it never recursively follows its target.
  rm -f -- "$destination"
}

refuse_symlinked_install_dir "$install_dir"
[[ ! -L "$receipt" ]] || die "${receipt} is a symlink; refusing to trust or replace an installation receipt"
if destination_is_symlinked_directory "$binary_path"; then
  die 'agentdeck is a symlink to a directory; refusing to follow it'
fi
managed_install=false
if [[ -f "$receipt" ]]; then
  if owned_install_is_intact; then
    managed_install=true
  elif ! "$force"; then
    die "existing AgentDeck receipt or files do not match; refuse to replace them (use --force only after inspection)"
  fi
elif has_collision && ! "$force"; then
  die "${install_dir} already contains agentdeck without an AgentDeck receipt; refusing to overwrite"
fi
[[ ! -d "$binary_path" ]] || die "${binary_path} is a directory and cannot be replaced"

scratch="$(mktemp -d "${TMPDIR:-/tmp}/agentdeck-install.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
if [[ -n "$archive_input" ]]; then
  cp "$archive_input" "${scratch}/${archive}"
  cp "$checksums_input" "${scratch}/SHA256SUMS"
else
  command -v curl >/dev/null 2>&1 || die 'curl is required to download AgentDeck'
  curl --fail --location --silent --show-error "${release_base}/${archive}" -o "${scratch}/${archive}"
  curl --fail --location --silent --show-error "${release_base}/SHA256SUMS" -o "${scratch}/SHA256SUMS"
fi

expected_archive_hash="$(awk -v name="$archive" '$2 == name { hash = $1 } END { print hash }' "${scratch}/SHA256SUMS" | tr '[:upper:]' '[:lower:]')"
actual_archive_hash="$(hash_file "${scratch}/${archive}")"
[[ "$expected_archive_hash" =~ ^[0-9a-f]{64}$ && "$expected_archive_hash" == "$actual_archive_hash" ]] || die 'release checksum verification failed'
command -v tar >/dev/null 2>&1 || die 'tar is required to unpack AgentDeck'
archive_listing="${scratch}/archive-listing"
archive_metadata="${scratch}/archive-metadata"
canonical_manifest="${scratch}/canonical-manifest"
canonical_entries="${scratch}/canonical-entries"
sorted_archive_entries="${scratch}/sorted-archive-entries"
sorted_canonical_entries="${scratch}/sorted-canonical-entries"
canonical_archive_manifest > "$canonical_manifest"
awk '{ print substr($0, 3) }' "$canonical_manifest" > "$canonical_entries"
tar -tzf "${scratch}/${archive}" > "$archive_listing" || die 'release archive cannot be listed safely'
LC_ALL=C sort "$archive_listing" > "$sorted_archive_entries"
LC_ALL=C sort "$canonical_entries" > "$sorted_canonical_entries"
cmp -s "$sorted_archive_entries" "$sorted_canonical_entries" || die 'release archive must contain exactly the documented release members, without duplicates or traversal paths'
tar -tvzf "${scratch}/${archive}" > "$archive_metadata" || die 'release archive metadata cannot be inspected safely'
while IFS=' ' read -r expected_type archive_entry; do
  member_type="$(awk -v entry="$archive_entry" '$NF == entry { count += 1; type = substr($0, 1, 1) } END { if (count == 1) print type }' "$archive_metadata")"
  [[ "$member_type" == "$expected_type" ]] || die "release archive member must be an expected ${expected_type} entry: ${archive_entry}"
done < "$canonical_manifest"
tar -xzf "${scratch}/${archive}" -C "$scratch"
candidate="${scratch}/agentdeck"
[[ -f "$candidate" && ! -L "$candidate" ]] || die 'release archive lacks a regular agentdeck file'
candidate_hash="$(hash_file "$candidate")"

mkdir -p "$install_dir"
remove_replacement_symlink "$binary_path"
temporary_binary="${install_dir}/.agentdeck.new.$$"
install -m 0755 "$candidate" "$temporary_binary"
mv -f "$temporary_binary" "$binary_path"

receipt_dir="$(dirname "$receipt")"
mkdir -p "$receipt_dir"
chmod 700 "$receipt_dir"
temporary_receipt="${receipt_dir}/.receipt.new.$$"
(
  umask 077
  printf 'schema=2\ninstall_dir=%s\nversion=%s\ntarget=%s\nrelease_base=%s\narchive_sha256=%s\nbinary_sha256=%s\n' \
    "$install_dir" "$version" "$target" "$release_base" "$actual_archive_hash" "$candidate_hash" > "$temporary_receipt"
)
chmod 600 "$temporary_receipt"
mv -f "$temporary_receipt" "$receipt"

printf 'Installed AgentDeck %s (%s) to %s\n' "$version" "$target" "$install_dir"
printf 'Receipt: %s\n' "$receipt"
printf 'Run: %s version\n' "$binary_path"
