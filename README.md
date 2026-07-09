# Dark Bio - Ark Emulator

A desktop app that runs an Ark device locally for development and demo
purposes. It boots the real ArkOS firmware inside QEMU and hosts a small
native window standing in for the device's physical face (4 RGB LEDs and a
reset pin).

> [!CAUTION]
> **The emulator is not a vault.** The backing disk image is a plain raw file
> on your host filesystem. Anything the emulated Ark stores ends up there,
> readable by anyone with access to your machine. Do not put real genomic
> data, real keys, or anything else you want to keep private into the
> emulator. It exists for development, demos, and integration testing only.
> The cryptographic and physical-security properties Dark Bio promises apply
> to the actual Ark hardware, not this emulator. For any workload where data
> confidentiality matters, use a real Ark.

## Prerequisites

All platforms also need a pair of ArkOS emulator firmware artifacts: a kernel
image (`vmlinuz`) and a gzipped initramfs. The launcher takes their paths via
`--kernel` and `--initrd`.

### Linux

- `qemu-system-aarch64`: Arch: `pacman -S qemu-system-aarch64`; Debian/Ubuntu: `apt install qemu-system-arm`.
- `webkit2gtk-4.1` + `libsoup3`: Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`.

### macOS

- `qemu-system-aarch64`: `brew install qemu`.
- WKWebView ships with the OS; nothing extra needed.

### Windows

- `qemu-system-aarch64`: install QEMU via its official Windows installer.
- WebView2 runtime: preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft.

## Run

```sh
cargo run --release -p launcher -- --kernel /path/to/vmlinuz --initrd /path/to/initramfs.gz
```

The window shows the device face. The reset pin is a real button; the four
corner LEDs render whatever the firmware streams from its RGB-LED driver. The
backing `disk.img` is created automatically on first launch (4 GiB sparse).
Delete it to reset the emulated device's state.

Press **Escape** to close the window (Alt+F4 / WM shortcuts also work).

## Configuration

| flag | default | meaning |
|---|---|---|
| `--kernel` | *required* | path to the kernel image (`vmlinuz`) |
| `--initrd` | *required* | path to the initramfs (`.gz`) |
| `--disk` | `disk.img` | path to the backing disk; auto-allocated on first run |
| `--host-addr` | `127.0.0.1:18181` | host address that SLIRP forwards into the guest's `:18181` |
| `--memory` | `8192` | guest RAM in MiB; lower it on memory-constrained hosts |

Run with `--help` for the full list.

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU, hosts the window. |
| `ui/` | Static HTML/CSS/JS. Renders the device + pin, drives the firmware's `/v1/hw` driver bus. |

