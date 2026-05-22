# Dark Bio - Ark Emulator

This repository contains the emulator to run an Ark device locally as a development environment or for trial / demo purposes.

## Hello World

A minimal end-to-end demonstration of the emulator stack. A line sent over a
WebSocket reaches the firmware running inside QEMU and comes back uppercased:

```
ws client ──TCP──▶ host:8080 ──QEMU SLIRP hostfwd──▶ guest:8080 ──▶ firmware WS server
```

No protocol translation in the host process — the firmware speaks WebSocket
itself. The launcher just spawns QEMU and supervises it.

## Layout

| path | role | scope |
|---|---|---|
| `launcher/` | host Rust binary: opens a fixed-size native window (Tao + Wry / system webview), hosts `ui/index.html` inside it, spawns QEMU with `hostfwd`, ties QEMU's lifecycle to the window | **stays** (this repo) |
| `ui/` | the emulator's own UI, embedded by the launcher into its window. Represents the device's *hardware* face — in production a panel with 4 LEDs + a reset button; here a single switch on the firmware's `/hw` channel that toggles uppercase / lowercase echoing | **stays** (this is what the emulator project actually owns) |
| `dashboard/` | demo client that plays the role of the production dashboard — speaks the firmware's `/ws` (dashboard) protocol. Opened in a browser separately from the launcher window. Replaced by the real dashboard in production | **stays for the demo**, replaced in production |
| `firmware/` | placeholder for the real Ark firmware: `helloware/` Rust binary (hosts the `/ws` + `/hw` WS endpoints) + `build.sh` to produce a kernel + initramfs | **goes away** once the real (closed-source) firmware repo ships artifacts |

The launcher consumes firmware as an opaque artifact (a `vmlinuz` + an
`initramfs.gz` in some directory). The hello-world `firmware/` directory just
happens to know how to produce one; nothing in `launcher/` or `ui/` reaches
into `firmware/` for code.

## Prerequisites

- Linux host (macOS and Windows support is partial — see notes below)
- `rustup` with the `aarch64-unknown-linux-musl` target (`rustup target add aarch64-unknown-linux-musl`)
- `docker` configured with `linux/arm64` emulation (typically via `binfmt`/`qemu-user-static`)
- `qemu-system-aarch64` (Arch: `pacman -S qemu-system-aarch64`; Debian/Ubuntu: `apt install qemu-system-arm`; macOS: `brew install qemu`)
- System webview, used by the launcher to host the emulator UI:
  - **Linux:** `webkit2gtk-4.1` + `libsoup3` (Arch: `pacman -S webkit2gtk-4.1`; Debian/Ubuntu: `apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`)
  - **macOS:** WKWebView ships with the OS, no install needed
  - **Windows:** WebView2 runtime (preinstalled on recent Windows 10/11; otherwise downloadable from Microsoft)

## Run

```sh
# 1. one-time: build the placeholder firmware (kernel + initramfs)
./firmware/build.sh

# 2. every run: the launcher opens its own native window (the emulator UI)
# and runs QEMU only while that window is alive. Closing the window stops
# everything; QEMU exiting on its own also closes the window.
cargo run --release -p launcher

# 3. (demo only) open the dashboard stand-in in a browser to talk /ws:
#    open dashboard/index.html
#    -- or serve it: python3 -m http.server -d dashboard 8081 && open http://127.0.0.1:8081
```

The launcher's window embeds `ui/index.html` directly via the system webview;
no browser-side UI is needed to interact with the emulator's hardware face.

The demo `dashboard/` UI defaults to `ws://127.0.0.1:8080/ws`; override with
`?bridge=ws://other:port/ws`.

Type a message in the dashboard UI, press enter:

```
  you   > hello there
  guest > HELLO from helloware: HELLO THERE
```

Flip the case switch in the emulator's `ui/` to `lower`, then type again:

```
  you   > hello there
  guest > HELLO from helloware: hello there
```

## Configuration

The launcher reads two env vars:

| var | default | meaning |
|---|---|---|
| `EMULATOR_FIRMWARE` | `firmware/build` | directory containing `vmlinuz` and `initramfs.gz` |
| `EMULATOR_HOST_ADDR`| `127.0.0.1:8080` | host address that SLIRP forwards into the guest's `:8080` |

Pointing `EMULATOR_FIRMWARE` at a directory holding a real Ark firmware
artifact is how production deployment will work — no source changes to the
launcher.

## How the layers fit together

1. `firmware/build.sh` cross-compiles `helloware` to
   `aarch64-unknown-linux-musl`, then runs an `linux/arm64` Alpine container
   that installs `linux-virt` + `busybox-static`, drops in helloware, and
   packages a gzipped newc cpio initramfs.
2. `launcher` opens a fixed-size native window via Tao + Wry, embeds
   `ui/index.html` directly into the system webview (no browser, no HTTP
   server), and spawns `qemu-system-aarch64 -M virt` with the kernel +
   initramfs, a virtio-net device, and a `-netdev user,...,hostfwd=tcp:HOST-:8080`
   rule. `PR_SET_PDEATHSIG=SIGKILL` (Linux) ensures QEMU dies if the launcher
   exits or is killed; a background `wait()` thread on QEMU calls `exit(0)`
   if QEMU dies first, closing the window with the process.
3. Inside the guest, `/init` mounts `/proc`, `/sys`, `/dev`, assigns
   `10.0.2.15/24` to `eth0`, brings it up, then `exec`s `/usr/bin/helloware`.
4. `helloware` runs an axum WebSocket server on `0.0.0.0:8080` with two routes:
   `/ws` (the dashboard channel — echoes text) and `/hw` (the hardware
   channel — exposes a case toggle). The two share an `AtomicBool` so the
   hardware UI's switch state determines how `/ws` cases its replies.
5. QEMU's SLIRP translates host-side `127.0.0.1:8080` TCP traffic into a
   socket connection to the guest's `10.0.2.15:8080`. The client is
   end-to-end WebSocket with the firmware; no host process bridges bytes.

## Next steps (not part of this demo)

- Replace `firmware/` with the real `arkos-core` emulator firmware artifact.
- Grow `ui/` toward the real Ark hardware face: 4 RGB LEDs (read from `/hw`)
  + a reset button (write to `/hw`).
- Mount a backing disk image for the eMMC partitions (`-drive` + virtio-blk).
- Wire the real dashboard's emulator-mode transport directly to the firmware's
  `/ws` (replacing the demo `dashboard/`).
