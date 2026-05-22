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
| `launcher/` | host Rust binary: spawns QEMU with `hostfwd`, pipes its serial console for visibility, kills QEMU when it exits | **stays** (this repo) |
| `ui/` | single-file HTML/JS demo client. In production, the dashboard plays this role | **stays** (separate concern from the launcher — launcher doesn't embed it) |
| `firmware/` | placeholder for the real Ark firmware: `helloware/` Rust binary (which hosts the WS server) + `build.sh` to produce a kernel + initramfs | **goes away** once the real (closed-source) firmware repo ships artifacts |

The launcher consumes firmware as an opaque artifact (a `vmlinuz` + an
`initramfs.gz` in some directory). The hello-world `firmware/` directory just
happens to know how to produce one; nothing in `launcher/` or `ui/` reaches
into `firmware/` for code.

## Prerequisites

- Linux host
- `rustup` with the `aarch64-unknown-linux-musl` target (`rustup target add aarch64-unknown-linux-musl`)
- `docker` configured with `linux/arm64` emulation (typically via `binfmt`/`qemu-user-static`)
- `qemu-system-aarch64` (Arch: `pacman -S qemu-system-aarch64`; Debian/Ubuntu: `apt install qemu-system-arm`; macOS: `brew install qemu`)

## Run

```sh
# 1. one-time: build the placeholder firmware (kernel + initramfs)
./firmware/build.sh

# 2. every run: launch QEMU with hostfwd
cargo run --release -p launcher

# 3. open the demo UI: open ui/index.html directly in a browser, or serve it:
#    python3 -m http.server -d ui 8081  &&  open http://127.0.0.1:8081
```

The UI defaults to `ws://127.0.0.1:8080/ws`; override with `?bridge=ws://other:port/ws`.

Type a message, press enter. Expected:

```
  you   > hello there
  guest > HELLO from helloware: HELLO THERE
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
2. `launcher` spawns `qemu-system-aarch64 -M virt` with the kernel + initramfs,
   a virtio-net device, and a `-netdev user,...,hostfwd=tcp:HOST-:8080` rule.
   `PR_SET_PDEATHSIG=SIGKILL` + `kill_on_drop` ensure QEMU dies with the
   launcher.
3. Inside the guest, `/init` mounts `/proc`, `/sys`, `/dev`, assigns
   `10.0.2.15/24` to `eth0`, brings it up, then `exec`s `/usr/bin/helloware`.
4. `helloware` runs an axum WebSocket server on `0.0.0.0:8080` at the path
   `/ws`. Each connection receives a banner and then echoes uppercased text.
5. QEMU's SLIRP translates host-side `127.0.0.1:8080` TCP traffic into a
   socket connection to the guest's `10.0.2.15:8080`. The client is
   end-to-end WebSocket with the firmware; no host process bridges bytes.

## Next steps (not part of this demo)

- Replace `firmware/` with the real `arkos-core` emulator firmware artifact.
- Multiplex per-purpose WS paths on the same port: `/dashboard` for the device
  protocol, `/hardware` for LED state / button events.
- Mount a backing disk image for the eMMC partitions (`-drive` + virtio-blk).
- Wire the dashboard's emulator-mode transport directly to the firmware's WS
  (replacing this demo UI).
