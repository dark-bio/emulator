# What this tool prints, and what it exits with

There are two outputs. The reading output is the default, formatted for people
with blocks and tables, and it keeps those layouts when redirected. --json
prints one complete JSON document on stdout and one JSON Lines event per line
on stderr. Help prints text under either.

## The two streams

stdout carries the result of a command and nothing else. A bare run has no
result, so its stdout stays empty, except on a source build, where the guest
console has it.

stderr carries everything a person reads along the way: notes, warnings,
hints, the steps -v narrates, the diagnostics --log enables, and errors. Each
one is a line reading `kind: message`, or `{"event":"...","message":"..."}`
under --json. The kinds are note, warning, hint, step, log and error. When a
caller cannot keep the two streams apart, -q --json is the recipe: -q drops
everything but errors and hints.

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
on that port left there. `info` names the directory, and `list --json` names
each emulator's file. The file holds the launcher's lines and what QEMU said,
and never anything from the device: an emulator's guest prints nothing at all.

## Errors

A failure prints `error[code]: message` on stderr, followed by a `hint:` line
wherever there is a next step, and the same as an error object under --json.
The codes are stable.

- usage: the arguments do not make sense, or a help topic does not exist.
  Read `ark-emulator help` and try again. Exit 2.
- confirmation-required: wipe was given nothing to confirm with. Pass --yes.
  Exit 1.
- disk-missing: the named image is not there. Check the path, or let start
  create it. Exit 1.
- io: a file could not be read or written. The message names it. Exit 1.
- firmware-missing: this build carries no firmware for that architecture.
  Pass --kernel and --initrd. Exit 1.
- qemu-missing: there is no QEMU to run. Install one, or use a packaged
  build, which carries its own. Exit 1.
- port-exhausted: every port in the range is taken. Pass --host-addr to name
  one, or stop an emulator. Exit 1.
- stopped-unexpectedly: QEMU died while the emulator was starting. The
  message carries the tail of the launcher's log. Exit 1.
- disk-busy: an emulator is booted from that image. Stop it first. Exit 3.
- no-emulator: nothing is running on that port. `ark-emulator list` shows
  what is. Exit 3.
- registry-unreachable: something answered on the registry's port and could
  not be read. Exit 3.
- timeout: the device was not ready, or did not stop, within --timeout. The
  emulator is still running; watch `ark-emulator list`. Exit 7.

## Exit codes

0 done, 1 a local file or a confirmation, 2 usage, 3 no such emulator or an
unreadable registry, 7 a wait ran out, 130 Ctrl-C, 143 SIGTERM. Ctrl-C during
a start ends the wait only; the emulator carries on booting.
