#!/usr/bin/env bash
# fetch-qemu-linux.sh: populate launcher/binaries/ and launcher/qemu-libs/
# with a relocatable QEMU, for both CI and local development.
#
# Only the QEMU system emulator matching the build host's own architecture is
# bundled, as the generic "qemu-system-guest" sidecar name (which real
# qemu-system-* binary that is depends on the host). Bundling both guest
# architectures would double the installer size for no benefit to most
# users; a source build still gets both via a PATH-installed QEMU (see
# qemu.rs's `spawn_qemu`). qemu-img has no per-arch variant and is always
# bundled.
#
# QEMU comes from apt, dynamically linked against the base system's glibc
# plus a long tail of libraries (glib, pixman, gnutls, ...) that vary across
# distros. This is the classic AppImage portability problem. This copies
# every non-base-system dependency into launcher/qemu-libs/ (bundled as a
# Tauri resource) and relies on LD_LIBRARY_PATH (set at spawn time by the
# launcher, see platform.rs's `prepend_library_path`) rather than patching
# rpaths. The ELF NEEDED entries `ldd` resolves here are bare sonames (e.g.
# "libglib-2.0.so.0"), which the dynamic linker re-resolves against
# LD_LIBRARY_PATH on every run, so no binary patching is required. glibc and
# the loader itself are deliberately NOT bundled: they must match the host
# kernel/loader exactly, so bundling them would make things less portable,
# not more. Run this on an older-glibc runner (e.g. ubuntu-22.04): glibc is
# forward-compatible, so a binary built against an old one keeps working on
# newer ones, not the reverse.
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
# Everything lands in the same libs_dir as the .so dependencies and is located
# via -L (see main.rs's `spawn_qemu`); QEMU looks up only the filenames it
# needs and ignores the rest, so sharing the directory is harmless.
#
# Run from the emulator repo root:
#   .github/scripts/fetch-qemu-linux.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin_dir="$repo_root/launcher/binaries"
libs_dir="$repo_root/launcher/qemu-libs"
mkdir -p "$bin_dir" "$libs_dir"

triple="$(rustc -vV | sed -n 's/^host: //p')"
if [ -z "$triple" ]; then
  echo "could not determine host target triple via 'rustc -vV'" >&2
  exit 1
fi

# Libraries assumed present, at a compatible ABI, on any glibc Linux host.
# Bundling these would pin the app to this build machine's exact versions,
# the opposite of the goal.
is_base_system_lib() {
  case "$(basename "$1")" in
    linux-vdso.so.1|ld-linux-x86-64.so.2|ld-linux-aarch64.so.1| \
    libc.so.6|libm.so.6|libpthread.so.0|libdl.so.2|librt.so.1| \
    libresolv.so.2|libutil.so.1|libgcc_s.so.1)
      return 0 ;;
    *)
      return 1 ;;
  esac
}

# Copies $1's non-base-system shared library dependencies into libs_dir
# (skips ones already copied).
collect_deps() {
  local bin="$1"
  ldd "$bin" | awk '{print $1, $3}' | while read -r name path; do
    [ -n "${path:-}" ] && [ "$path" != "not" ] || continue
    dest="$libs_dir/$(basename "$name")"
    is_base_system_lib "$name" && continue
    [ -e "$dest" ] || cp -L "$path" "$dest"
  done
}

case "$triple" in
  aarch64-*) native_qemu=qemu-system-aarch64 ;;
  x86_64-*)  native_qemu=qemu-system-x86_64 ;;
  *)
    echo "unsupported host architecture in triple $triple" >&2
    exit 1 ;;
esac

declare -A binaries=(
  [qemu-system-guest]="$native_qemu"
  [qemu-img]=qemu-img
)

for name in "${!binaries[@]}"; do
  bin="${binaries[$name]}"
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
done

# Ask QEMU which datadirs it searches rather than guessing at distro layout.
# Debian splits these across packages and directories (seabios provides the
# BIOS and VGA ROMs, ipxe-qemu the option ROMs) while other distros ship one
# directory, and -L help is the only authoritative answer. -L on find follows
# the symlinks Debian uses for some ROMs.
qemu_datadirs="$("$native_qemu" -L help 2>/dev/null || true)"
if [ -z "$qemu_datadirs" ]; then
  echo "could not determine QEMU's firmware datadirs via '$native_qemu -L help'" >&2
  exit 1
fi
while IFS= read -r dir; do
  [ -d "$dir" ] || continue
  find -L "$dir" -maxdepth 1 \( -iname '*.bin' -o -iname '*.rom' -o -iname '*.fd' -o -iname '*.dtb' \) -size -5M \
    -exec cp -L {} "$libs_dir/" \;
done <<EOF
$qemu_datadirs
EOF

# Assert per guest arch, since a silent miss here only surfaces later as an
# opaque QEMU firmware error on a machine that has no QEMU to fall back to.
required_firmware="efi-virtio.rom"
if [ "$native_qemu" = "qemu-system-x86_64" ]; then
  required_firmware="$required_firmware bios-256k.bin vgabios-stdvga.bin"
fi
for f in $required_firmware; do
  if [ ! -f "$libs_dir/$f" ]; then
    echo "$f not found in any QEMU datadir:" >&2
    echo "$qemu_datadirs" >&2
    echo "on Debian/Ubuntu, efi-virtio.rom needs the ipxe-qemu package" >&2
    exit 1
  fi
done

# Bundled libraries can depend on each other, so keep walking until a pass
# copies nothing new.
prev_count=-1
cur_count="$(find "$libs_dir" -type f | wc -l)"
while [ "$cur_count" -ne "$prev_count" ]; do
  prev_count="$cur_count"
  for lib in "$libs_dir"/*.so*; do
    [ -f "$lib" ] && collect_deps "$lib"
  done
  cur_count="$(find "$libs_dir" -type f | wc -l)"
done

echo "populated $bin_dir (triple $triple) and $libs_dir:"
ls -la "$bin_dir" "$libs_dir"
