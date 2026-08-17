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

## Prerequisites

All platforms also need a pair of ArkOS emulator firmware artifacts: a kernel
image (`<base>-kernel.<arch>`) and a gzipped initramfs (`<base>-initrd.<arch>.gz`),
built for either `arm64` or `amd64` by the firmware repo's
`./arkos.sh build-emulator`. The launcher takes their paths via `--kernel` and
`--initrd`, and their architecture via `--arch`, which defaults to the host's
architecture and picks the matching QEMU system emulator.

When the guest architecture matches the host's, the launcher enables hardware
virtualization (KVM on Linux, needing access to `/dev/kvm`, typically via the
`kvm` group; HVF on macOS) and falls back to plain emulation if unavailable.
Cross-architecture guests always run under plain emulation and boot noticeably
slower.

### Linux

- `qemu-system-aarch64` and/or `qemu-system-x86_64` (matching your firmware artifacts): Arch: `pacman -S qemu-system-aarch64 qemu-system-x86`; Debian/Ubuntu: `apt install qemu-system-arm qemu-system-x86`.
- `qemu-img` (used to allocate the disk image): Arch: `pacman -S qemu-img`; Debian/Ubuntu: `apt install qemu-utils`.
- `webkit2gtk-4.1` + `libsoup3`: Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`.

### macOS

- QEMU: `brew install qemu` (ships both `qemu-system-aarch64` and `qemu-system-x86_64`).
- WKWebView ships with the OS; nothing extra needed.

### Windows

- QEMU: the official Windows installer (ships both `qemu-system-aarch64` and `qemu-system-x86_64`).
- WebView2 runtime: preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft.

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
| `--kernel` | *required* | path to the kernel image (`<base>-kernel.<arch>`) |
| `--initrd` | *required* | path to the initramfs (`<base>-initrd.<arch>.gz`) |
| `--arch` | host arch | CPU architecture of the firmware artifacts (`arm64` or `amd64`) |
| `--disk` | `disk.img` | path to the backing disk; auto-allocated on first run |
| `--env` | `release` | cloud environment the device is bound to when its disk is first created; ignored for existing disks (the binding is burnt in) |
| `--host-addr` | `127.0.0.1:18181` | host address that SLIRP forwards into the guest's `:18181` |
| `--memory` | `8192` | guest RAM in MiB; lower it on memory-constrained hosts |

Run with `--help` for the full list.

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU, hosts the window. |
| `ui/` | Static HTML/CSS/JS. Renders the device + pin, drives the firmware's `/v1/hw` driver bus. |

