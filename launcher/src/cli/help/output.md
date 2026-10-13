# Output

stdout carries only the result of a command. stderr carries everything read
along the way: notes, warnings, hints, steps, logs and errors. Keep the two
apart when parsing.

## Reading output

The default output is formatted for reading, the same on a terminal and in
a pipe. One item prints as a block of labels and values, several as a
table. Absent values print as -, empty lists as none and booleans as yes and
no. Expiry prints as its date in UTC. The view may leave out fields, while
--json always has them all. A mark before a state is decoration, and the
word beside it is the value. Colors need a terminal, and NO_COLOR or
CLICOLOR=0 turns them off.

## JSON

--json prints one result document on stdout and one JSON event per line on
stderr. Keys are snake_case, absent values are null and times are ISO 8601
in UTC. Help and completions print text even with --json.

Every stderr line under --json is one event, however long its message:

    {"event":"note","message":"emulator.ark is already booted"}

The kinds are note, warning, hint, step, log and error. An error event
carries the error object in place of a message, and a log event also
carries level, target and fields. A caller that merged the two streams
drops the lines starting with `{"event":` and keeps the document.

## Events and verbosity

-q hides notes, warnings and steps, and keeps errors, hints and logs. -v
adds steps. --log debug adds the launcher's diagnostics, including the log
of an emulator that start launches, and --log trace adds every request to
the registry.

## Bare runs and log files

A bare run has no result, so its stdout stays empty, apart from a run with
--kernel and --initrd, which gives stdout to the guest console. Its stderr
carries the launcher's log lines, as log events under --json.

Every emulator writes its launcher's diagnostics, what QEMU says included,
to a log file in the data directory, named after the port it holds. Each
line is one JSON record with level, target and fields, and the message is
in fields.message. Each launch on a port replaces the file.
`ark-emulator doctor --json` names the directory, and
`ark-emulator start --json` and `ark-emulator list --json` name each
emulator's file. The device itself never writes to it.

## New releases

At most once an hour, ark-emulator reads the redirect at
https://github.com/dark-bio/emulator/releases/latest to learn the newest
release. The request carries nothing about this computer, its emulators or
the running version. It runs in a detached copy that exits within 30s, so
no command waits for it. The answer is kept in update.json in the
emulator's cache directory.

While the kept answer names a newer version, start, list, stop, button and
wipe open with a note naming both versions and how to upgrade, and so does
a headless bare run. Under --json it is an ordinary note event. The note
never changes the result or the exit code, and -q hides it. doctor looks up
afresh and reports the answer as its update check. A nonempty CI turns off
both the lookup and the note.

## Failures

A failure prints error[code]: message on stderr, then hint: lines naming
the next step. Under --json the error event carries the error object, with
its code and message. A command with no result prints nothing on stdout, or
under --json a document holding only the error:

    {"error": {"code": "no-emulator", "message": "no emulator is running"}}

A command that did part of its work keeps that result on stdout and still
exits non-zero. A stop that fails or times out lists the emulators it
already stopped. A command that cannot write its output, such as into a
closed pipe, fails with io.

## Error codes

Each code is stable and belongs to one exit class.

Exit 1 is a local file, a confirmation, or a launch that failed.

- `confirmation-required` means a wipe was not confirmed. Pass --yes.
- `disk-missing` means the named image is not there. Check the path.
- `io` means a file could not be read or written, and the message names it.
  A headless bare run that cannot start reports this code too.
- `firmware-missing` means this build carries no firmware for that
  architecture. Pass --kernel and --initrd.
- `qemu-missing` means doctor found that the QEMU this build runs cannot
  run. Reinstall Ark Emulator, or install QEMU for a source build.
- `no-acceleration` means doctor found only software emulation, so a boot
  takes minutes. The hint names the fix for the platform.
- `port-exhausted` means every port from 18181 to 18280 is taken. Pass
  --port, or stop an emulator.
- `stopped-unexpectedly` means the launcher or the guest failed. A failed
  start quotes the end of the launcher's log.
- `could-not-start` means a bare run with a window could not bring the
  emulator up. The message is what the error window would show.

Exit 2 is a command line that makes no sense.

- `usage` means invalid arguments or an unknown help topic.
  `ark-emulator help` lists the commands and the topics.

Exit 3 is an emulator in the way or out of reach.

- `disk-busy` means an emulator holds that image. Stop it first.
- `no-emulator` means nothing running matches, or nothing runs.
  `ark-emulator list` shows what does.
- `ambiguous-emulator` means several emulators match, or several run and
  none was named. Name one by its locator, or pass --all to stop.
- `registry-unreachable` means the registry could not be read or refused a
  command. Emulators from different releases can cause it, so update Ark
  Emulator and restart every running emulator.
- `control-unsupported` means the emulator lacks the control the command
  needs, or its control endpoint is missing. Update Ark Emulator and
  restart every running emulator.
- `control-unreachable` means the emulator's control endpoint could not be
  reached or understood. A failed stop can still be shutting down, and a
  button input without a reply has an unknown outcome, which
  `ark-emulator button release` clears.
- `button-unavailable` means the emulated hardware is disconnected or
  restarting, or could not take the input. Wait for the boot and try again,
  since an input never carries into a restarted guest.

Exit 7 is a wait that ran out.

- `timeout` means a start or a stop was not confirmed within --timeout, so
  the emulator keeps booting or is still shutting down, or a button reply
  did not come and its outcome is unknown.

Exit 130 and 143 are Ctrl-C and SIGTERM. During a start they end only the
wait, and the emulator keeps booting.
