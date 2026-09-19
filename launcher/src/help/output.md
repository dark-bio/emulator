# What this tool prints, and what it exits with

There are two outputs. The reading output is the default, formatted for people
with blocks and tables, and it keeps those layouts when redirected. --json
prints one complete JSON document on stdout and one JSON Lines event per line
on stderr. Help and completions print text under either.

## The two streams

stdout carries the result of a command and nothing else. A bare run has no
result, so its stdout stays empty, except on a source build, where the guest
console has it.

stderr carries everything a person reads along the way: notes, warnings,
hints, the steps -v narrates, the diagnostics --log enables, and errors. Each
one is a line reading `kind: message`, or `{"event":"...","message":"..."}`
under --json. The kinds are note, warning, hint, step, log and error. A caller
that cannot keep the two streams apart drops the lines that start with
`{"event":`, since under --json every stderr line is one; -q drops everything
but errors and hints, and never removes them.

Color and glyphs appear only on a terminal. NO_COLOR and CLICOLOR=0 turn the
color off, and a stream that is not a terminal never had it. Nothing depends
on them: every state is also a word.

## Values

Absent values print as `-`, empty lists as `none`, and the two truth values as
`yes` and `no`. Sizes carry a binary unit in the reading output and end in
`_bytes` in JSON. Times are ISO 8601 in UTC. The reading output may show fewer
fields than the document; --json always has them all.

## Log files

Every emulator writes its launcher's own lines to a file under the data
directory, named after the port it holds, replacing what an earlier emulator
on that port left there. `doctor --json` names the directory, and
`list --json` names each emulator's file. The file holds the launcher's lines
and what QEMU said, and never anything from the device, since an emulator's
guest prints nothing at all.

## Errors

A failure prints `error[code]: message` on stderr, followed by a `hint:` line
wherever there is a next step. Under --json the error object rides in the
error event on stderr and, when no result was printed, on stdout inside an
object whose one member is `error`. The codes are stable.

- usage: the arguments do not make sense, or a help topic does not exist.
  Read `ark-emulator help` and try again. Exit 2.
- confirmation-required: wipe was given nothing to confirm with. Pass --yes.
  Exit 1.
- disk-missing: the named image is not there. Check the path, or let start
  create it. Exit 1.
- io: a file could not be read or written. The message names it. Exit 1.
- firmware-missing: this build carries no firmware for that architecture.
  Pass --kernel and --initrd. Exit 1.
- qemu-missing: the QEMU this build would run could not be run. Install
  one, or use a packaged build, which carries its own. Exit 1.
- no-acceleration: the guest would run under software emulation. The hint
  names the platform's fix. Exit 1.
- port-exhausted: every port in the range is taken. Pass --port to name one,
  or stop an emulator. Exit 1.
- stopped-unexpectedly: QEMU died while the emulator was starting. The
  message carries the tail of the launcher's log. Exit 1.
- could-not-start: a bare run could not bring the emulator up. The message is
  the report the error window would have shown. Exit 1.
- disk-busy: an emulator holds that image. Stop it first. Exit 3.
- no-emulator: nothing running matches, or nothing is running at all.
  `ark-emulator list` shows what is. Exit 3.
- ambiguous-emulator: several emulators match, or several run and none was
  named. Name one by its locator, or pass --all to stop. Exit 3.
- registry-unreachable: something answered on the registry's port and could
  not be read. Exit 3.
- timeout: the device was not ready, or did not stop, within --timeout. The
  emulator is still running; watch `ark-emulator list`. Exit 7.

## Exit codes

0 done, 1 a local file or a confirmation, 2 usage, 3 no such emulator, an
ambiguous one or an unreadable registry, 7 a wait ran out, 130 Ctrl-C, 143
SIGTERM. Ctrl-C during a start ends the wait only; the emulator carries on
booting.
