#!/usr/bin/env bash
# smoke-unix.sh: launch a built emulator and wait for the emulated device to
# finish booting. Covers Linux and macOS.
#
# The point is the standalone claim a portable build makes, so run it on a clean
# machine, pass no flags, and let the launcher resolve its own bundled firmware,
# create its own qcow2, and boot. Waiting for a specific line on the guest
# console is a much stronger signal than waiting out a timer: it means QEMU
# started, the bundled libraries resolved, the kernel booted, and the firmware's
# services came up.
#
# OpenRC writes its status column by moving the cursor, so on a real console
# "[ ok ]" arrives after a cursor-up escape and lands on a line of its own once
# the escapes are stripped. Each status is reattached to the message above it
# before matching, keeping the match within one line; matching across lines
# would let a later service's "[ ok ]" satisfy an earlier one that never
# reported. Stripped with perl, not sed, because BSD sed cannot read \x1B.
#
# The stripped text is written to a file and grepped there rather than piped
# into grep. Under `pipefail`, `grep -q` exiting on its first match closes the
# pipe early, perl dies of SIGPIPE, and the pipeline reports failure even though
# the pattern was found. That only bites once the log is big enough for perl to
# still be writing, which is to say only on a real boot.
#
# Exits non-zero if the process dies before the marker appears, if the marker
# never appears within the timeout, or if the service reports failure.
#
#   .github/scripts/smoke-unix.sh <executable> [launcher args...]
#
# The deadline is generous because a CI runner cannot open /dev/kvm and has no
# nested virtualization to give WHPX, so the guest boots under TCG. A passing
# run exits the moment the marker appears, so the ceiling costs it nothing.
#
# Env: SMOKE_TIMEOUT (seconds), SMOKE_MARKER, SMOKE_LOG.
set -euo pipefail

# A fatal error opens a window the launcher waits on, and nothing here can
# dismiss it, so ask for the report on stderr and an immediate exit instead.
export ARK_EMULATOR_NO_DIALOG=1

timeout="${SMOKE_TIMEOUT:-120}"
marker="${SMOKE_MARKER:-Starting runcore}"
log="${SMOKE_LOG:-smoke.log}"

if [ $# -lt 1 ]; then
  echo "usage: $0 <executable> [launcher args...]" >&2
  exit 2
fi
exe="$1"
shift

if [ ! -x "$exe" ]; then
  echo "$exe is not an executable file" >&2
  exit 2
fi

stripped="$(mktemp)"
pid=""

cleanup() {
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    # The launcher ties QEMU's lifetime to its own, so this tears down the guest.
    kill "$pid" 2>/dev/null || true
    for _ in 1 2 3 4 5; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$stripped"
}
trap cleanup EXIT

# Regenerates the matchable form of the log. The reattachment also handles the
# non-TTY layout, where the status is printed on its own line with no escapes.
refresh() {
  perl -0777 -pe '
    s/\e\[[0-9;?]*[a-zA-Z]//g;
    s/\e[()][A-B0-9]//g;
    s/\r//g;
    s/\n[ \t]*(\[ *(?:ok|!!|oops) *\])/ $1/g;
  ' "$log" > "$stripped"
}

# After refresh, sharing a line means the status belongs to that marker and not
# to some other service. SMOKE_MARKER is an extended regex, not a literal.
matched() {
  grep -qE "$marker.*\[ *$1 *\]" "$stripped"
}

# Tells "never got there" apart from "got there, but the status did not match",
# which are very different failures to debug.
seen_marker() {
  grep -qE "$marker" "$stripped"
}

dump_log() {
  refresh
  echo "----- $log -----"
  cat "$stripped"
  echo "----- end of $log -----"
}

: > "$log"
echo "launching $exe $*"
"$exe" "$@" >"$log" 2>&1 &
pid=$!

deadline=$(( $(date +%s) + timeout ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  if ! kill -0 "$pid" 2>/dev/null; then
    status=0
    wait "$pid" || status=$?
    echo "the emulator exited with status $status before reaching the marker" >&2
    dump_log
    exit 1
  fi

  refresh
  if matched "ok"; then
    echo "matched \"$marker ... [ ok ]\", the device booted"
    dump_log
    exit 0
  fi
  if matched '!!'; then
    echo "\"$marker\" reported failure" >&2
    dump_log
    exit 1
  fi

  sleep 1
done

refresh
if seen_marker; then
  echo "saw \"$marker\" but no bracketed status within ${timeout}s;" >&2
  echo "the marker regex may need adjusting for how the status was laid out" >&2
else
  echo "no \"$marker\" within ${timeout}s" >&2
fi
dump_log
exit 1
