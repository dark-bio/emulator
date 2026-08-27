//! Spawns QEMU with the firmware image and hosts a Tauri window for the
//! emulator UI. Window and QEMU are lifecycle-bound, so closing either tears
//! down the other on every platform, even on a hard kill:
//!
//!   ws clients ──TCP──▶ host:18181 ──QEMU SLIRP hostfwd──▶ guest:18181 ──▶ firmware
//!
//!   - QEMU exits, and the wait thread below exits the launcher.
//!   - The launcher exits, and QEMU dies with it via the OS-specific
//!     protection in `orphan`, which survives SIGKILL and force-quit.
//!
//! The backing disk image is an unencrypted qcow2 file that starts small and
//! grows on demand. There is no encryption at the qemu layer. The emulator is
//! a dev/test convenience, not a vault; see README.
//!
//!   - `qemu`:     the QEMU command line, the guest it builds, and its disk.
//!   - `bundle`:   where a packaged build's firmware, sidecars and libs live.
//!   - `platform`: OS-specific quirks, so the other three stay cfg-free.
//!   - `orphan`:   ties QEMU's lifetime to this process.

// Release builds link as a GUI app on Windows so launching the app doesn't pop
// up a console alongside the UI. Debug builds keep the console subsystem so
// `cargo run`'s diagnostics still print. See `platform` for the rest of the
// Windows console story, including the one QEMU would otherwise get.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bundle;
mod orphan;
mod platform;
mod qemu;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread;

use clap::Parser;

use bundle::{resolve_disk_path, resolve_firmware, resolve_qemu_libs};
use qemu::{ensure_disk, spawn_qemu, GuestArch};

/// Launch configuration parsed from command-line arguments.
#[derive(Parser)]
#[command(about = "Ark device emulator: boots ArkOS in QEMU behind a small UI.")]
struct Config {
    /// Path to the kernel image (vmlinuz). Defaults to the firmware bundled
    /// with this build for --arch. Must be given together with --initrd.
    #[arg(long)]
    kernel: Option<PathBuf>,

    /// Path to the initramfs (.gz). See --kernel.
    #[arg(long)]
    initrd: Option<PathBuf>,

    /// CPU architecture of the firmware artifacts; defaults to the host's
    /// architecture.
    #[arg(long, value_enum)]
    arch: Option<GuestArch>,

    /// Path to the backing disk image; auto-allocated on first run. Defaults
    /// to a `disk.img` under this app's data directory.
    #[arg(long)]
    disk: Option<PathBuf>,

    /// Cloud environment the device gets bound to when its disk is first
    /// created; ignored for existing disks (the binding is burnt in).
    #[arg(long, default_value = "release", value_parser = ["develop", "staging", "release"])]
    env: String,

    /// Host address that SLIRP forwards into the guest's port.
    #[arg(long, default_value = "127.0.0.1:18181")]
    host_addr: SocketAddr,

    /// Guest RAM in MiB. Lower it on memory-constrained hosts.
    #[arg(long, default_value_t = 8192)]
    memory: u32,
}

fn main() {
    let cfg: Config = Config::parse();

    // The host's architecture is also the only one that gets hardware
    // acceleration, so it is the default.
    let arch = cfg.arch.unwrap_or_else(|| match std::env::consts::ARCH {
        "aarch64" => GuestArch::Arm64,
        "x86_64" => GuestArch::Amd64,
        other => {
            eprintln!("no firmware exists for {other} hosts; pass --arch explicitly");
            std::process::exit(1);
        }
    });

    // The /v1/hw address is fixed at launch, so hand it to the UI as a
    // constant injected before page scripts run rather than over a command.
    // The UI reads window.__HW_ADDR__ and dials it, following --host-addr
    // instead of assuming a fixed port.
    let hw_addr = format!("window.__HW_ADDR__ = {:?};", cfg.host_addr.to_string());
    tauri::Builder::default()
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("hw-addr")
                .js_init_script(hw_addr)
                .build(),
        )
        .setup(move |app| {
            let (kernel, initrd) = resolve_firmware(app, &cfg, arch)?;
            let qemu_libs = resolve_qemu_libs(app);
            let disk = resolve_disk_path(app, &cfg)?;

            if let Err(e) = ensure_disk(&disk, qemu_libs.as_deref()) {
                eprintln!(
                    "[launcher] failed to prepare disk image at {}: {e}",
                    disk.display()
                );
                std::process::exit(1);
            }

            // A background thread watches QEMU and exits the process if it
            // dies on its own, which closes Tauri's window with us.
            let qemu = spawn_qemu(&cfg, arch, &kernel, &initrd, &disk, qemu_libs.as_deref());
            thread::spawn(move || {
                let mut child = qemu;
                match child.wait() {
                    Ok(status) => {
                        eprintln!("[launcher] QEMU exited with {status}");
                        std::process::exit(status.code().unwrap_or(0));
                    }
                    Err(e) => {
                        eprintln!("[launcher] wait on QEMU failed: {e}");
                        std::process::exit(1);
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
