#!/usr/bin/env bash
# emulator: Ark hardware emulator
# Copyright 2026 Dark Bio AG. All rights reserved.
set -euo pipefail

# Installed as hdiutil on PATH to capture failures inside Tauri's bundler.
if [ "${1:-}" != "create" ]; then
  exec /usr/bin/hdiutil "$@"
fi
shift

# Keep debug logging off during creation to avoid changing contention timing.
if /usr/bin/hdiutil create "$@"; then
  exit 0
else
  status=$?
fi

# Capture contention before the runner cleans up diskimages-helper. A failure
# here must preserve the original hdiutil exit status.
echo '::group::Disk-image failure diagnostics' >&2
/usr/bin/hdiutil info >&2 || true
/sbin/mount >&2 || true
sudo -n /usr/sbin/lsof -nP +c 0 \
  | awk 'NR == 1 || /\/Volumes\/|\/dev\/disk|\/bundle\/|XProtect|diskimages|^mds|^mdworker/' >&2 \
  || true
sudo -n /usr/bin/log show --last 1m --style compact --info --debug \
  --predicate 'process == "diskarbitrationd" OR process BEGINSWITH "diskimages"' >&2 \
  || true
echo '::endgroup::' >&2
exit "$status"
