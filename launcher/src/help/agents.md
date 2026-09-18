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

- `ark-emulator start` boots the remembered disk image, the same device the
  owner opens by double-click, and returns once the firmware accepts clients.
  It prints the locator to pass to `ark -d`. Pass --disk for a separate
  device, and --env for the cloud environment of an image created on this
  run; an existing image keeps the environment it was created with. Use
  --json for exact values.
- Expect seconds with hardware acceleration and minutes without; `info`
  reports which this computer has. --timeout bounds the wait. On timeout the
  emulator keeps booting, the exit is 7, and `ark-emulator list` shows when
  it is ready.
- start is idempotent. An image that is already booted is reported with
  started false and exit 0. Several emulators run at once, each on its own
  image and its own port from 18181 up.
- `ark-emulator stop PORT` shuts one down the way closing its window does.
  `ark-emulator stop --all` stops every one.
- `ark-emulator wipe PATH --yes` deletes a stopped image. The next start is a
  factory-fresh device that needs `ark enroll`, `ark pair` and `ark unlock`
  again. Nothing else asks a question, and start, list, stop and info never
  open a window. A bare `ark-emulator` opens the device window and may ask
  where to keep the image; do not use it from a script.

## Reading results

stdout carries the result, stderr carries events, and error[code]: lines are
stable. `ark-emulator help output` lists the codes with next steps. Exit
codes: 0 done, 1 local file or confirmation, 2 usage, 3 no such emulator or
an unreadable registry, 7 a wait ran out, 130 Ctrl-C, 143 SIGTERM. When you
cannot keep the two streams apart, run with -q --json.

## Checking state

`ark-emulator list` shows every running emulator with its port, image, ready
state and what the firmware has reported about the device.
`ark-emulator info` shows the bundled firmware, QEMU, whether hardware
acceleration is available, and where the data directory, the settings file
and the logs are. Every emulator writes its launcher log to a file under the
data directory, named by port; start --json and list --json name it, and a
failed start quotes its tail. The device itself prints nothing.

## After it boots

Use `ark`. `ark devices` lists the emulator with its locator, and
`ark -d emulator:PORT status` shows its state. A fresh emulator has a
self-signed identity; `ark enroll` gives it an attested one through a browser
login at the Ark Hub, then `ark pair` and `ark unlock` need the owner's phone.
The firmware inside the emulator is the build bundled with this app.
`ark firmware update` is unsupported on an emulator; `ark-emulator --version`
names the bundled build, and a newer emulator release carries newer firmware.
An emulated Ark's attested identity expires 30 days after `ark enroll`, and
the cloud refuses an expired one. `ark-emulator list` and `ark status` show
the day. After it, wipe the image and start a fresh device, which is the
point: nothing worth keeping should build up inside an emulator.
Worked apps to run on it, in Rust, Go, C and Python, are at
https://github.com/dark-bio/examples.
