#!/usr/bin/env bash
# fetch-firmware.sh: populate launcher/firmware/<arch>/{kernel,initrd.gz}
# from the pinned firmware release, for the build host's own architecture
# only. This matches the single QEMU guest arch fetch-qemu-* bundles; see
# those scripts. Cross-arch guests aren't supported by a packaged installer;
# a source build passes its own --kernel/--initrd instead.
#
# Firmware images are published to dark-bio/emulator-images, a public repo
# dedicated to that purpose, so no token is needed to read its releases (gh
# still works better authenticated, for the API rate limit; the workflow
# passes the ambient GITHUB_TOKEN for that, nothing needs to be created or
# configured for it). FIRMWARE_TAG is still mandatory and has no default
# here, so the pinned version lives solely in the workflow's env, not
# duplicated into this script.
#
# Asset names embed the firmware's version and build commit
# (arkos-<version>-<commit>-emulator-kernel.<arch>), neither of which this
# script assumes. FIRMWARE_TAG alone pins the release, so both are matched
# with a wildcard. Bumping the bundled firmware is therefore just updating
# FIRMWARE_TAG in the workflow; nothing here needs to change.
#
# Each asset has a same-named .sha256 file published alongside it, checked
# here since `gh release download` doesn't verify one itself: HTTPS covers
# tampering in transit, not a corrupted/truncated download completing
# anyway, or a release's assets being silently replaced under an unchanged
# filename without FIRMWARE_TAG changing. Verifying against a checksum
# published in the same release isn't a defense against that release itself
# being compromised, since both files could be replaced together; it's a
# correctness check, not a trust boundary.
#
# Run from the emulator repo root:
#   FIRMWARE_TAG=<tag> .github/scripts/fetch-firmware.sh
set -euo pipefail

firmware_repo="dark-bio/emulator-images"
: "${FIRMWARE_TAG:?set FIRMWARE_TAG to the firmware release tag to pin, e.g. v0.11.4}"

if ! command -v gh >/dev/null; then
  echo "the GitHub CLI (gh) is required" >&2
  exit 1
fi

# macOS has no sha256sum by default (shasum -a 256 instead); Linux and Git
# for Windows' bundled coreutils both have sha256sum.
sha256_of() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# Compares against the first whitespace-delimited field of $1.sha256 rather
# than feeding the file to `sha256sum -c` directly, so this doesn't depend
# on the checksum file's recorded filename matching our own local path.
verify_checksum() {
  local file="$1" recorded actual
  recorded="$(awk '{print $1}' "${file}.sha256")"
  actual="$(sha256_of "$file")"
  if [ "$recorded" != "$actual" ]; then
    echo "checksum mismatch for $(basename "$file"): expected $recorded, got $actual" >&2
    exit 1
  fi
}

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
