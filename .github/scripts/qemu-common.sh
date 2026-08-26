# qemu-common.sh: the half of fetch-qemu-linux.sh and fetch-qemu-macos.sh that
# does not differ between them. Sourced by both, never run on its own.
#
# Only the QEMU system emulator matching the build host's own architecture is
# bundled, under the generic "qemu-system-guest" sidecar name. Bundling both
# guest architectures would double the installer size for no benefit to most
# users; a source build still gets both from a PATH-installed QEMU (see qemu.rs's
# `spawn_qemu`). qemu-img has no per-arch variant and is always bundled.
#
# The caller defines one hook before invoking anything here:
#
#   collect_deps <file>   copy $1's non-system shared library dependencies into
#                         libs_dir, skipping ones already there
#
# That is the only part that differs, since ldd and otool report a binary's
# dependencies in different shapes and the two platforms draw the line between
# system and bundled library in different places. Everything else, including
# which real qemu-system-* binary the sidecar is, is settled here.
#
# Stays within bash 3.2: macOS runners resolve `bash` to Apple's pre-installed
# 3.2, so no associative arrays and no namerefs.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin_dir="$repo_root/launcher/binaries"
libs_dir="$repo_root/launcher/qemu-libs"
mkdir -p "$bin_dir" "$libs_dir"

triple="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "$triple" ]; then
  echo "could not determine host target triple via 'rustc -vV'" >&2
  exit 1
fi

case "$triple" in
  aarch64-*) native_qemu=qemu-system-aarch64 ;;
  x86_64-*)  native_qemu=qemu-system-x86_64 ;;
  *)
    echo "unsupported host architecture in triple $triple" >&2
    exit 1 ;;
esac

# Copies the binary $2 into bin_dir under the sidecar name $1, along with its
# library dependencies.
bundle_binary() {
  local name="$1" bin="$2" src
  # `command -v` prints nothing and returns non-zero on failure; caught
  # explicitly so a missing binary reports a clear cause instead of `set -e`
  # killing the script with no output.
  src="$(command -v "$bin" || true)"
  if [ -z "$src" ]; then
    echo "$bin not found on PATH (needed for the $name sidecar); is qemu installed?" >&2
    exit 1
  fi
  cp -L "$src" "$bin_dir/${name}-${triple}"
  collect_deps "$src"
}

# Copies QEMU's firmware and option ROMs into libs_dir, where the launcher
# points -L at them. $1 is an optional platform-specific hint to print if one is
# missing.
#
# Copies every small file from the datadir rather than hand-picking filenames,
# since which ROMs QEMU loads depends on the configured devices. The 5MB cap
# excludes the ~64MB ARM UEFI blobs (edk2-arm-{code,vars}.fd), which a direct
# kernel boot never uses and which would otherwise dominate the bundle. QEMU
# looks up only the filenames it needs, so sharing libs_dir with the shared
# libraries is harmless.
bundle_firmware() {
  local hint="${1:-}" datadirs dir f required
  # Ask QEMU which datadirs it searches rather than guessing at the package
  # manager's layout. Queried through the binary on PATH, not the copy already
  # in bin_dir, since the paths are reported relative to the binary's own
  # location. -L on find follows the symlinks some layouts use for ROMs.
  datadirs="$("$native_qemu" -L help 2>/dev/null || true)"
  if [ -z "$datadirs" ]; then
    echo "could not determine QEMU's firmware datadirs via '$native_qemu -L help'" >&2
    exit 1
  fi
  while IFS= read -r dir; do
    [ -d "$dir" ] || continue
    find -L "$dir" -maxdepth 1 \
      \( -iname '*.bin' -o -iname '*.rom' -o -iname '*.fd' -o -iname '*.dtb' \) \
      -size -5M -exec cp -L {} "$libs_dir/" \;
  done <<EOF
$datadirs
EOF

  # Asserted per guest arch, since a silent miss here only surfaces later as an
  # opaque QEMU firmware error on a machine that has no QEMU to fall back to.
  # efi-virtio.rom is the option ROM every virtio-pci device carries, so both
  # guests need it. x86_64 additionally runs SeaBIOS before a direct -kernel
  # boot, and -nographic still implies a default VGA device.
  required="efi-virtio.rom"
  if [ "$native_qemu" = "qemu-system-x86_64" ]; then
    required="$required bios-256k.bin vgabios-stdvga.bin"
  fi
  for f in $required; do
    if [ ! -f "$libs_dir/$f" ]; then
      echo "$f not found in any QEMU datadir:" >&2
      echo "$datadirs" >&2
      if [ -n "$hint" ]; then
        echo "$hint" >&2
      fi
      exit 1
    fi
  done
}

# Walks the dependencies of everything already in libs_dir matching the glob $1,
# repeating until a pass copies nothing new: bundled libraries depend on each
# other.
close_deps() {
  local glob="$1" lib prev cur
  prev=-1
  cur="$(find "$libs_dir" -type f | wc -l)"
  while [ "$cur" -ne "$prev" ]; do
    prev="$cur"
    for lib in "$libs_dir"/$glob; do
      [ -f "$lib" ] && collect_deps "$lib"
    done
    cur="$(find "$libs_dir" -type f | wc -l)"
  done
}
