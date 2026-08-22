#!/usr/bin/env bash
set -euo pipefail

archive="${1:?archive path required}"
target="${2:?target triple required}"
case "$archive" in
  *.tar.gz) ;;
  *) echo "expected .tar.gz archive: $archive" >&2; exit 1 ;;
esac

case "$target" in
  x86_64-apple-darwin|aarch64-apple-darwin|x86_64-unknown-linux-gnu) ;;
  *) echo "unsupported Unix release target: $target" >&2; exit 1 ;;
esac

scratch="$(mktemp -d "${TMPDIR:-/tmp}/agentdeck-archive-layout.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
actual_entries="$scratch/actual-entries"
sorted_actual_entries="$scratch/sorted-actual-entries"
expected_entries="$scratch/expected-entries"
sorted_expected_entries="$scratch/sorted-expected-entries"
metadata="$scratch/metadata"

tar -tzf "$archive" > "$actual_entries" || { echo "cannot list archive: $archive" >&2; exit 1; }
cat > "$expected_entries" <<'EOF'
agentdeck
README.md
LICENSE
install.sh
uninstall.sh
service.sh
services/
services/agentdeck-task.xml.in
services/agentdeck.service.in
services/com.agentdeck.agentdeck.plist.in
EOF
LC_ALL=C sort "$actual_entries" > "$sorted_actual_entries"
LC_ALL=C sort "$expected_entries" > "$sorted_expected_entries"
cmp -s "$sorted_actual_entries" "$sorted_expected_entries" || {
  echo 'archive layout must contain exactly the documented release members, without duplicates or traversal paths' >&2
  exit 1
}

tar -tvzf "$archive" > "$metadata" || { echo "cannot inspect archive metadata: $archive" >&2; exit 1; }
while IFS= read -r required; do
  expected_type='-'
  [[ "$required" == 'services/' ]] && expected_type='d'
  member_type="$(awk -v entry="$required" '$NF == entry { count += 1; type = substr($0, 1, 1) } END { if (count == 1) print type }' "$metadata")"
  [[ "$member_type" == "$expected_type" ]] || {
    echo "archive member must be an expected ${expected_type} entry: ${required}" >&2
    exit 1
  }
done < "$expected_entries"

# Validate the member after the exact manifest and type checks above. This makes
# the release job prove that an archive labelled for a target actually contains
# that target's executable, rather than merely the build host's binary.
command -v file >/dev/null 2>&1 || {
  echo 'the file utility is required to verify the release binary architecture' >&2
  exit 1
}
tar -xzf "$archive" -C "$scratch" agentdeck || {
  echo "cannot extract agentdeck from archive: $archive" >&2
  exit 1
}
binary_info="$(file -b "$scratch/agentdeck")"
case "$target" in
  x86_64-apple-darwin)
    [[ "$binary_info" == "Mach-O 64-bit executable x86_64"* ]] || {
      echo "expected x86_64 Mach-O agentdeck for $target; got: $binary_info" >&2
      exit 1
    }
    ;;
  aarch64-apple-darwin)
    [[ "$binary_info" == "Mach-O 64-bit executable arm64"* ]] || {
      echo "expected arm64 Mach-O agentdeck for $target; got: $binary_info" >&2
      exit 1
    }
    ;;
  x86_64-unknown-linux-gnu)
    [[ "$binary_info" == ELF* && "$binary_info" == *x86-64* ]] || {
      echo "expected x86-64 ELF agentdeck for $target; got: $binary_info" >&2
      exit 1
    }
    ;;
esac
