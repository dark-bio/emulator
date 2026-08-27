#!/usr/bin/env bash
# fetch-qemu-linux.sh: the Linux half of qemu-common.sh.
#
# QEMU comes from apt, dynamically linked against glibc plus a long tail of
# libraries that vary across distros. Every non-base-system dependency is copied
# into launcher/qemu-libs/ and resolved at run time via LD_LIBRARY_PATH rather
# than by patching rpaths: ldd resolves bare sonames, which the dynamic linker
# re-resolves on every run.
#
# glibc and the loader are deliberately not bundled; they must match the host.
# Build on the oldest glibc available, since it is forward-compatible: a binary
# built against an old one keeps working on newer ones, not the reverse.
#
# Do not test the firmware bundling by pointing -L at an empty directory on a
# machine that has QEMU installed: -L only adds to the search path, so QEMU
# falls back to its built-in datadir and the bundle looks complete when it is not.
#
#   .github/scripts/fetch-qemu-linux.sh
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/qemu-common.sh"

# Assumed present at a compatible ABI on any glibc host. Bundling these would
# pin the app to this build machine's exact versions.
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

# `ldd` failures are swallowed rather than left to `pipefail`, which would abort
# the script from inside close_deps with only ldd's own message to go on.
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

# QEMU's accelerators, block drivers and UI backends can be dlopen'd modules
# rather than linked in, and Debian and Ubuntu build them that way. ldd cannot
# see a dlopen, so they never turn up through collect_deps. Copied ahead of the
# dependency walk below so their own dependencies get collected too.
#
# Without the TCG module, a machine with no QEMU installed has no software
# emulation to fall back on when KVM is unavailable, and QEMU aborts on an
# assertion rather than failing cleanly.
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
  # Asserted for the same reason as the firmware: a silent miss only surfaces
  # later, on a machine with no QEMU to fall back to.
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
