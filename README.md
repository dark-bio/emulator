# Dark Bio — Ark Emulator

A desktop app that runs an Ark device locally for development and demo
purposes. It boots the real ArkOS firmware inside QEMU and hosts a small
native window standing in for the device's physical face (4 RGB LEDs and a
reset pin).

> [!CAUTION]
> **The emulator is not a vault.** The backing disk image is a plain raw file
> on your host filesystem — anything the emulated Ark stores ends up there,
> readable by anyone with access to your machine. Do not put real genomic
> data, real keys, or anything else you want to keep private into the
> emulator. It exists for development, demos, and integration testing only.
> The cryptographic and physical-security properties Dark Bio promises apply
> to the actual Ark hardware, not this emulator. For any workload where data
> confidentiality matters, use a real Ark.

## Prerequisites

All platforms also need a firmware artifact directory containing `vmlinuz` +
`initramfs.gz`, produced by the firmware repo's `./arkos.sh emulator-build`.

### Linux

- `qemu-system-aarch64` — Arch: `pacman -S qemu-system-aarch64`; Debian/Ubuntu: `apt install qemu-system-arm`.
- `webkit2gtk-4.1` + `libsoup3` — Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`.

### macOS

- `qemu-system-aarch64` — `brew install qemu`.
- WKWebView ships with the OS; nothing extra needed.

### Windows

- `qemu-system-aarch64` — install QEMU via its official Windows installer.
- WebView2 runtime — preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft.

## Run

```sh
cargo run --release -p launcher -- --firmware /path/to/firmware/build
```

The window shows the device face. The reset pin is a real button; the four
corner LEDs render whatever the firmware streams from its RGB-LED driver. The
backing `disk.img` is created automatically on first launch (4 GiB sparse) —
delete it to reset the emulated device's state.

Press **Escape** to close the window (Alt+F4 / WM shortcuts also work).

## Configuration

| flag | default | meaning |
|---|---|---|
| `--firmware` | `firmware/build` | directory containing `vmlinuz` + `initramfs.gz` |
| `--disk` | `disk.img` | path to the backing disk; auto-allocated on first run |
| `--host-addr` | `127.0.0.1:8080` | host address that SLIRP forwards into the guest's `:8080` |

Run with `--help` for the full list.

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU, hosts the window. |
| `ui/` | Static HTML/CSS/JS. Renders the device + pin, drives the firmware's `/hw` driver bus. |

## Known issues

- **macOS / Windows: orphaned QEMU on abnormal exit.** If the launcher dies
  in a way that bypasses normal cleanup (`kill -9`, force quit, OOM kill,
  panic in FFI, etc.), QEMU may be left running and you'll need to terminate
  it manually. Linux is fine — `PR_SET_PDEATHSIG` makes the kernel take care
  of it.
