#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/agentdeck-release-test.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
case "$(uname -s):$(uname -m)" in
  Darwin:arm64) target='aarch64-apple-darwin' ;;
  Darwin:x86_64) target='x86_64-apple-darwin' ;;
  Linux:x86_64) target='x86_64-unknown-linux-gnu' ;;
  *) echo 'unsupported test host' >&2; exit 1 ;;
esac

release_dir="$scratch/release assets"
install_dir="$scratch/install directory"
receipt="$scratch/receipts/installation"
mkdir -p "$release_dir/package" "$install_dir"
printf '#!/usr/bin/env bash\nprintf agentdeck-test\n' > "$release_dir/package/agentdeck"
chmod 0755 "$release_dir/package/agentdeck"
archive="$release_dir/agentdeck-${target}.tar.gz"
(cd "$root" && bash release/package-unix.sh --binary "$release_dir/package/agentdeck" --target "$target" --output-dir "$release_dir")
(cd "$release_dir" && shasum -a 256 "$(basename "$archive")" > SHA256SUMS)

install() {
  bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" \
    --dir "$install_dir" --receipt "$receipt" --version v9.9.9 "$@"
}
uninstall() { bash "$root/release/uninstall.sh" --dir "$install_dir" --receipt "$receipt" "$@"; }
reject_archive() {
  rejected_archive="$1"
  rejected_name="$(basename "$rejected_archive")"
  rejected_dir="$scratch/rejected $rejected_name"
  rejected_receipt="$scratch/rejected-receipt-$rejected_name"
  rejected_checksums="$scratch/rejected-checksums-$rejected_name"
  rejected_hash="$(shasum -a 256 "$rejected_archive" | awk '{print $1}')"
  printf '%s  agentdeck-%s.tar.gz\n' "$rejected_hash" "$target" > "$rejected_checksums"
  if bash "$root/release/install.sh" --archive "$rejected_archive" --checksums "$rejected_checksums" --dir "$rejected_dir" --receipt "$rejected_receipt"; then
    echo "unsafe archive was accepted: $rejected_name" >&2; exit 1
  fi
  test ! -e "$rejected_dir"
  test ! -e "$rejected_receipt"
}

# Fresh install from package-unix.sh's canonical release archive, then idempotent
# replacement, spaces, version receipt, and receipt-proven uninstall.
install
test -x "$install_dir/agentdeck"
test "$("$install_dir/agentdeck")" = 'agentdeck-test'
grep -Fx 'schema=2' "$receipt"
grep -Fx 'version=v9.9.9' "$receipt"
install
uninstall --dry-run >/dev/null
uninstall
test ! -e "$install_dir/agentdeck"
test ! -e "$receipt"

# A foreign same-name binary survives default collision refusal and receipt-less uninstall.
printf foreign > "$install_dir/agentdeck"
if install; then echo 'foreign collision was overwritten without --force' >&2; exit 1; fi
if uninstall; then echo 'receipt-less uninstall removed foreign files' >&2; exit 1; fi
test "$(<"$install_dir/agentdeck")" = foreign
install --force
test "$("$install_dir/agentdeck")" = 'agentdeck-test'
uninstall

# Schema 1 receipts predate the single-binary ownership contract. Neither upgrade nor
# removal trusts that proof implicitly; --force is required to adopt the binary safely.
legacy_dir="$scratch/schema 1 install"
legacy_receipt="$scratch/schema-1-receipt"
bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" \
  --dir "$legacy_dir" --receipt "$legacy_receipt" --version v9.9.9
sed 's/^schema=2$/schema=1/' "$legacy_receipt" > "$scratch/schema-1-receipt.tmp"
mv "$scratch/schema-1-receipt.tmp" "$legacy_receipt"
if bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" \
  --dir "$legacy_dir" --receipt "$legacy_receipt" --version v9.9.9; then
  echo 'schema 1 receipt was trusted for upgrade' >&2; exit 1
fi
if bash "$root/release/uninstall.sh" --dir "$legacy_dir" --receipt "$legacy_receipt"; then
  echo 'schema 1 receipt was trusted for removal' >&2; exit 1
fi
test "$("$legacy_dir/agentdeck")" = 'agentdeck-test'
bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" \
  --dir "$legacy_dir" --receipt "$legacy_receipt" --version v9.9.9 --force
bash "$root/release/uninstall.sh" --dir "$legacy_dir" --receipt "$legacy_receipt"

# A bad manifest must not create a binary or receipt.
bad_dir="$scratch/checksum failure"
bad_receipt="$scratch/bad-receipt"
mkdir -p "$bad_dir"
printf '%064d  %s\n' 0 "$(basename "$archive")" > "$release_dir/bad-SHA256SUMS"
if bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/bad-SHA256SUMS" --dir "$bad_dir" --receipt "$bad_receipt"; then
  echo 'bad checksum was accepted' >&2; exit 1
fi
test ! -e "$bad_dir/agentdeck"
test ! -e "$bad_receipt"

# The release archive has an exact regular-file/directory manifest. Links, duplicate
# members, extras, and noncanonical traversal paths must fail before any installation.
symlink_package="$scratch/symlink package"
mkdir -p "$symlink_package"
ln -s nowhere "$symlink_package/agentdeck"
symlink_archive="$release_dir/symlink-agentdeck.tar.gz"
tar -C "$symlink_package" -czf "$symlink_archive" agentdeck
reject_archive "$symlink_archive"

hardlink_package="$scratch/hardlink package"
mkdir -p "$hardlink_package"
printf hardlink > "$hardlink_package/referent"
ln "$hardlink_package/referent" "$hardlink_package/agentdeck"
hardlink_archive="$release_dir/hardlink-agentdeck.tar.gz"
tar -C "$hardlink_package" -czf "$hardlink_archive" referent agentdeck
reject_archive "$hardlink_archive"

traversal_root="$scratch/traversal package"
mkdir -p "$traversal_root/nested"
printf traversal > "$traversal_root/outside"
traversal_archive="$release_dir/traversal-agentdeck.tar.gz"
(cd "$traversal_root/nested" && tar -czf "$traversal_archive" ../outside)
reject_archive "$traversal_archive"

duplicate_archive="$release_dir/duplicate-agentdeck.tar.gz"
duplicate_package="$scratch/duplicate package"
mkdir -p "$duplicate_package"
tar -xzf "$archive" -C "$duplicate_package"
tar -C "$duplicate_package" -czf "$duplicate_archive" agentdeck README.md LICENSE install.sh uninstall.sh service.sh services agentdeck
reject_archive "$duplicate_archive"

extra_archive="$release_dir/extra-agentdeck.tar.gz"
extra_package="$scratch/extra package"
mkdir -p "$extra_package"
tar -xzf "$archive" -C "$extra_package"
printf extra > "$extra_package/unexpected"
tar -C "$extra_package" -czf "$extra_archive" agentdeck README.md LICENSE install.sh uninstall.sh service.sh services unexpected
reject_archive "$extra_archive"

# A destination symlink to a directory is always refused, even with --force. It must
# neither be followed nor replaced, and no receipt may be written for its target.
for link_name in agentdeck; do
  symlink_install_dir="$scratch/symlink destination $link_name"
  foreign_directory="$scratch/foreign directory $link_name"
  symlink_receipt="$scratch/symlink-receipt-$link_name"
  mkdir -p "$symlink_install_dir" "$foreign_directory"
  printf sentinel > "$foreign_directory/sentinel"
  ln -s "$foreign_directory" "$symlink_install_dir/$link_name"
  if bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" --dir "$symlink_install_dir" --receipt "$symlink_receipt" --force; then
    echo "directory symlink destination was followed: $link_name" >&2; exit 1
  fi
  test -L "$symlink_install_dir/$link_name"
  test "$(readlink "$symlink_install_dir/$link_name")" = "$foreign_directory"
  test "$(<"$foreign_directory/sentinel")" = sentinel
  test ! -e "$foreign_directory/agentdeck"
  test ! -e "$symlink_receipt"
done

# A forced foreign symlink to a regular file is unlinked at its own path; its target
# remains untouched before and after the receipt-proven replacement is uninstalled.
file_link_dir="$scratch/file symlink destination"
file_link_target="$scratch/foreign regular target"
file_link_receipt="$scratch/file-symlink-receipt"
mkdir -p "$file_link_dir"
printf external > "$file_link_target"
ln -s "$file_link_target" "$file_link_dir/agentdeck"
bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" --dir "$file_link_dir" --receipt "$file_link_receipt" --force
test ! -L "$file_link_dir/agentdeck"
test "$(<"$file_link_target")" = external
bash "$root/release/uninstall.sh" --dir "$file_link_dir" --receipt "$file_link_receipt"
test "$(<"$file_link_target")" = external

# The receipt itself is proof material. A receipt symlink must be rejected before
# inspecting its target, even with --force, and cannot make the installer mutate it.
receipt_symlink_dir="$scratch/receipt symlink destination"
receipt_symlink_target="$scratch/foreign receipt target"
receipt_symlink_path="$scratch/receipt link"
mkdir -p "$receipt_symlink_dir"
printf external-receipt > "$receipt_symlink_target"
ln -s "$receipt_symlink_target" "$receipt_symlink_path"
if bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" --dir "$receipt_symlink_dir" --receipt "$receipt_symlink_path" --force; then
  echo 'receipt symlink was accepted' >&2; exit 1
fi
if bash "$root/release/uninstall.sh" --dir "$receipt_symlink_dir" --receipt "$receipt_symlink_path"; then
  echo 'receipt symlink was trusted for removal' >&2; exit 1
fi
test -L "$receipt_symlink_path"
test "$(<"$receipt_symlink_target")" = external-receipt
test ! -e "$receipt_symlink_dir/agentdeck"

# --dir itself must not be a symlink into an unrelated directory, even under --force.
dir_symlink_parent="$scratch/foreign install parent"
dir_symlink_path="$scratch/install directory symlink"
dir_symlink_receipt="$scratch/dir-symlink-receipt"
mkdir -p "$dir_symlink_parent"
printf sentinel > "$dir_symlink_parent/sentinel"
ln -s "$dir_symlink_parent" "$dir_symlink_path"
if bash "$root/release/install.sh" --archive "$archive" --checksums "$release_dir/SHA256SUMS" --dir "$dir_symlink_path/" --receipt "$dir_symlink_receipt" --force; then
  echo '--dir symlink was followed' >&2; exit 1
fi
test -L "$dir_symlink_path"
test "$(<"$dir_symlink_parent/sentinel")" = sentinel
test ! -e "$dir_symlink_parent/agentdeck"
test ! -e "$dir_symlink_receipt"

# A modified owned binary is not removed merely because a receipt exists.
install
printf modified > "$install_dir/agentdeck"
if uninstall; then echo 'modified binary was removed' >&2; exit 1; fi
test "$(<"$install_dir/agentdeck")" = modified
