# qemu-common.sh: the half of fetch-qemu-linux.sh and fetch-qemu-macos.sh that
# does not differ between them. Sourced by both, never run on its own.
#
# Only the host's own architecture is bundled, as the generic
# "qemu-system-guest" sidecar. Bundling both would double the installer size.
#
# The caller defines collect_deps <file>, copying $1's non-system library
# dependencies into libs_dir. That is the only platform-specific part.
#
# Must stay bash 3.2: macOS ships Apple's 3.2, so no associative arrays and no
# namerefs.

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
  # `command -v` fails silently, so report the cause here rather than letting
  # `set -e` kill the script with no output.
  src="$(command -v "$bin" || true)"
  if [ -z "$src" ]; then
    echo "$bin not found on PATH (needed for the $name sidecar); is qemu installed?" >&2
    exit 1
  fi
  cp -L "$src" "$bin_dir/${name}-${triple}"
  collect_deps "$src"
}

# Copies QEMU's firmware and option ROMs into libs_dir. $1 is an optional hint
# printed if one is missing.
#
# Takes every small file from the datadir rather than naming them, since which
# ROMs QEMU loads depends on the configured devices. The size cap excludes the
# ARM UEFI blobs, which a direct kernel boot never uses.
bundle_firmware() {
  local hint="${1:-}" datadirs dir f required
  # Ask QEMU rather than guessing at the package manager's layout, and ask the
  # binary on PATH, since the paths it reports are relative to its own location.
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

  # A silent miss only surfaces later as an opaque QEMU firmware error, on a
  # machine with no QEMU to fall back to. efi-virtio.rom is carried by every
  # virtio-pci device; x86_64 also runs SeaBIOS and gets a default VGA device.
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

# Walks the dependencies of everything in libs_dir matching the glob $1, until a
# pass copies nothing new: bundled libraries depend on each other.
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
