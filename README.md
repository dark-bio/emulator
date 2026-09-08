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

## Downloading a release

The artifacts attached to [GitHub Releases](../../releases) ship with QEMU
and the ArkOS firmware bundled in for that platform's own host architecture
only. There's nothing else to install and no flags to pass, but also no
support for emulating a guest architecture other than the host's (that's a
source-build, developer-only need; see [Prerequisites](#prerequisites)).

Each platform has both an installed and a no-install option:

| Platform | Installed | No-install |
|---|---|---|
| Linux | `.deb` | `.AppImage`: mark it executable and run it |
| macOS | `.dmg` | `.zip`: unzip, the `.app` inside runs from anywhere |
| Windows | NSIS `.exe` installer | `.zip`: unzip, run `Ark Emulator.exe` from inside |

The macOS builds are signed with an Apple Developer ID and notarized, so they
open normally. The Windows builds are unsigned, and your OS will flag them on
first run:

- **macOS**: nothing to do. Builds from source are ad-hoc signed instead, and
  those Gatekeeper does block: open **System Settings** → **Privacy &
  Security** → **Security** and click **Open Anyway**.
- **Windows**: SmartScreen shows "Windows protected your PC". Click **More
  info**, then **Run anyway**.
- **Linux**: no OS-level gatekeeping for an unsigned `.deb`/AppImage; nothing
  extra needed.

## Building from source

### Prerequisites

A source build uses whatever QEMU you have installed, and takes the firmware
as flags. Nothing is bundled, so unlike a released installer it can emulate
either guest architecture regardless of host.

**QEMU**, which also provides `qemu-img`:

- **Linux**: Arch: `pacman -S qemu-system-aarch64 qemu-system-x86 qemu-img`; Debian/Ubuntu: `apt install qemu-system-arm qemu-system-x86 qemu-utils`. Also needs `webkit2gtk-4.1` + `libsoup3`: Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`.
- **macOS**: `brew install qemu`. WKWebView ships with the OS; nothing extra needed.
- **Windows**: the official Windows installer. WebView2 runtime is preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft.

**Firmware**: a kernel image (`<base>-kernel.<arch>`) and a gzipped initramfs
(`<base>-initrd.<arch>.gz`), for either `arm64` or `amd64`. There is nothing to
build here: both are published as release assets at
[dark-bio/emulator-images](https://github.com/dark-bio/emulator-images/releases),
which is where a packaged build gets the firmware it bundles. Download a
matching pair and pass their paths via `--kernel` and `--initrd` (see
[Build and run](#build-and-run)).

When the guest architecture matches the host's, the launcher enables hardware
virtualization (KVM on Linux, needing access to `/dev/kvm`, typically via the
`kvm` group; HVF on macOS) and falls back to plain emulation if unavailable.
Cross-architecture guests always run under plain emulation and boot noticeably
slower. `--arch` picks the guest architecture and defaults to the host's.

### Build and run

```sh
cargo run --release -p launcher -- \
  --kernel /path/to/<base>-kernel.<arch> \
  --initrd /path/to/<base>-initrd.<arch>.gz
```

## Using the emulator

Everything below applies to a downloaded release and a source build alike. The
only difference is that a release already knows where its firmware is, so it
takes no flags to start.

The window shows the device face. The reset pin is a real button; the four
corner LEDs render whatever the firmware streams from its RGB-LED driver. The
notch under the device opens a tray naming the disk image this run is booted
from, with the full path on hover, alongside what the firmware reports about
the emulated device: the name it has been given, its serial, how long its
identity stays valid, and the environment the disk is bound to. The exact
expiry date is on hover. A device that has not been onboarded yet reads `not
onboarded` and shows no expiry, an unnamed one shows no name, and the rows stay
blank until the firmware has booted far enough to report them. Renaming or
onboarding the device updates the tray without a restart. The environment is
the one read back off the running device, so it is what `--env` settled on when
the disk was created rather than whatever this run happened to pass.

The first launch asks where to keep the backing disk, unless `--disk` says.
Whatever you choose is remembered in [the settings file](#settings) and reused
from then on. The disk is a dynamically growing qcow2 image, created if it is
not already there: it starts a few hundred KB in size and grows on demand as
the guest writes, up to a fixed virtual ceiling. Delete it to reset the
emulated device's state.

Press **Escape** to close the window (Alt+F4 / WM shortcuts also work).

### Running several at once

Start the emulator again and you get a second device, independent of the first.
Each instance takes the next free port from 18181 up, and each needs a disk
image of its own, so the second launch asks where to keep one rather than
reusing the image the first is booted from. Two guests writing one qcow2 would
corrupt it, so an explicit `--disk` naming an image that is already booted is
refused outright.

On macOS, Finder and `open` will not start a second copy of an app that is
already running. Ask for one explicitly:

```sh
open -n "/Applications/Ark Emulator.app"
```

Finding those instances is what the registry is for. Whichever launcher holds
`127.0.0.1:18180` serves it, and the rest publish themselves into it, so it
lives for as long as any emulator does without a process of its own. Any tool
can read it:

```sh
curl -s http://127.0.0.1:18180/v1/instances
```

```json
{ "version": 1,
  "instances": [
    { "port": 18181, "disk": "ark-a.img", "disk_id": "00902e5bf20c3a9a",
      "ready": true, "env": "develop", "name": "test ark" } ] }
```

`port` is the one that instance forwards into its guest, and `ready` says
whether its firmware has booted far enough to accept a client. `disk` is the
image's file name, deliberately not its path: any page in any browser can read
a loopback port, so nothing here says where anybody's files live. Entries last
only as long as they keep being refreshed, so an emulator that is killed
outright drops out within about fifteen seconds.

No launcher is special, and there is nothing to start or stop by hand. Closing
the one that happened to be serving frees the port, and the next launcher whose
heartbeat cannot be delivered takes over. The registry it starts with is empty
and refills as the others heartbeat, so for up to five seconds after that a
listing can come back short. An emulator that cannot reach or host a registry
at all boots anyway, without discovery.

### Configuration

| flag | default | meaning |
|---|---|---|
| `--kernel` | bundled firmware | path to the kernel image (`<base>-kernel.<arch>`); a source build has no bundled firmware, so pass this explicitly |
| `--initrd` | bundled firmware | path to the initramfs (`<base>-initrd.<arch>.gz`); see `--kernel` |
| `--arch` | host arch | CPU architecture of the firmware artifacts (`arm64` or `amd64`) |
| `--disk` | the remembered disk | path to the backing disk, for this run only; auto-allocated if it isn't there yet. Overrides the settings file without changing it |
| `--env` | `release` | cloud environment the device is bound to when its disk is first created; ignored for existing disks (the binding is burnt in) |
| `--host-addr` | first free port from 18181 | host address that SLIRP forwards into the guest's `:18181`. Given explicitly, it is used as-is, so a collision is QEMU's error to report |
| `--memory` | `8192` | guest RAM in MiB; lower it on memory-constrained hosts |

Run with `--help` for the full list.

### Settings

Anything the emulator remembers between runs lives in a `settings.toml` under
its own data directory:

| Platform | Path |
|---|---|
| Linux | `~/.local/share/bio.dark.emulator/settings.toml` |
| macOS | `~/Library/Application Support/bio.dark.emulator/settings.toml` |
| Windows | `%APPDATA%\bio.dark.emulator\settings.toml` |

Nothing ships with the app. The file is written on first run, so a portable
copy carried to another machine starts fresh there.

```toml
version = 1
disk = "/home/you/arks/demo.img"
```

`version` is the schema version, and a file from a newer emulator than the one
reading it is an error rather than something to overwrite. `disk` is the image
to boot when `--disk` is not given; it appears once something has been chosen.
Delete the file, or just that line, to be asked again. So does pointing it at
an image that no longer exists.

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU, hosts the window, and carries the packaging config and macOS entitlements. |
| `ui/` | Static HTML/CSS/JS. Renders the device + pin, drives the firmware's `/v1/hw` driver bus. |
| `docs/` | Maintainer documentation. Currently the one-time Apple Developer setup the macOS signing in CI depends on. |
| `.github/` | CI. Builds an installer per platform, then smoke tests each no-install artifact on a clean machine. The scripts under `scripts/` gather a relocatable QEMU and the pinned firmware for packaging; they are used by CI and runnable by hand. |
