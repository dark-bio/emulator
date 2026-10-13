# Ark Emulator

Emulated Ark enclave for development and demos. It boots the real ArkOS
firmware in QEMU, with an optional window showing the device's face, its
four RGB LEDs and its reset button. The
[`ark`](https://github.com/dark-bio/cli) command line tool and
[Ark Hub](https://hub.dark.bio) talk to it exactly as to a hardware Ark. Its
owner approves access on their phone in Ark Companion, on
[iOS](https://apps.apple.com/app/id6751324700) or
[Android](https://play.google.com/store/apps/details?id=bio.dark.companion).

> [!CAUTION]
> **The emulator is not a vault.** It runs the Ark's firmware without the
> hardware that protects the Ark's keys. Those keys sit in a plain qcow2 disk
> image on your computer, so anyone with the file holds the device's
> identity. The owner's data stays encrypted while the device is locked, and
> lies open in the memory of a process on your computer while it is
> unlocked. Keep real genomic data, real keys and anything else private on a
> hardware Ark. The emulator exists for development, demos and integration
> testing.

## Installation

Download a build for your operating system and processor from
[GitHub Releases](https://github.com/dark-bio/emulator/releases). Every
release carries QEMU and ArkOS, so there is nothing else to install.

| Platform | Installer | Portable version |
|---|---|---|
| macOS | `.dmg` | Unzip the `.zip` and open the app |
| Windows | `.exe` | Unzip the `.zip` and run `ark-emulator.exe` |
| Linux | `.deb` | Make the `.AppImage` executable and run it |

On macOS, Homebrew installs it too:

```sh
brew install --cask dark-bio/tap/ark-emulator
```

macOS needs version 15 (Sequoia) or later. Windows builds are unsigned, so
if SmartScreen blocks a downloaded release, choose **More info**, then
**Run anyway**.

## The window

1. Open **Ark Emulator**.
2. In the settings screen, choose **New** beside the file name to create an
   emulator file, or **Open** to pick one you have.
3. Choose **Save and start** to remember your choices and boot, or **Start**
   to boot without saving them.
4. Open [Ark Hub](https://hub.dark.bio) in Chrome or Edge and connect to the
   running emulator, or use `ark`.

Each `.ark` file is one emulated Ark. Open the same file to continue where
you left off, or create a new one for a blank device. Click the notch below
the device for its details, then the gear for the settings. **Autostart**
boots your saved emulator when the app opens, and the **developers**
section holds the memory and environment settings. Settings saved while an
emulator runs apply to the next launch, and an environment change applies
only to a newly created device.

Launch the app again, or choose **New Window** (Cmd+N) on macOS, to run
another emulator on another file. Close a window with your system's close
or quit shortcut, such as Alt+F4 on Windows or Cmd+Q on macOS.

## Command line

`ark-emulator` manages emulators without a window. Homebrew, the Windows
installer and the `.deb` put it on your `PATH`, and on Windows a new
terminal window picks it up. From the portable Windows ZIP, run
`.\bin\ark-emulator.cmd` instead, and on macOS and Linux the executable
inside the portable build.

```sh
ark-emulator start --headless   # boot without a window, return once ready
ark-emulator list               # what is running on this computer
ark-emulator button press       # press the reset button once
ark-emulator stop               # shut it down
ark-emulator wipe --yes         # reset the stopped image to a fresh device
```

Without `--headless`, `start` opens the device window.
`ark-emulator --headless`, with no command, runs an emulator in the
foreground instead, until Ctrl-C. Hand the locator `start` prints to `ark`,
which drives the emulated Ark as it drives hardware:

```sh
ark --device emulator:18181 status   # identity, firmware, pairing and lock
ark enroll                           # prints where to enroll it at Ark Hub
```

A fresh emulator is enrolled once at Ark Hub, in a browser, before it pairs.
The enrollment lasts 30 days, and then `ark-emulator stop`, `wipe` and
`start` give a fresh device to enroll again.

## Help

`ark-emulator --help` is the overview, `ark-emulator <command> --help` a
command's page with its contract, and `ark-emulator help --all` the whole
manual. AI agents read [`ark-emulator help agents`](launcher/src/cli/help/agents.md)
first, then `ark help agents` before driving the Ark itself. The topics'
sources read the same on GitHub:

| Topic | Contents |
|---|---|
| [`agents`](launcher/src/cli/help/agents.md) | Running ark-emulator from an AI agent, and the common tasks |
| [`images`](launcher/src/cli/help/images.md) | The image that is the device, where it lives, its environment and identity, and wiping |
| [`registry`](launcher/src/cli/help/registry.md) | The listing of running emulators, for a tool that reads it itself |
| [`output`](launcher/src/cli/help/output.md) | Reading output, JSON, logs and failures |
| [`errors`](launcher/src/cli/help/errors.md) | Every error code by exit class, and when running a command again helps |

Apps to run on an emulator, in Rust, Go, C and Python, are in the
[examples](https://github.com/dark-bio/examples) repository.

## Build from source

Install Rust, the
[Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/) and
QEMU, `qemu-img` included. Download a matching kernel and initramfs for your
processor, `arm64` or `amd64`, from
[the emulator firmware releases](https://github.com/dark-bio/emulator-images/releases).
A source build bundles neither QEMU nor firmware. From this repository:

```sh
cargo build --release -p launcher
./target/release/ark-emulator \
  --kernel /path/to/kernel \
  --initrd /path/to/initrd.gz
```

Packaging builds QEMU with `make qemu-linux`, `make qemu-macos` or
`make qemu-windows`. Linux and Windows targets build on Linux amd64 with
Docker; macOS builds on its native architecture. The builder fetches the GitLab
revision pinned in `.github/packaging/qemu/source.json` into `target/` on
demand. The builder, pinned inputs, Make targets and CI actions live together
in `.github/packaging/qemu/`. CI reuses verified runtime and source archives
when its cache matches the build inputs. Every packaged release includes the
corresponding QEMU sources, dependency sources and build recipes.

## Layout

| Path | Role |
|---|---|
| `launcher/` | Rust runtime and optional Tauri window. Owns QEMU, the hardware connection and discovery, and carries the packaging configuration and macOS entitlements. |
| `ui/` | Static HTML, CSS and JS. Renders the device state from Rust and forwards input through Tauri commands. |
| `docs/` | Maintainer documentation, today the one-time Apple Developer setup that macOS signing in CI depends on. |
| `.github/packaging/qemu/` | QEMU builder, pinned inputs, Make targets and CI actions. Produces the runtime and matching source archive for each platform. |
| `.github/` | CI workflows, firmware fetching and release packaging configuration. |
