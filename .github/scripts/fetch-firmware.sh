#!/usr/bin/env bash
# fetch-firmware.sh: populate launcher/firmware/<arch>/{kernel,initrd.gz}
# from the pinned firmware release, for the build host's own architecture
# only. This matches the single QEMU guest arch fetch-qemu-* bundles; see
# those scripts. Cross-arch guests aren't supported by a packaged installer;
# a source build passes its own --kernel/--initrd instead.
#
# The firmware repo is private (unlike this one), so both its path and a
# token with read access to it come from Actions secrets rather than being
# named here: see .github/workflows/release.yml. FIRMWARE_TAG is not secret
# (a bare version number doesn't reveal which repo it's from), but is still
# mandatory and has no default here. The pinned version belongs solely in
# the workflow's env, not duplicated into this script.
#
# Asset names embed the firmware's version and build commit
# (arkos-<version>-<commit>-emulator-kernel.<arch>), neither of which this
# script assumes. FIRMWARE_TAG alone pins the release, so both are matched
# with a wildcard. Bumping the bundled firmware is therefore just updating
# FIRMWARE_TAG in the workflow; nothing here needs to change.
#
# Run from the emulator repo root:
#   FIRMWARE_REPO=<owner>/<repo> FIRMWARE_TAG=<tag> GH_TOKEN=<token> \
#     .github/scripts/fetch-firmware.sh
set -euo pipefail

: "${FIRMWARE_REPO:?set FIRMWARE_REPO to the firmware release repo, e.g. owner/repo}"
: "${FIRMWARE_TAG:?set FIRMWARE_TAG to the firmware release tag to pin, e.g. v0.11.4}"

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

gh release download "$FIRMWARE_TAG" --repo "$FIRMWARE_REPO" \
  --pattern "$kernel_pattern" \
  --pattern "$initrd_pattern" \
  --dir "$tmp"

kernel="$(find "$tmp" -maxdepth 1 -name "$kernel_pattern" | head -1)"
initrd="$(find "$tmp" -maxdepth 1 -name "$initrd_pattern" | head -1)"
if [ -z "$kernel" ] || [ -z "$initrd" ]; then
  echo "release $FIRMWARE_TAG has no assets matching $kernel_pattern / $initrd_pattern" >&2
  exit 1
fi

cp "$kernel" "$out_dir/kernel"
cp "$initrd" "$out_dir/initrd.gz"

echo "populated $out_dir:"
find "$out_dir" -type f
