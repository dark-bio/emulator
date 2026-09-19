#!/usr/bin/env bash
# smoke-unix.sh: boot a built emulator through its own command line, prove the
# device accepts clients, then shut it down again. Covers Linux and macOS.
#
# The point is the standalone claim a portable build makes, so run it on a clean
# machine, pass no flags of your own, and let the launcher resolve its bundled
# firmware, create its own qcow2 and boot. `start` returns once the registry
# reports the device ready, which is the moment a client can connect, so a pass
# means QEMU started, the bundled libraries resolved, the kernel booted, the
# firmware came up and the emulator published itself. `stop` then proves the
# other half: the emulator can be shut down without anybody clicking anything.
#
# Exits non-zero if the start fails, if it answers anything but a ready device,
# or if the stop does not take.
#
#   .github/scripts/smoke-unix.sh <executable> [extra start args...]
#
# The deadline is generous because a CI runner cannot open /dev/kvm and has no
# nested virtualization to give WHPX, so the guest boots under TCG. A passing
# run returns the moment the device is ready, so the ceiling costs it nothing.
#
# Env: SMOKE_TIMEOUT (seconds), SMOKE_LOG.
set -euo pipefail

timeout="${SMOKE_TIMEOUT:-300}"
log="${SMOKE_LOG:-smoke.log}"
events="${log%.log}.events.log"

if [ $# -lt 1 ]; then
  echo "usage: $0 <executable> [extra start args...]" >&2
  exit 2
fi
exe="$1"
shift

if [ ! -x "$exe" ]; then
  echo "$exe is not an executable file" >&2
  exit 2
fi

port=""

dump() {
  echo "----- $log -----"
  cat "$log" || true
  echo "----- $events -----"
  cat "$events" || true
  echo "----- end of logs -----"
}

# The emulator outlives this script, so a failure anywhere past the start has to
# take it down on the way out. Only one this run booted: a start that reported
# an emulator already running leaves it to whoever started it.
cleanup() {
  if [ -n "$port" ] && [ "$started" = "true" ]; then
    "$exe" stop "emulator:$port" --no-input >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

: > "$log"
: > "$events"

# --no-input so nothing can ask where to keep an image, and --json so the answer
# is exact. The result document lands on stdout and the events on stderr.
echo "starting $exe"
status=0
"$exe" start --no-input --json --timeout "$timeout" "$@" >"$log" 2>"$events" || status=$?

# The document is ours and its shape is part of the contract, so reading it with
# sed is enough and keeps this script free of a JSON parser. It is read before
# the status is judged, since a start that timed out still names the emulator
# it left booting, and that one has to be stopped on the way out.
port="$(sed -n 's/.*"port": *\([0-9][0-9]*\).*/\1/p' "$log" | head -1)"
ready="$(sed -n 's/.*"ready": *\([a-z][a-z]*\).*/\1/p' "$log" | head -1)"
started="$(sed -n 's/.*"started": *\([a-z][a-z]*\).*/\1/p' "$log" | head -1)"

if [ "$status" -ne 0 ]; then
  echo "start exited with status $status" >&2
  dump
  exit 1
fi
if [ "$ready" != "true" ] || [ "$started" != "true" ] || [ -z "$port" ]; then
  echo "start answered started=$started ready=$ready port=$port" >&2
  dump
  exit 1
fi
echo "the device on port $port accepts clients"

echo "stopping the emulator on port $port"
status=0
"$exe" stop "emulator:$port" --no-input --json --timeout "$timeout" >>"$log" 2>>"$events" || status=$?
if [ "$status" -ne 0 ]; then
  echo "stop exited with status $status" >&2
  dump
  exit 1
fi
port=""

dump
echo "the emulator booted and shut down"
