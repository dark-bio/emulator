# Driving ark-emulator from an AI agent

Ark Emulator boots the real Ark firmware in a virtual machine on this
computer, for development and demos. One process is one emulated Ark, and
the whole device is one plain disk image, so real data belongs on hardware.
ark-emulator starts, lists and stops emulators, presses their reset button
and wipes their images. Nothing it does needs the owner's approval, so no
command page has an Approval line. Everything that touches the emulated
Ark's data goes through `ark`, from https://github.com/dark-bio/cli, where
the owner approves on their phone in Ark Companion exactly as on hardware.
Read `ark help agents` before driving the Ark itself.

## Running

- Add --json to parse a result. stdout then holds one complete document and
  stderr one JSON event per line. A caller that merged the two streams
  drops the lines starting with `{"event":` and keeps the document.
- No command prompts without a terminal, or under --json or --no-input. The
  one that asks is wipe, for its confirmation, which --yes gives instead.
- `ark-emulator start` boots an emulator in a process of its own and returns
  once the firmware has booted, printing the locator that `ark --device`
  takes. The emulator keeps running until `ark-emulator stop` or until its
  window closes. --headless runs it without a window, which needs no
  display. Otherwise its process opens the device window, and a covered
  window or a locked screen does not pause the device.
- A start takes seconds with hardware acceleration and minutes without.
  --timeout, 120s unless set, bounds the wait for the firmware. On a
  timeout the emulator keeps booting, the command exits 7, and
  `ark-emulator list` shows when it is ready. Ctrl-C ends only the wait.
  Run a slow start in the background. Use your harness's background mode
  where it has one. Otherwise keep the streams and the exit code in files:

      rm -f exit.txt
      (ark-emulator start --headless --json > result.json 2> events.log
       echo $? > exit.txt) &

  Read events.log every few seconds. The command has ended once exit.txt
  exists, and result.json then holds its document.
- Starting an image that is already booted is no error. start waits for
  that emulator and reports it with started false and exit 0, in the window
  mode and the lock state it already has.
- A bare `ark-emulator` runs an emulator in the foreground with its device
  window. Unless --no-input or --headless is given, the window may ask where
  to keep the image, and a failure opens an error window. A bare run with
  --headless never asks, and Ctrl-C or SIGTERM stops its device. Use start
  instead, unless a foreground process is what you need.

## Reading results

- The exit code is the outcome. 0 is done, 1 a local file, a confirmation
  or a failed launch, 2 usage, 3 an emulator in the way or out of reach, 7
  a wait that ran out, 130 Ctrl-C and 143 SIGTERM.
- A failure prints error[code]: message, then hint: lines naming the next
  step. Codes are stable, and `ark-emulator help output` lists every one
  with its next step.
- A command that did part of its work keeps that result on stdout and still
  exits non-zero, such as a stop that stopped some emulators before one
  failed.
- A note that a newer Ark Emulator is available repeats on every start,
  list, stop, button and wipe until it is upgraded. Pass it on to the
  person, since upgrading is their call.

## Checking state

`ark-emulator list` shows every running emulator with its locator, image,
readiness, environment, name, serial and when its identity expires, under
the names `ark devices` uses. `ark-emulator doctor` checks this computer and
this build, and suggests fixes without applying them. Its JSON names where
the data directory, the settings and the logs live. Every emulator writes
its launcher log to a file named after its port, which
`ark-emulator start --json` and `ark-emulator list --json` name, and a
failed start quotes the end of it.

An emulator runs the firmware bundled with Ark Emulator, which
`ark-emulator --version` names, unless --kernel and --initrd boot another
build. `ark firmware update` is unsupported on an emulator, so newer
firmware comes with a newer Ark Emulator.

## Tasks

### Boot an emulator for ark

Run `ark-emulator start --headless --json` in the background as shown
above, and pass the locator in its result to `ark --device`. A fresh
emulator is self-signed, so it is enrolled before it pairs. `ark enroll`
prints the Ark Hub address where the person enrolls it in a browser, and
`ark pair` then needs the owner's phone. Every boot starts locked, like
hardware after power-on, so a paired emulator needs `ark unlock` after each
boot. `ark help agents` covers all three. Apps to run on an emulator, in
Rust, Go, C and Python, are at https://github.com/dark-bio/examples.

### Run several emulators

Each emulator boots its own image, so `ark-emulator start --image PATH`
with another path starts another device, on the first free port from 18181
to 18280. `ark-emulator help images` covers which image a plain start
boots.

### Press the button

An emulated Ark has the reset button of the hardware, in its window and
through ark-emulator. During `ark pair` the owner presses it once the colors
on the Ark match their phone, and the press confirms the pairing at once. A
headless emulator shows no colors, so `ark-emulator button press
--release-after 0` presses it when the person asks for that. At any other
time, a press without --release-after holds the button until
`ark-emulator button release`, and a hold of 5s arms a factory reset,
which erases the device's data and pairing once the button is let go.

### Start over

An emulated identity expires 30 days after `ark enroll`, and the cloud then
refuses it. A factory reset keeps the identity, so a fresh device needs a
wipe. `ark-emulator stop`, `ark-emulator wipe --yes` and
`ark-emulator start` give one on the remembered image. For an image started
with --image, name it to all three, by its locator to stop and by its path
to wipe and start. The fresh device needs enrolling and pairing again, and
pairing leaves it unlocked until its next boot. A copy of a stopped image
is a snapshot of the device, and booting the copy returns to it.

### Stop an emulator

`ark-emulator stop` shuts the only running emulator down, the way closing
its window does. The guest gets no warning, so stop it only while no `ark`
command runs against it. With several running, name one as `ark --device`
names it, by locator, serial, name or image, or pass --all for every one.

### Diagnose

`ark-emulator doctor` checks for a newer release, QEMU, the bundled
firmware, hardware acceleration, the data directory, the settings, the
remembered image, the registry and a free port.
