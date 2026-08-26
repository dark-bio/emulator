#!/usr/bin/env bash
# fetch-qemu-linux.sh: populate launcher/binaries/ and launcher/qemu-libs/ with
# a relocatable QEMU. The Linux half of qemu-common.sh, which carries the
# skeleton and the bundling rules both platforms share.
#
# QEMU comes from apt, dynamically linked against the base system's glibc plus a
# long tail of libraries (glib, pixman, gnutls, ...) that vary across distros.
# This is the classic AppImage portability problem. Every non-base-system
# dependency is copied into launcher/qemu-libs/ (bundled as a Tauri resource)
# and found at run time through LD_LIBRARY_PATH (set by the launcher, see
# platform.rs's `prepend_library_path`) rather than by patching rpaths: the ELF
# NEEDED entries `ldd` resolves here are bare sonames (e.g. "libglib-2.0.so.0"),
# which the dynamic linker re-resolves against LD_LIBRARY_PATH on every run.
#
# glibc and the loader itself are deliberately NOT bundled: they must match the
# host kernel and loader exactly, so bundling them would make things less
# portable, not more. Run this on an older-glibc runner (e.g. ubuntu-22.04):
# glibc is forward-compatible, so a binary built against an old one keeps
# working on newer ones, not the reverse.
#
# Do not test the firmware bundling by pointing -L at an empty directory on a
# machine that has QEMU installed: -L only *adds* to the search path, so QEMU
# silently falls back to its built-in datadir and the bundle looks complete when
# it is not.
#
# Run from the emulator repo root:
#   .github/scripts/fetch-qemu-linux.sh
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/qemu-common.sh"

# Libraries assumed present, at a compatible ABI, on any glibc Linux host.
# Bundling these would pin the app to this build machine's exact versions, the
# opposite of the goal.
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

# Copies $1's non-base-system shared library dependencies into libs_dir (skips
# ones already copied). `ldd` failures are swallowed rather than left to
# `pipefail`, which would otherwise abort the whole script from inside
# close_deps with nothing but ldd's own message to go on.
collect_deps() {
  local bin="$1"
  { ldd "$bin" 2>/dev/null || true; } | awk '{print $1, $3}' | while read -r name path; do
    [ -n "${path:-}" ] && [ "$path" != "not" ] || continue
    is_base_system_lib "$name" && continue
    dest="$libs_dir/$(basename "$name")"
    [ -e "$dest" ] || cp -L "$path" "$dest"
  done
}

bundle_binary qemu-system-guest "$native_qemu"
bundle_binary qemu-img qemu-img

bundle_firmware "on Debian/Ubuntu, efi-virtio.rom needs the ipxe-qemu package"

# QEMU can be built with its accelerators, block drivers and UI backends as
# dlopen'd modules rather than linked in, and Debian and Ubuntu build it that
# way. ldd cannot see a dlopen, so these never turn up through collect_deps, and
# the launcher points QEMU at them with QEMU_MODULE_DIR (see qemu.rs's
# `spawn_qemu`). They go into libs_dir ahead of the dependency walk below, so
# the modules' own dependencies get collected too.
#
# Missing them is not a corner case. Without the TCG module a machine with no
# QEMU installed has no software emulation to fall back on the moment KVM is
# unavailable, which is every user not in the kvm group, and QEMU aborts on an
# assertion inside its accelerator setup rather than failing cleanly.
module_dir=""
for candidate in /usr/lib/*/qemu /usr/lib/qemu /usr/lib64/qemu; do
  [ -d "$candidate" ] || continue
  if [ -n "$(find "$candidate" -maxdepth 1 -name '*.so' -print -quit)" ]; then
    module_dir="$candidate"
    break
  fi
done

if [ -n "$module_dir" ]; then
  find "$module_dir" -maxdepth 1 -name '*.so' -exec cp -L {} "$libs_dir/" \;
  # Asserted for the same reason as the firmware in qemu-common.sh: a silent
  # miss only surfaces later, on a machine with no QEMU to fall back to.
  accel_module="accel-tcg-${native_qemu#qemu-system-}.so"
  if [ ! -f "$libs_dir/$accel_module" ]; then
    echo "$accel_module not found in $module_dir" >&2
    exit 1
  fi
  echo "bundled $(find "$module_dir" -maxdepth 1 -name '*.so' | wc -l) QEMU modules from $module_dir"
else
  # A distro that links everything in has nothing here to collect.
  echo "this QEMU build has no loadable modules"
fi

close_deps '*.so*'

echo "populated $bin_dir (triple $triple) and $libs_dir:"
ls -la "$bin_dir" "$libs_dir"
