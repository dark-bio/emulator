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

Each asset has a `.sha256` file published alongside it; verify with
`sha256sum -c` (Linux/Windows) or `shasum -a 256 -c` (macOS).

The macOS builds are ad-hoc signed, which is enough to execute on Apple
Silicon but is not an Apple Developer ID and carries no notarization. The
Windows builds are unsigned outright. Either way your OS will flag them on
first run:

- **macOS**: Gatekeeper blocks the app on first launch. Open **System
  Settings** → **Privacy & Security** → **Security**, click **Open Anyway**
  (only available for about an hour after the blocked attempt), then enter
  your password to confirm. Only needed once.
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
(`<base>-initrd.<arch>.gz`), built for either `arm64` or `amd64` by the
firmware repo's own build tooling. Pass their paths via `--kernel` and
`--initrd` (see [Run](#run)).

When the guest architecture matches the host's, the launcher enables hardware
virtualization (KVM on Linux, needing access to `/dev/kvm`, typically via the
`kvm` group; HVF on macOS) and falls back to plain emulation if unavailable.
Cross-architecture guests always run under plain emulation and boot noticeably
slower. `--arch` picks the guest architecture and defaults to the host's.

### Run

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

### Configuration

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
| `launcher/tauri.release.conf.json` | Adds the bundled QEMU and firmware to the base Tauri config. Applied by CI only, so a source build stays free of them. |
| `.github/scripts/` | CI-only: gather a relocatable, host-arch QEMU and the pinned firmware release for packaging, and code-sign what macOS requires signed. |
| `launcher/entitlements.plist` | macOS entitlements. Restores what the Hardened Runtime takes away: hardware acceleration, the TCG JIT, and `DYLD_LIBRARY_PATH`. |
| `.github/workflows/release.yml` | Tag-triggered CI: builds macOS/Windows/Linux installers and attaches them to a GitHub Release. |

