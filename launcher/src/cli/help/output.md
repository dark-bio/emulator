# Output

stdout carries only the result of a command. stderr carries everything read
along the way: progress, notes, warnings, hints, steps, logs and errors.
Keep the two apart when parsing.

## Reading output

The default output is formatted for reading, the same on a terminal and in
a pipe. A result about one thing prints as a block of labels and values,
and a list as a table, with an empty list as a sentence in place of its
table, such as "No emulators running". Absent values print as - and
booleans as yes and no. Times print in local time, as the date in a table
and in full in a block. Paths show the home directory as ~, except on
Windows, and the view may leave out fields, while --json keeps every field,
every path whole and times in UTC. A mark
such as ✓ prints as its ASCII twin, such as ok, when the locale is not
UTF-8. Colors need a terminal, and NO_COLOR or CLICOLOR=0 turns them off.

## JSON

--json prints one result document on stdout and one JSON event per line on
stderr. Keys are snake_case, absent values are null and times are ISO 8601
in UTC. Help and completions print text even with --json.

Every stderr line under --json is one event, however long its message:

    {"event":"note","message":"emulator.ark is already booted"}

The kinds are progress, note, warning, hint, step, log and error. A
progress event also carries stage, phase, step, steps, percent, done_bytes,
total_bytes and remaining_seconds, each null when unknown. An error event
carries the error object, hints included, in place of a message, and a log
event also carries level, target and fields. A caller that merged the two
streams drops the lines starting with `{"event":` and keeps the document.

## Events and verbosity

-q hides progress, notes, warnings and steps, and keeps errors, hints and
logs. -v adds steps. --log debug adds the launcher's diagnostics, including
the log of an emulator that start launches, and --log trace adds every
request to the registry.

## Runs with no command, and log files

A run with no command has no result, so its stdout stays empty, apart from
a run with --kernel and --initrd, which gives stdout to the guest console.
Its stderr narrates the boot, and carries the launcher's log lines only
with --log, as log events under --json.

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
a headless run with no command. Under --json it is an ordinary note event.
The note never changes the result or the exit code, and -q hides it.
doctor looks up afresh and reports the answer as its ark-emulator check. A
nonempty CI, or a local build whose version ends in -local, turns off both
the lookup and the note.

## Failures

A failure prints error[code]: message on stderr, then hint: lines naming
the next step. Under --json the error event carries the error object, with
its code, message and hints, and no separate hint events follow. A command
with no result prints nothing on stdout, or under --json a document holding
only the error:

    {
      "error": {
        "code": "no-emulator",
        "message": "no emulator is running",
        "hints": ["run `ark-emulator start` to boot one"]
      }
    }

A command that did part of its work keeps that result on stdout and still
exits non-zero. A stop that fails or times out lists the emulators it
already stopped. A command that cannot write its output, such as into a
closed pipe, fails with io. `ark-emulator help errors` lists every code and
when running a command again helps.
