#!/usr/bin/env bash
# fetch-qemu-macos.sh: populate launcher/binaries/ and launcher/qemu-libs/
# with a relocatable QEMU, for both CI and local development.
#
# `brew install qemu` links its binaries against dylibs under the Homebrew
# Cellar (e.g. /opt/homebrew/Cellar/... or /usr/local/Cellar/...), which
# isn't portable to a machine without that exact Homebrew layout. Rather than
# rewriting each binary's load commands with install_name_tool (which also
# invalidates their code signature, requiring a re-sign), this copies every
# non-system dylib dependency into launcher/qemu-libs/ (bundled as a Tauri
# resource) unmodified and relies on DYLD_LIBRARY_PATH (set at spawn time by
# the launcher, see main.rs's `prepend_library_path`): dyld searches
# DYLD_LIBRARY_PATH/<basename> before a dependency's recorded install-name
# path, even when that path is absolute, so no binary patching is required.
#
# Only the QEMU system emulator matching the build host's own architecture is
# bundled, as the generic "qemu-system-guest" sidecar name (which real
# qemu-system-* binary that is depends on the host). Bundling both guest
# architectures would double the installer size for no benefit to most
# users; a source build still gets both via a PATH-installed QEMU (see
# main.rs's `spawn_qemu`). qemu-img has no per-arch variant and is always
# bundled.
#
# Both guests need QEMU's firmware/BIOS datadir, so this is not gated on
# architecture. x86_64 needs bios-256k.bin for SeaBIOS, which the q35 machine
# model runs before a direct -kernel boot, plus vgabios-stdvga.bin since
# -nographic still implies a default VGA device. arm64 needs efi-virtio.rom,
# the option ROM every virtio-pci device carries, including the net and block
# devices the guest is built from.
#
# Do not test this by pointing -L at an empty directory on a machine with
# QEMU installed: -L only *adds* to the search path, so QEMU silently falls
# back to its built-in datadir and the bundle looks complete when it is not.
# That masked this exact bug until a packaged .app ran on a Mac without QEMU.
#
# Copies every small file (<5MB) from the datadir rather than hand-picking
# filenames, since which ROMs QEMU loads depends on the configured devices.
# The cap excludes the ~64MB ARM UEFI blobs (edk2-arm-{code,vars}.fd), which
# a direct kernel boot never uses and which would dominate the bundle size.
# Everything lands in the same libs_dir as the dylibs and is located via -L
# (see main.rs's `spawn_qemu`); QEMU looks up only the filenames it needs and
# ignores the rest, so sharing the directory is harmless.
#
# Run once per architecture, from the emulator repo root, on that
# architecture's own machine. arm64 Homebrew and x86_64 Homebrew are separate
# installs, so this cannot cross-bundle:
#   .github/scripts/fetch-qemu-macos.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin_dir="$repo_root/launcher/binaries"
libs_dir="$repo_root/launcher/qemu-libs"
mkdir -p "$bin_dir" "$libs_dir"

if ! command -v brew >/dev/null; then
  echo "Homebrew is required (https://brew.sh)" >&2
  exit 1
fi

brew list qemu >/dev/null 2>&1 || brew install qemu

triple="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "$triple" ]; then
  echo "could not determine host target triple via 'rustc -vV'" >&2
  exit 1
fi

# System frameworks/dylibs are never bundled: they ship with every macOS
# install. Whether dyld would check DYLD_LIBRARY_PATH for them is moot,
# since we never copy them in the first place.
is_system_lib() {
  case "$1" in
    /usr/lib/*|/System/*) return 0 ;;
    *) return 1 ;;
  esac
}

# Copies $1's non-system dylib dependencies into libs_dir (skips ones
# already copied).
collect_deps() {
  local bin="$1"
  otool -L "$bin" | tail -n +2 | awk '{print $1}' | while read -r dep; do
    is_system_lib "$dep" && continue
    # A dylib's first `otool -L` entry is its own install name; skip
    # self-references so we don't try to copy the binary onto itself.
    [ "$dep" = "$(otool -D "$bin" 2>/dev/null | tail -n +2)" ] && continue
    dest="$libs_dir/$(basename "$dep")"
    [ -e "$dest" ] || cp -L "$dep" "$dest"
  done
}

case "$triple" in
  aarch64-*) native_qemu=qemu-system-aarch64 ;;
  x86_64-*)  native_qemu=qemu-system-x86_64 ;;
  *)
    echo "unsupported host architecture in triple $triple" >&2
    exit 1 ;;
esac

# Not an associative array: macOS runners resolve plain `bash` to Apple's
# pre-installed bash 3.2 (frozen pre-GPLv3), which predates `declare -A`
# (bash 4.0+). Two explicit calls instead.
fetch_binary() {
  local name="$1" bin="$2" src
  # `command -v` prints nothing and returns non-zero on failure; caught
  # explicitly here so a missing binary reports a clear cause instead of
  # `set -e` killing the script with no output.
  src="$(command -v "$bin" || true)"
  if [ -z "$src" ]; then
    echo "$bin not found on PATH (needed for the $name sidecar); is qemu installed?" >&2
    exit 1
  fi
  cp -L "$src" "$bin_dir/${name}-${triple}"
  collect_deps "$src"
}

fetch_binary "qemu-system-guest" "$native_qemu"
fetch_binary "qemu-img" "qemu-img"

# `brew --prefix` is opt/<formula>, a symlink into the Cellar that plain
# `find` doesn't reliably traverse, and `find | -exec cp` doesn't fail on zero
# matches, so a wrong path here would fail silently. `brew --cellar` gives the
# real install directory directly; -L is defensive against further symlinks
# inside it. efi-virtio.rom anchors the search because both guests need it.
cellar="$(brew --cellar qemu)"
rom="$(find -L "$cellar" -name 'efi-virtio.rom' 2>/dev/null | head -1)"
if [ -z "$rom" ]; then
  echo "could not locate efi-virtio.rom under $cellar (QEMU's firmware datadir)" >&2
  exit 1
fi
qemu_share="$(dirname "$rom")"
find -L "$qemu_share" -maxdepth 1 \( -iname '*.bin' -o -iname '*.rom' -o -iname '*.fd' -o -iname '*.dtb' \) -size -5M \
  -exec cp -L {} "$libs_dir/" \;

# Assert per guest arch, since a silent miss here only surfaces later as an
# opaque QEMU firmware error on a machine that has no QEMU to fall back to.
required_firmware="efi-virtio.rom"
if [ "$native_qemu" = "qemu-system-x86_64" ]; then
  required_firmware="$required_firmware bios-256k.bin vgabios-stdvga.bin"
fi
for f in $required_firmware; do
  if [ ! -f "$libs_dir/$f" ]; then
    echo "$f was not copied into $libs_dir from $qemu_share" >&2
    exit 1
  fi
done

# Bundled dylibs can depend on each other, so keep walking until a pass
# copies nothing new.
prev_count=-1
cur_count="$(find "$libs_dir" -type f | wc -l)"
while [ "$cur_count" -ne "$prev_count" ]; do
  prev_count="$cur_count"
  for lib in "$libs_dir"/*.dylib; do
    [ -f "$lib" ] && collect_deps "$lib"
  done
  cur_count="$(find "$libs_dir" -type f | wc -l)"
done

echo "populated $bin_dir (triple $triple) and $libs_dir:"
ls -la "$bin_dir" "$libs_dir"
