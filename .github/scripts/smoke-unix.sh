#!/usr/bin/env bash
# smoke-unix.sh: launch a built emulator and wait for the emulated device to
# finish booting, for both CI and local development. Covers Linux and macOS,
# which differ only in how the workflow locates the executable and in needing
# an X server; unlike the fetch-qemu-* scripts, the logic itself is shared.
#
# The point of this check is the standalone claim a portable build makes: it
# is meant to run on a machine with no QEMU, no webkit and no firmware on
# disk. So run it on a clean machine, pass no flags, and let the launcher
# resolve its own bundled firmware, create its own qcow2, and boot.
#
# QEMU is spawned with -serial stdio and inherits the launcher's handles, so
# one redirection captures both the launcher's own "[launcher] ..."
# diagnostics and the guest's console. Waiting for a specific line on that
# console is a much stronger signal than waiting out a timer: it means QEMU
# started, the bundled libraries resolved, the kernel booted, and the
# firmware's services came up.
#
# OpenRC writes its status column by moving the cursor rather than by
# printing the status next to the message, so on a real console "[ ok ]"
# arrives after a cursor-up escape and ends up on a line of its own once
# those escapes are stripped. Each status is therefore reattached to the
# message above it before matching, which keeps the match within one line.
# Matching across lines instead would let a later service's "[ ok ]" satisfy
# an earlier service that never reported one. Stripping is done with perl,
# not sed, because BSD sed does not understand \x1B escapes.
#
# The stripped text is materialized to a file and grepped there rather than
# piped into grep. Under `pipefail`, `grep -q` exiting on its first match
# closes the pipe early, perl dies of SIGPIPE, and the pipeline reports
# failure even though the pattern was found. That only bites once the log is
# big enough for perl to still be writing, which is to say only on a real
# boot.
#
# Exits non-zero if the process dies before the marker appears, if the marker
# never appears within the timeout, or if the service reports failure.
#
#   .github/scripts/smoke-unix.sh <executable> [launcher args...]
#
# Env: SMOKE_TIMEOUT (seconds, default 60), SMOKE_MARKER, SMOKE_LOG.
set -euo pipefail

timeout="${SMOKE_TIMEOUT:-60}"
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
    # The launcher ties QEMU's lifetime to its own, so killing the launcher
    # is enough to tear the guest down too.
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

# Regenerates the matchable form of the log: ANSI colour and cursor
# escapes and carriage returns removed, then any status bracket left
# stranded at the start of a line reattached to the message above it. The
# reattachment also handles a plain non-TTY layout, where the status is
# printed on its own line with no escapes at all.
refresh() {
  perl -0777 -pe '
    s/\e\[[0-9;?]*[a-zA-Z]//g;
    s/\e[()][A-B0-9]//g;
    s/\r//g;
    s/\n[ \t]*(\[ *(?:ok|!!|oops) *\])/ $1/g;
  ' "$log" > "$stripped"
}

# Whether the marker and the given status share a line, which after refresh
# means the status belongs to that marker and not to some other service.
# SMOKE_MARKER is used as an extended regular expression, not a literal.
matched() {
  grep -qE "$marker.*\[ *$1 *\]" "$stripped"
}

# Seen at all, regardless of outcome. Used to tell "never got there" apart
# from "got there, but the status did not match", which are very different
# failures to debug.
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
