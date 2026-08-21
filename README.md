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
| macOS | `.dmg` | `.app.zip`: unzip, the `.app` runs from anywhere |
| Windows | NSIS `.exe` installer | `...-portable.zip`: unzip, run `Ark Emulator.exe` from inside |

They're currently unsigned (no Apple Developer ID / Windows code-signing
certificate yet), so your OS will flag them on first run:

- **macOS**: Gatekeeper blocks the app ("cannot be opened because the
  developer cannot be verified"). Right-click the app in Finder and choose
  **Open**, then confirm in the dialog that appears; this is only needed
  once.
- **Windows**: SmartScreen shows "Windows protected your PC". Click **More
  info**, then **Run anyway**.
- **Linux**: no OS-level gatekeeping for an unsigned `.deb`/AppImage; nothing
  extra needed.

The rest of this README covers building and running from source instead,
which needs the prerequisites below.

## Prerequisites

Building from source needs the same things a released installer bundles for
you: `qemu-img`, a QEMU system emulator, and a pair of ArkOS emulator
firmware artifacts. A released installer only ever bundles the host-native
guest architecture, to keep size down. A source build is not limited this
way: with QEMU installed as below, both `qemu-system-aarch64` and
`qemu-system-x86_64` are available on `PATH`, so it can still emulate either
guest regardless of host.

**QEMU**: `cargo build`/`cargo run`/`cargo check` all require
`launcher/binaries/` to already be populated (Tauri validates
`bundle.externalBin` at build time, not just at packaging time), so before
your first build, run the script for your platform once from the repo root:

```sh
.github/scripts/fetch-qemu-linux.sh     # Linux
.github/scripts/fetch-qemu-macos.sh     # macOS
pwsh .github/scripts/fetch-qemu-windows.ps1   # Windows
```

These are the same scripts CI uses to build released installers; run
standalone they populate `launcher/binaries/` and `launcher/qemu-libs/` from
whatever QEMU is on your system (installed as below), which is enough for
local development. `qemu-img` comes along with either QEMU install:

- **Linux**: Arch: `pacman -S qemu-system-aarch64 qemu-system-x86 qemu-img`; Debian/Ubuntu: `apt install qemu-system-arm qemu-system-x86 qemu-utils`. Also needs `webkit2gtk-4.1` + `libsoup3`: Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`.
- **macOS**: `brew install qemu` (ships both `qemu-system-aarch64` and `qemu-system-x86_64`, plus `qemu-img`). WKWebView ships with the OS; nothing extra needed.
- **Windows**: the official Windows installer (ships both `qemu-system-aarch64` and `qemu-system-x86_64`, plus `qemu-img`). WebView2 runtime is preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft.

**Firmware**: a kernel image (`<base>-kernel.<arch>`) and a gzipped initramfs
(`<base>-initrd.<arch>.gz`), built for either `arm64` or `amd64` by the
firmware repo's `./arkos.sh build-emulator`. Pass their paths via `--kernel`
and `--initrd` (see [Run](#run)): a local build has no bundled firmware to
fall back to, since `launcher/firmware/` is only populated by CI.

When the guest architecture matches the host's, the launcher enables hardware
virtualization (KVM on Linux, needing access to `/dev/kvm`, typically via the
`kvm` group; HVF on macOS) and falls back to plain emulation if unavailable.
Cross-architecture guests always run under plain emulation and boot noticeably
slower. `--arch` picks the guest architecture and defaults to the host's.

## Run

```sh
cargo run --release -p launcher -- \
  --kernel /path/to/<base>-kernel.<arch> \
  --initrd /path/to/<base>-initrd.<arch>.gz
```

The window shows the device face. The reset pin is a real button; the four
corner LEDs render whatever the firmware streams from its RGB-LED driver. The
backing disk is created automatically on first launch as a dynamically growing
qcow2 image: it starts a few hundred KB in size and grows on demand as the guest
writes, up to a fixed virtual ceiling. Delete it to reset the emulated device's
state.

Press **Escape** to close the window (Alt+F4 / WM shortcuts also work).

## Configuration

| flag | default | meaning |
|---|---|---|
| `--kernel` | bundled firmware | path to the kernel image (`<base>-kernel.<arch>`); a source build has no bundled firmware, so pass this explicitly |
| `--initrd` | bundled firmware | path to the initramfs (`<base>-initrd.<arch>.gz`); see `--kernel` |
| `--arch` | host arch | CPU architecture of the firmware artifacts (`arm64` or `amd64`) |
| `--disk` | this app's data directory | path to the backing disk; auto-allocated on first run. Deliberately not the current directory, which isn't reliably writable for a packaged app |
| `--env` | `release` | cloud environment the device is bound to when its disk is first created; ignored for existing disks (the binding is burnt in) |
| `--host-addr` | `127.0.0.1:18181` | host address that SLIRP forwards into the guest's `:18181` |
| `--memory` | `8192` | guest RAM in MiB; lower it on memory-constrained hosts |

Run with `--help` for the full list.

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU, hosts the window. |
| `ui/` | Static HTML/CSS/JS. Renders the device + pin, drives the firmware's `/v1/hw` driver bus. |
| `.github/scripts/` | Populate `launcher/binaries/`, `launcher/qemu-libs/`, and `launcher/firmware/` (all gitignored) with a relocatable, host-arch-only QEMU and the pinned firmware release; used by CI and locally (see [Prerequisites](#prerequisites)). |
| `.github/workflows/release.yml` | Tag-triggered CI: builds macOS/Windows/Linux installers and attaches them to a GitHub Release. |

