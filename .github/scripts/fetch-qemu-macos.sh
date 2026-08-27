#!/usr/bin/env bash
# fetch-qemu-macos.sh: the macOS half of qemu-common.sh.
#
# Homebrew links its binaries against dylibs under the Cellar, which is not
# portable to a machine without that exact layout. Rather than rewriting load
# commands with install_name_tool, every non-system dylib is copied into
# launcher/qemu-libs/ unmodified and resolved at run time via DYLD_LIBRARY_PATH,
# which dyld searches before a dependency's recorded install name, even an
# absolute one.
#
# Run once per architecture, on that architecture's own machine: arm64 and
# x86_64 Homebrew are separate installs, so this cannot cross-bundle.
#
#   .github/scripts/fetch-qemu-macos.sh
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/qemu-common.sh"

if ! command -v brew >/dev/null; then
  echo "Homebrew is required (https://brew.sh)" >&2
  exit 1
fi

brew list qemu >/dev/null 2>&1 || brew install qemu

# Never bundled: they ship with every macOS install.
is_system_lib() {
  case "$1" in
    /usr/lib/*|/System/*) return 0 ;;
    *) return 1 ;;
  esac
}

collect_deps() {
  local bin="$1" install_name
  # A dylib's first `otool -L` entry is its own install name; skip
  # self-references so we don't copy the binary onto itself.
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

# Tauri's bundler never signs the files it copies in as resources, which is
# where these land. They load fine under Homebrew's own ad-hoc signature; what
# rejects them is notarization, which refuses any nested Mach-O not signed with
# the bundle's identity. Signed here so they are sealed by the bundle signature
# afterwards, the order Apple requires.
#
# Ad-hoc signatures cannot carry a secure timestamp, hence the split.
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
