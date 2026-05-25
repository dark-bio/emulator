# Dark Bio — Ark Emulator

A desktop app that runs an Ark device locally for development and demo
purposes. It boots the real ArkOS firmware inside QEMU and hosts a small
native window that stands in for the device's physical hardware face (4 RGB
LEDs and a reset pin).

```
host browser / dashboard ──TCP──▶ host:8080 ──SLIRP hostfwd──▶ guest:8080 ──▶ firmware
                                                                                │
emulator UI (this window) ◀──/hw bus──▶ firmware driver layer ◀────────────────┘
```

The launcher process spawns QEMU, hosts a Tauri window pointed at `ui/`, and
ties everything's lifecycle together (closing the window quits QEMU; QEMU
exiting closes the window; `PR_SET_PDEATHSIG` cleans up on Linux even if the
launcher dies abnormally).

## Layout

| path | role |
|---|---|
| `launcher/` | Tauri app (Rust). Spawns QEMU with `hostfwd`, opens a fixed-size native window hosting `ui/index.html`. |
| `ui/` | Static HTML/CSS/JS. Renders the device + reset pin, drives the firmware's `/hw` driver bus (LED frames in, button edges out). |

The firmware itself is not in this repo — it's built from the (closed-source)
firmware repository and consumed here as an opaque artifact set: `vmlinuz`,
`initramfs.gz`, and `disk.img`.

## The `/hw` bus

The firmware exposes one WebSocket endpoint at `ws://<host>:8080/hw` carrying
driver-bus messages with a small envelope:

```json
{ "d": "<driver>", "id": "<instance>", "payload": { ... } }
```

Currently rendered by the UI:

- **`rgbled`** (firmware → UI): `payload.colors` is a length-4 array of
  `[r,g,b]` triples (sRGB888), one per corner LED.
- **`button`** (UI → firmware): `payload.edge` is `"falling"` (press) or
  `"rising"` (release); `id` is the GPIO pin (`"5"` on R3+ revbits boards,
  `"27"` on R0–R2).

Other driver tags (`switch`, `revbits`, `bootblink`) flow over the same bus
and are ignored by the UI for now.

## Prerequisites

- Linux host (macOS and Windows partly supported — see notes below)
- `qemu-system-aarch64` (Arch: `pacman -S qemu-system-aarch64`; Debian/Ubuntu: `apt install qemu-system-arm`; macOS: `brew install qemu`)
- System webview, used by the launcher to host the emulator UI:
  - **Linux:** `webkit2gtk-4.1` + `libsoup3` (Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`)
  - **macOS:** WKWebView ships with the OS, no install needed
  - **Windows:** WebView2 runtime (preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft)
- A firmware artifact directory (`vmlinuz` + `initramfs.gz` + `disk.img`)
  produced by the firmware repo's `./arkos.sh emulator-build`.

## Run

```sh
# Point EMULATOR_FIRMWARE at the artifact directory and launch.
EMULATOR_FIRMWARE=/path/to/firmware/build cargo run --release -p launcher
```

The window shows the device face. The reset pin is a real button: clicking
it sends a falling edge on `/hw`; releasing sends a rising edge. The four
corner LEDs render whatever the firmware streams from its RGB-LED driver.

Press **Escape** to close the window (Alt+F4 / WM shortcuts also work).

## Configuration

The launcher reads two env vars:

| var | default | meaning |
|---|---|---|
| `EMULATOR_FIRMWARE` | `firmware/build` | directory containing `vmlinuz`, `initramfs.gz`, and `disk.img` |
| `EMULATOR_HOST_ADDR`| `127.0.0.1:8080` | host address that SLIRP forwards into the guest's `:8080` |

## How the layers fit together

1. `launcher` opens a fixed-size, decorationless native window via Tauri,
   loads `ui/index.html` into the system webview (assets served via Tauri's
   `frontendDist`), and spawns `qemu-system-aarch64 -M virt` with the kernel,
   initramfs, a virtio-blk disk, a virtio-net device, and a
   `-netdev user,...,hostfwd=tcp:HOST-:8080` rule.
2. `PR_SET_PDEATHSIG=SIGKILL` (Linux) ensures QEMU dies if the launcher
   exits or is killed; a background thread watches QEMU and calls
   `process::exit(0)` if QEMU dies first, closing the window with the
   process.
3. Inside the guest, ArkOS boots, brings up its network interface, and runs
   `arkos-core`, which exposes its driver bus on `0.0.0.0:8080/hw`.
4. The UI connects to that endpoint, renders `rgbled` driver frames into the
   four corner dots, and emits `button` edge events when the reset pin is
   pressed or released.
5. QEMU's SLIRP translates host-side `127.0.0.1:8080` TCP traffic into a
   socket connection to the guest's `10.0.2.15:8080`. The UI is end-to-end
   WebSocket with the firmware; no host process bridges bytes.
