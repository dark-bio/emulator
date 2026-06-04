// launcher: spawns QEMU with the firmware image and hosts a Tauri window
// for the emulator UI. Window and QEMU are lifecycle-bound — closing either
// tears down the other; on Linux, PR_SET_PDEATHSIG ensures QEMU can't
// outlive the launcher even on SIGKILL.
//
//   ws clients ──TCP──▶ host:8080 ──QEMU SLIRP hostfwd──▶ guest:8080 ──▶ firmware
//
// The backing disk image is plain raw — no encryption at the qemu layer.
// The emulator is a dev/test convenience, not a vault; see README.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;

use clap::Parser;

/// Command-line arguments. The disk is deliberately separate from `--firmware`
/// — it's user state, not a build artifact, and rebuilding the firmware
/// shouldn't wipe it. Defaults resolve against the cwd at launch time.
#[derive(Parser)]
#[command(about = "Ark device emulator: boots ArkOS in QEMU behind a small UI.")]
struct Args {
    /// Firmware build directory containing vmlinuz + initramfs.gz.
    #[arg(long, default_value = "firmware/build")]
    firmware: PathBuf,

    /// Path to the backing disk image; auto-allocated on first run.
    #[arg(long, default_value = "disk.img")]
    disk: PathBuf,

    /// Host address that SLIRP forwards into the guest's :8080.
    #[arg(long, default_value = "127.0.0.1:8080")]
    host_addr: String,
}

/// Launch-time configuration, derived from the parsed `Args`.
struct Config {
    kernel: PathBuf,
    initrd: PathBuf,
    disk: PathBuf,
    host_addr: String,
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        Self {
            kernel: args.firmware.join("vmlinuz"),
            initrd: args.firmware.join("initramfs.gz"),
            disk: args.disk,
            host_addr: args.host_addr,
        }
    }
}

fn main() {
    let cfg: Config = Args::parse().into();

    for required in [&cfg.kernel, &cfg.initrd] {
        if !required.exists() {
            eprintln!(
                "missing {} -- pass --firmware pointing at a directory containing \
                 vmlinuz + initramfs.gz (produced by the firmware repo's \
                 `./arkos.sh emulator-build`)",
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

    println!(
        "[launcher] firmware from {}, hardware bus at ws://{}/hw once guest boots",
        cfg.kernel.parent().unwrap().display(),
        cfg.host_addr,
    );

    // Spawn QEMU; a background thread watches it and exits the process if it
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

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Initial size of the sparse disk image. The firmware's first-boot
/// `initdisk` service partitions and formats it; the launcher only allocates
/// the raw container.
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
/// paravirt net + disk, host port 8080 forwarded into the guest, PDEATHSIG
/// attached on Linux.
fn spawn_qemu(cfg: &Config) -> Child {
    let mut cmd = Command::new("qemu-system-aarch64");
    cmd.args([
        "-M",
        "virt",
        "-cpu",
        "cortex-a72",
        "-m",
        "8192",
        "-nographic",
        "-kernel",
    ])
    .arg(&cfg.kernel)
    .args(["-initrd"])
    .arg(&cfg.initrd)
    // rdinit=/sbin/init hands control to openrc, which brings up networking
    // and starts the arkos-core supervisor.
    .args(["-append", "console=ttyAMA0 rdinit=/sbin/init", "-netdev"])
    .arg(format!("user,id=net0,hostfwd=tcp:{}-:8080", cfg.host_addr))
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
    ])
    .stdin(Stdio::null());

    protect_from_orphan(&mut cmd);

    cmd.spawn().unwrap_or_else(|e| {
        eprintln!("failed to spawn qemu-system-aarch64: {e}");
        std::process::exit(1);
    })
}

/// Attach `PR_SET_PDEATHSIG=SIGKILL` to the QEMU child. Ensures the kernel
/// kills QEMU if the launcher dies for any reason — including SIGKILL or
/// panic, neither of which run Rust's `Drop`. macOS has no prctl equivalent;
/// Windows wants a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Both
/// deferred — on those hosts QEMU may outlive an abnormally-terminated
/// launcher.
#[cfg(target_os = "linux")]
fn protect_from_orphan(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs in the forked child before exec; only the
    // async-signal-safe prctl is called.
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            if rc == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn protect_from_orphan(_cmd: &mut Command) {}
