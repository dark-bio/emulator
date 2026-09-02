#!/usr/bin/env bash
# fetch-firmware.sh: populate launcher/firmware/<arch>/{kernel,initrd.gz} from
# the pinned firmware release, for the build host's own architecture only.
#
# dark-bio/emulator-images is public, so no token is needed to read its
# releases. gh still works better authenticated, for the API rate limit.
# Neither FIRMWARE_TAG nor the digests have a default, so the pin lives solely
# in the workflow rather than being duplicated here. Asset names embed the
# version, so the kernel and initrd are matched with a wildcard.
#
# Each asset is checked against its pinned digest rather than against the
# .sha256 the release publishes beside it, which a replaced release would carry
# too. `gh release download` verifies nothing itself.
#
#   FIRMWARE_TAG=<tag> \
#   FIRMWARE_<ARCH>_KERNEL_SHA256=<hex> FIRMWARE_<ARCH>_INITRD_SHA256=<hex> \
#     .github/scripts/fetch-firmware.sh
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

# Only the host arch's two digests are needed, so the others may be unset. Bash
# 3.2 has no associative arrays, hence the indirection through a name.
arch_upper="$(echo "$arch" | tr '[:lower:]' '[:upper:]')"
kernel_sha_var="FIRMWARE_${arch_upper}_KERNEL_SHA256"
initrd_sha_var="FIRMWARE_${arch_upper}_INITRD_SHA256"
kernel_sha256="${!kernel_sha_var:?set $kernel_sha_var to the pinned kernel digest}"
initrd_sha256="${!initrd_sha_var:?set $initrd_sha_var to the pinned initrd digest}"

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
  --dir "$tmp"

kernel="$(find "$tmp" -maxdepth 1 -name "$kernel_pattern" | head -1)"
initrd="$(find "$tmp" -maxdepth 1 -name "$initrd_pattern" | head -1)"
if [ -z "$kernel" ] || [ -z "$initrd" ]; then
  echo "release $FIRMWARE_TAG has no assets matching $kernel_pattern / $initrd_pattern" >&2
  exit 1
fi

verify_checksum "$kernel" "$kernel_sha256"
verify_checksum "$initrd" "$initrd_sha256"

cp "$kernel" "$out_dir/kernel"
cp "$initrd" "$out_dir/initrd.gz"

echo "populated $out_dir:"
find "$out_dir" -type f
