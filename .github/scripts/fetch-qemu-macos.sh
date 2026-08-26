#!/usr/bin/env bash
# fetch-qemu-macos.sh: populate launcher/binaries/ and launcher/qemu-libs/ with
# a relocatable QEMU. The macOS half of qemu-common.sh, which carries the
# skeleton and the bundling rules both platforms share.
#
# `brew install qemu` links its binaries against dylibs under the Homebrew
# Cellar (e.g. /opt/homebrew/Cellar/... or /usr/local/Cellar/...), which isn't
# portable to a machine without that exact Homebrew layout. Rather than
# rewriting each binary's load commands with install_name_tool, every non-system
# dylib dependency is copied into launcher/qemu-libs/ (bundled as a Tauri
# resource) unmodified and found at run time through DYLD_LIBRARY_PATH (set by
# the launcher, see platform.rs's `prepend_library_path`): dyld searches
# DYLD_LIBRARY_PATH/<basename> before a dependency's recorded install-name path,
# even when that path is absolute. The app bundle entitles the sidecars to read
# DYLD_* variables, which the Hardened Runtime would otherwise strip (see
# entitlements.plist).
#
# Run once per architecture, from the emulator repo root, on that
# architecture's own machine. arm64 Homebrew and x86_64 Homebrew are separate
# installs, so this cannot cross-bundle:
#   .github/scripts/fetch-qemu-macos.sh
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/qemu-common.sh"

if ! command -v brew >/dev/null; then
  echo "Homebrew is required (https://brew.sh)" >&2
  exit 1
fi

brew list qemu >/dev/null 2>&1 || brew install qemu

# System frameworks/dylibs are never bundled: they ship with every macOS
# install. Whether dyld would check DYLD_LIBRARY_PATH for them is moot, since we
# never copy them in the first place.
is_system_lib() {
  case "$1" in
    /usr/lib/*|/System/*) return 0 ;;
    *) return 1 ;;
  esac
}

# Copies $1's non-system dylib dependencies into libs_dir (skips ones already
# copied).
collect_deps() {
  local bin="$1" install_name
  # A dylib's first `otool -L` entry is its own install name; skip
  # self-references so we don't try to copy the binary onto itself.
  install_name="$(otool -D "$bin" 2>/dev/null | tail -n +2)"
  otool -L "$bin" | tail -n +2 | awk '{print $1}' | while read -r dep; do
    is_system_lib "$dep" && continue
    [ "$dep" = "$install_name" ] && continue
    dest="$libs_dir/$(basename "$dep")"
    [ -e "$dest" ] || cp -L "$dep" "$dest"
  done
}

bundle_binary qemu-system-guest "$native_qemu"
bundle_binary qemu-img qemu-img

bundle_firmware

close_deps '*.dylib'

# Sign the bundled dylibs. Tauri's bundler signs the sidecars and the .app
# itself, but never the files it copies in as resources, which is where these
# land. They arrive carrying Homebrew's own ad-hoc signature, so they do load
# as-is; what rejects them is notarization, which refuses any nested Mach-O not
# signed with the bundle's identity under the Hardened Runtime. Signing them
# here rather than post-hoc keeps them ahead of the bundle signature that seals
# them, which is the order Apple requires. Nothing else in libs_dir is Mach-O;
# the BIOS and option ROMs are plain data.
#
# APPLE_SIGNING_IDENTITY takes precedence over tauri.conf.json's own
# signingIdentity in the Tauri bundler, so setting it covers the sidecars and
# the .app as well as these. Ad-hoc signatures cannot carry a secure timestamp,
# hence the split.
identity="${APPLE_SIGNING_IDENTITY:--}"
if [ "$identity" = "-" ]; then
  timestamp="--timestamp=none"
else
  timestamp="--timestamp"
fi
for lib in "$libs_dir"/*.dylib; do
  [ -f "$lib" ] || continue
  codesign --force --options runtime "$timestamp" --sign "$identity" "$lib"
done

echo "populated $bin_dir (triple $triple) and $libs_dir:"
ls -la "$bin_dir" "$libs_dir"
