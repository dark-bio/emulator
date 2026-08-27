#!/usr/bin/env bash
# fetch-firmware.sh: populate launcher/firmware/<arch>/{kernel,initrd.gz} from
# the pinned firmware release, for the build host's own architecture only.
#
# dark-bio/emulator-images is public, so no token is needed to read its
# releases. gh still works better authenticated, for the API rate limit.
# FIRMWARE_TAG has no default, so the pinned version lives solely in the
# workflow rather than being duplicated here. Asset names embed the version,
# so the kernel and initrd are matched with a wildcard.
#
# Each asset has a same-named .sha256 published alongside it, checked here since
# `gh release download` verifies nothing itself. This catches a truncated
# download, not a compromised release: both files could be replaced together.
#
#   FIRMWARE_TAG=<tag> .github/scripts/fetch-firmware.sh
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/checksums.sh"

firmware_repo="dark-bio/emulator-images"
: "${FIRMWARE_TAG:?set FIRMWARE_TAG to the firmware release tag to pin}"

if ! command -v gh >/dev/null; then
  echo "the GitHub CLI (gh) is required" >&2
  exit 1
fi

triple="$(rustc -vV | sed -n 's/^host: //p')"
case "$triple" in
  aarch64-*) arch=arm64 ;;
  x86_64-*)  arch=amd64 ;;
  *)
    echo "unsupported host architecture in triple $triple" >&2
    exit 1 ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out_dir="$repo_root/launcher/firmware/$arch"
mkdir -p "$out_dir"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

kernel_pattern="arkos-*-emulator-kernel.${arch}"
initrd_pattern="arkos-*-emulator-initrd.${arch}.gz"

gh release download "$FIRMWARE_TAG" --repo "$firmware_repo" \
  --pattern "$kernel_pattern" \
  --pattern "$initrd_pattern" \
  --pattern "${kernel_pattern}.sha256" \
  --pattern "${initrd_pattern}.sha256" \
  --dir "$tmp"

kernel="$(find "$tmp" -maxdepth 1 -name "$kernel_pattern" | head -1)"
initrd="$(find "$tmp" -maxdepth 1 -name "$initrd_pattern" | head -1)"
if [ -z "$kernel" ] || [ -z "$initrd" ]; then
  echo "release $FIRMWARE_TAG has no assets matching $kernel_pattern / $initrd_pattern" >&2
  exit 1
fi
if [ ! -f "${kernel}.sha256" ] || [ ! -f "${initrd}.sha256" ]; then
  echo "release $FIRMWARE_TAG is missing a .sha256 for the kernel or initrd asset" >&2
  exit 1
fi

verify_checksum "$kernel"
verify_checksum "$initrd"

cp "$kernel" "$out_dir/kernel"
cp "$initrd" "$out_dir/initrd.gz"

echo "populated $out_dir:"
find "$out_dir" -type f
