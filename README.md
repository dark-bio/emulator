# Dark Bio - Ark Emulator

A desktop app that runs an Ark device locally for development and demo
purposes. It boots the real ArkOS firmware inside QEMU and hosts a small
native window standing in for the device's physical face (4 RGB LEDs and a
reset pin).

> [!CAUTION]
> **The emulator is not a vault.** The backing disk image is an unencrypted
> qcow2 file on your host filesystem. Anything the emulated Ark stores ends up there,
> readable by anyone with access to your machine. Do not put real genomic
> data, real keys, or anything else you want to keep private into the
> emulator. It exists for development, demos, and integration testing only.
> The cryptographic and physical-security properties Dark Bio promises apply
> to the actual Ark hardware, not this emulator. For any workload where data
> confidentiality matters, use a real Ark.

## Install

Download a build for your operating system and processor from
[GitHub Releases](https://github.com/dark-bio/emulator/releases). Releases
include QEMU and ArkOS, so you can launch the app without configuring either.

| Platform | Installer | Portable version |
|---|---|---|
| macOS | `.dmg` | Unzip the `.zip` and open the app |
| Windows | `.exe` | Unzip the `.zip` and run `ark-emulator.exe` |
| Linux | `.deb` | Make the `.AppImage` executable and run it |

On macOS, you can also install with Homebrew:

```sh
brew install --cask dark-bio/tap/ark-emulator
```

macOS requires version 15 (Sequoia) or later. Windows builds are unsigned;
if SmartScreen blocks a downloaded release, choose **More info**, then
**Run anyway**.

## Get started

1. Open **Ark Emulator**.
2. In the settings screen, use **New** beside the filename to create an
   emulator file, or **Open** to choose one you already have.
3. Choose **Save and start** to remember your choices and boot the emulator.
   Use **Start** to boot without saving your settings.
4. Open [Ark Hub](https://hub.dark.bio) in Chrome or Edge and connect to the
   running emulator.

Each `.ark` file stores one emulator's data. Open the same file to continue
where you left off, or create a new file to start with a blank device.

Click the notch below the device to see its information, then the gear to
open settings. **Autostart** starts your saved emulator automatically on the
next launch. The **developers** section contains memory and environment
settings. Settings saved while an emulator is running apply on a later launch;
an environment change applies only to a newly created device.

To close the window, use your operating system's close or quit shortcut,
such as Alt+F4 on Windows or Cmd+Q on macOS.

## Run multiple emulators

Launch the app again to open another window. On macOS, use **New Window**
(Cmd+N) in the app menu. Each running instance needs a different emulator file;
use **Open** or **New** in that window to choose one.

## Command line

The app's executable is `ark-emulator`, and it manages emulators without
opening a window. The Homebrew cask and the `.deb` put it on your `PATH`; from
a portable build, run the executable inside the app.

On Windows, use `./ark-emulator.ps1 list` in PowerShell. The script waits for
the command, preserves terminal colors, and returns its exit code. Keep it
beside `ark-emulator.exe`. Windows may require a downloaded script to be
unblocked or permitted by your PowerShell execution policy. For pipelines,
call the executable directly, such as `./ark-emulator.exe --json list |
ConvertFrom-Json`.

```sh
ark-emulator start        # boot one, and print its locator once it is ready
ark-emulator list         # show what is running on this computer
ark-emulator stop         # shut it down, the way closing its window does
```

Hand the locator to [`ark`](https://github.com/dark-bio/cli), which talks to an
emulated Ark exactly as it talks to hardware:

```sh
ark -d emulator:18181 status
```

A fresh emulator has a self-signed identity. `ark enroll` prints the Ark Hub
address that gives it an attested one, valid for 30 days; after that,
`ark-emulator stop`, `wipe` and `start` give a fresh device to enroll again.

`ark-emulator --help` lists every command and option, and `ark-emulator help`
names the reference topics, whose sources in
[`launcher/src/help`](launcher/src/help) read the same on GitHub. AI agents
should read [`ark-emulator help agents`](launcher/src/help/agents.md) first,
then `ark help agents` before driving the Ark itself. Apps to run on an
emulator, in Rust, Go, C and Python, are at
[examples](https://github.com/dark-bio/examples). Pairing and unlocking happen
in Ark Companion, on
[iOS](https://apps.apple.com/app/id6751324700) or
[Android](https://play.google.com/store/apps/details?id=bio.dark.companion).

## Build from source

Install Rust, the [Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/),
and QEMU, including `qemu-img`. Download a matching kernel and initramfs pair
for your processor (`arm64` or `amd64`) from
[the emulator firmware releases](https://github.com/dark-bio/emulator-images/releases).
Source builds do not bundle QEMU or firmware.

From this repository, run:

```sh
cargo build --release -p launcher
./target/release/ark-emulator \
  --kernel /path/to/kernel \
  --initrd /path/to/initrd.gz
```

For everything the executable can do, run
`./target/release/ark-emulator --help`.

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU, hosts the window, and carries the packaging config and macOS entitlements. |
| `ui/` | Static HTML/CSS/JS. Renders the device + pin, the info tray and the settings panel, and drives the firmware's `/v1/hw` driver bus. |
| `docs/` | Maintainer documentation. Currently the one-time Apple Developer setup the macOS signing in CI depends on. |
| `.github/` | CI. Builds an installer per platform, then smoke tests each no-install artifact on a clean machine. The scripts under `scripts/` gather a relocatable QEMU and the pinned firmware for packaging; they are used by CI and runnable by hand. `packaging/` holds the Homebrew cask template a release publishes to the tap. |
