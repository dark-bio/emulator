# Managing the Ark emulator from a script or an AI agent

The emulator boots the real Ark firmware in a virtual machine on this computer.
One process is one emulated Ark, and its whole state is one disk image. It
exists for development and demos, so keep real data on hardware. Everything
that touches the emulated Ark's data goes through `ark`, from the repository
at https://github.com/dark-bio/cli, where the owner approves on their phone
in Ark Companion exactly as on hardware. This tool never talks to the Ark, and
nothing here needs approval, which is why no command has an Approval line.
Read `ark help agents` before driving the Ark itself.

## Running

- `ark-emulator start` boots the remembered image, the same device the
  owner opens by double-click, and returns once the firmware accepts clients.
  It prints the locator to pass to `ark -d`. The emulator runs in a process
  of its own. Pass --headless to run without a window or display server;
  otherwise it shows the device face. A covered window or locked screen
  does not interrupt the device. Pass --image for a separate device, and --env for the cloud
  environment of an image created on this run; an existing image keeps the
  environment it was created with. Use --json for exact values.
- Expect seconds with hardware acceleration and minutes without;
  `ark-emulator doctor` says which this computer has and what to do about
  it. --timeout bounds the wait. On timeout the emulator keeps booting, the
  exit is 7, and `ark-emulator list` shows when it is ready.
- start is idempotent. An image that is already booted is waited for and
  reported with started false and exit 0, keeping its current window mode.
  A second device is asked for by
  naming a second image; each runs on its own port from 18181 up.
- `ark-emulator stop` shuts the only running emulator down the way closing
  its window does. With several running, name one as `ark -d` would, by its
  locator, serial, name or image, or pass --all for every one.
- `ark-emulator wipe --yes` resets the image start would boot to a fresh
  device, and `wipe PATH --yes` another one. The file stays, so the next
  start boots a factory-fresh device that needs `ark enroll`, `ark pair` and
  `ark unlock` again. The reset loop is stop, wipe, start.
- No management command opens a window of its own. A bare `ark-emulator`
  opens the device window and may ask where to keep the image.
  `ark-emulator --headless` runs in the foreground without questions. It
  uses the same image defaults as start, ignores the saved autostart toggle,
  and exits on failure. Ctrl-C or SIGTERM stops that device and exits 130
  or 143. Interrupting a start command only ends its readiness wait.

## Reading results

stdout carries the result, stderr carries events, and error[code]: lines are
stable. `ark-emulator help output` lists the codes with next steps. Exit
codes: 0 done, 1 local file or confirmation, 2 usage, 3 no such emulator or
an unreadable registry, 7 a wait ran out, 130 Ctrl-C, 143 SIGTERM. When you
cannot keep the two streams apart, drop the lines that start with
`{"event":` under --json; what remains is the document.

A note that a newer Ark Emulator is available names the upgrade and repeats
on every start, list, stop and wipe until it happens, so pass it on to the
person; upgrading is their call. doctor reports the same as its update check,
and a nonempty CI turns the note off.

A start without hardware acceleration takes minutes, so run it in the
background and follow stderr, without starting another:

    ark-emulator start --headless --json > result.json 2> events.log &
    tail -n 2 events.log
    wait $!

## Checking state

`ark-emulator list` shows every running emulator with its locator, image,
ready state and what the firmware has reported about the device, under the
names `ark devices` uses for the same facts.
`ark-emulator doctor` checks this computer and this build, whether a newer
release is out, QEMU, the bundled firmware, hardware acceleration, the data
directory, the settings, the remembered image, the registry and a free port,
and says what to fix; its
JSON also carries where everything lives. Every emulator writes its launcher
log to a file under the data directory, named by port; start --json and
list --json name it, and a failed start quotes its tail. The device itself
prints nothing in a packaged build. A foreground source build gives stdout
to the guest console, including under --json; its stderr still carries
JSON events. Use start --json when a result document is needed.

## After it boots

Use `ark`. `ark devices` lists the emulator with its locator, and
`ark -d emulator:PORT status` shows its state. A fresh emulator has a
self-signed identity; `ark enroll` gives it an attested one through a browser
login at Ark Hub, then `ark pair` and `ark unlock` need the owner's phone.
The firmware inside the emulator is the build bundled with this app.
`ark firmware update` is unsupported on an emulator; `ark-emulator --version`
names the bundled build, and a newer emulator release carries newer firmware.
An emulated Ark's attested identity expires 30 days after `ark enroll`, and
the cloud refuses an expired one. `ark-emulator list` shows the day, and
`ark genuine` says when it has passed. After it, stop, wipe and start again;
nothing worth keeping should build up inside an emulator. Copying a stopped
image copies the device, which is the snapshot; a copy boots locked and needs
`ark unlock` again.
Worked apps to run on it, in Rust, Go, C and Python, are at
https://github.com/dark-bio/examples.
