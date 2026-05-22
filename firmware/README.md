# firmware/ — placeholder for the real Ark firmware

Everything in this directory is **throwaway** and exists only so the rest of
the emulator (the bridge + UI) has something to talk to during development.

In production, the bridge consumes a prebuilt artifact (kernel + initramfs)
produced by the real, closed-source Ark firmware repository. At that point
this directory can be deleted entirely.

## What's here

| | |
|---|---|
| `helloware/` | A tiny Rust binary, cross-compiled to `aarch64-unknown-linux-musl`, that runs as PID 1 in the QEMU guest. It hosts a WebSocket server on `0.0.0.0:8080` (path `/ws`) that echoes uppercased messages. |
| `build.sh` | Builds `build/vmlinuz` (Alpine `linux-virt` aarch64 kernel) and `build/initramfs.gz` (busybox + helloware + a small `/init` that brings up `eth0` before `exec`'ing helloware). Uses Docker with `linux/arm64` emulation. |
| `build/` | Output directory (gitignored). |

## Build

```sh
./build.sh
```

Outputs:

- `build/vmlinuz` — kernel
- `build/initramfs.gz` — initramfs

The bridge reads these from `firmware/build/` by default. Override with the
`EMULATOR_FIRMWARE` env var to point at any directory containing `vmlinuz` and
`initramfs.gz` (which is how it'll consume real firmware artifacts later).

## Why isn't this a Cargo workspace member?

`helloware/` is intentionally outside the top-level Cargo workspace
(`exclude = ["firmware"]` in `../Cargo.toml`). It's cross-compiled to a
different target, has its own `.cargo/config.toml`, and conceptually lives
outside this repo — keeping it a standalone crate makes that boundary obvious.
