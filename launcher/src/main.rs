// launcher: spawns QEMU with the firmware image and hosts a Tauri window
// for the emulator UI. Window and QEMU are lifecycle-bound — closing either
// tears down the other, on every platform and even on a hard kill:
//   - QEMU exits  -> the wait thread exits the launcher (all platforms).
//   - launcher exits -> QEMU dies with it via the OS-specific protection in
//     the `orphan` module, which survives SIGKILL/force-quit.
//
//   ws clients ──TCP──▶ host:18181 ──QEMU SLIRP hostfwd──▶ guest:18181 ──▶ firmware
//
// The backing disk image is plain raw — no encryption at the qemu layer.
// The emulator is a dev/test convenience, not a vault; see README.

mod orphan;

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;

use clap::Parser;

/// The port the firmware uses to communicate with the host.
const GUEST_PORT: u16 = 18181;

/// Launch configuration parsed from command-line arguments.
#[derive(Parser)]
#[command(about = "Ark device emulator: boots ArkOS in QEMU behind a small UI.")]
struct Config {
    /// Path to the kernel image (vmlinuz).
    #[arg(long)]
    kernel: PathBuf,

    /// Path to the initramfs (.gz).
    #[arg(long)]
    initrd: PathBuf,

    /// Path to the backing disk image; auto-allocated on first run.
    #[arg(long, default_value = "disk.img")]
    disk: PathBuf,

    /// Host address that SLIRP forwards into the guest's port.
    #[arg(long, default_value = "127.0.0.1:18181")]
    host_addr: SocketAddr,

    /// Guest RAM in MiB. Lower it on memory-constrained hosts.
    #[arg(long, default_value_t = 8192)]
    memory: u32,
}

fn main() {
    let cfg: Config = Config::parse();

    for required in [&cfg.kernel, &cfg.initrd] {
        if !required.exists() {
            eprintln!(
                "missing {} -- pass --kernel and --initrd pointing at the firmware files",
                required.display()
            );
            std::process::exit(1);
        }
    }

    if let Err(e) = ensure_disk(&cfg.disk) {
        eprintln!(
            "[launcher] failed to prepare disk image at {}: {e}",
            cfg.disk.display()
        );
        std::process::exit(1);
    }

    // Spawn QEMU; it's orphan-protected (see `spawn_qemu` / the `orphan`
    // module), and a background thread watches it and exits the process if it
    // dies on its own, which causes Tauri's window to close with us.
    let qemu = spawn_qemu(&cfg);
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

    // The /v1/hw address is fixed at launch, so hand it to the UI as a constant
    // injected before page scripts run rather than over a command. The UI reads
    // window.__HW_ADDR__ and dials it, following --host-addr instead of
    // assuming a fixed port.
    let hw_addr = format!("window.__HW_ADDR__ = {:?};", cfg.host_addr.to_string());
    tauri::Builder::default()
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("hw-addr")
                .js_init_script(hw_addr)
                .build(),
        )
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Initial size of the sparse disk image. The firmware partitions and formats
/// it on first boot; the launcher only allocates the raw container.
const DISK_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Lazily creates the backing disk image if missing. Idempotent — to reset
/// device state, delete the file and re-launch.
fn ensure_disk(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    println!(
        "[launcher] disk.img missing, allocating sparse {} GiB blank disk",
        DISK_BYTES >> 30
    );
    // File::set_len on a freshly-created file produces a sparse file on
    // Linux ext4/btrfs/xfs and macOS APFS. On Windows NTFS the bytes are
    // zero-allocated rather than sparse, so the initial file is a real 4 GiB.
    let file = fs::File::create(path)?;
    file.set_len(DISK_BYTES)?;
    Ok(())
}

/// Spawn `qemu-system-aarch64` configured for the emulator: virt machine with
/// paravirt net + disk, host port 18181 forwarded into the guest. Spawned
/// through `orphan::guard` so the child can't outlive the launcher.
fn spawn_qemu(cfg: &Config) -> Child {
    let mut cmd = Command::new("qemu-system-aarch64");
    cmd.args(["-M", "virt", "-cpu", "cortex-a72", "-m"])
        .arg(cfg.memory.to_string())
        .args(["-nographic", "-kernel"])
        .arg(&cfg.kernel)
        .args(["-initrd"])
        .arg(&cfg.initrd)
        // rdinit=/sbin/init hands control to the firmware's init, which brings up
        // networking and the ArkOS services.
        .args(["-append", "console=ttyAMA0 rdinit=/sbin/init", "-netdev"])
        .arg(format!(
            "user,id=net0,hostfwd=tcp:{}-:{GUEST_PORT}",
            cfg.host_addr
        ))
        .args(["-device", "virtio-net-pci,netdev=net0", "-drive"])
        .arg(format!(
            "file={},if=none,id=disk0,format=raw",
            cfg.disk.display()
        ))
        .args([
            "-device",
            "virtio-blk-pci,drive=disk0",
            "-serial",
            "stdio",
            "-monitor",
            "none",
        ]);

    orphan::guard(cmd).spawn().unwrap_or_else(|e| {
        eprintln!("failed to spawn qemu-system-aarch64: {e}");
        std::process::exit(1);
    })
}
