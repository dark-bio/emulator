// launcher: spawns QEMU with the firmware image and hosts a Tauri window
// for the emulator UI. Window and QEMU are lifecycle-bound; closing either
// tears down the other, on every platform and even on a hard kill:
//   - QEMU exits  -> the wait thread exits the launcher (all platforms).
//   - launcher exits -> QEMU dies with it via the OS-specific protection in
//     the `orphan` module, which survives SIGKILL/force-quit.
//
//   ws clients ──TCP──▶ host:18181 ──QEMU SLIRP hostfwd──▶ guest:18181 ──▶ firmware
//
// The backing disk image is an unencrypted qcow2 file that starts small and
// grows on demand; there is no encryption at the qemu layer. The emulator is a
// dev/test convenience, not a vault; see README.

mod orphan;

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

    /// CPU architecture of the firmware artifacts; defaults to the host's
    /// architecture.
    #[arg(long, value_enum)]
    arch: Option<GuestArch>,

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

    // Resolve the guest architecture; it decides which QEMU system emulator
    // to spawn and how to configure it. Unless overridden via --arch, assume
    // firmware built for the host's architecture, which is also the variant
    // that gets hardware acceleration.
    let arch = cfg.arch.unwrap_or_else(|| match std::env::consts::ARCH {
        "aarch64" => GuestArch::Aarch64,
        "x86_64" => GuestArch::X86_64,
        other => {
            eprintln!("no firmware exists for {other} hosts -- pass --arch explicitly");
            std::process::exit(1);
        }
    });

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
    let qemu = spawn_qemu(&cfg, arch);
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

/// CPU architecture of the firmware being booted. Selects the QEMU binary,
/// machine model and serial console to boot with.
#[derive(Clone, Copy, clap::ValueEnum)]
enum GuestArch {
    #[value(name = "aarch64")]
    Aarch64,
    #[value(name = "x86_64")]
    X86_64,
}

impl GuestArch {
    /// Canonical name of the architecture, matching both the artifact name
    /// suffix the firmware build emits and `std::env::consts::ARCH`.
    fn name(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }

    /// QEMU system emulator that boots this architecture.
    fn qemu_binary(self) -> &'static str {
        match self {
            Self::Aarch64 => "qemu-system-aarch64",
            Self::X86_64 => "qemu-system-x86_64",
        }
    }

    /// Serial console device of the guest: the arm virt machine exposes a
    /// PL011 at ttyAMA0, the x86 q35 machine a 16550 at ttyS0.
    fn console(self) -> &'static str {
        match self {
            Self::Aarch64 => "ttyAMA0",
            Self::X86_64 => "ttyS0",
        }
    }
}

/// Virtual ceiling of the backing qcow2 disk. The host file starts tiny and
/// grows on demand as the guest writes, never exceeding this size.
const DISK_BYTES: u64 = 127_731_564_544;

/// Lazily creates the backing qcow2 disk image if missing. Idempotent; to reset
/// device state, delete the file and re-launch.
fn ensure_disk(path: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    println!("[launcher] disk image missing, creating qcow2 (grows on demand)");
    // qcow2 is sparse on every host (including Windows NTFS, where a raw
    // set_len would zero-fill the whole file): it starts at a few hundred KB and
    // grows as the guest writes. Delegated to qemu-img, which ships with QEMU,
    // rather than hand-writing the format. The bare byte count is read as bytes.
    let status = Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(path)
        .arg(DISK_BYTES.to_string())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "qemu-img create failed ({status}); is qemu-img installed and on PATH?"
        )));
    }
    Ok(())
}

/// Spawn the guest arch's QEMU system emulator configured for the emulator:
/// paravirt net + disk, host port 18181 forwarded into the guest. Spawned
/// through `orphan::guard` so the child can't outlive the launcher.
fn spawn_qemu(cfg: &Config, arch: GuestArch) -> Child {
    let native = arch.name() == std::env::consts::ARCH;
    let mut cmd = Command::new(arch.qemu_binary());
    // A native guest runs -cpu max (the host CPU under KVM/HVF, the maximal
    // emulated one under the TCG fallback; named foreign models are rejected
    // by KVM/HVF and -cpu host by TCG, max is the only value valid across the
    // accel fallback list). A cross-arch arm guest keeps cortex-a72 for
    // fidelity with the real device's SoC.
    match arch {
        GuestArch::Aarch64 if native => cmd.args(["-M", "virt", "-cpu", "max"]),
        GuestArch::Aarch64 => cmd.args(["-M", "virt", "-cpu", "cortex-a72"]),
        GuestArch::X86_64 => cmd.args(["-M", "q35", "-cpu", "max"]),
    };
    // Repeated -accel flags form an ordered fallback list: hardware
    // virtualization when the host grants it (e.g. /dev/kvm access), plain
    // emulation otherwise. Cross-arch guests can only ever run TCG, which is
    // also QEMU's default, so they get no flag.
    if native {
        if cfg!(target_os = "linux") {
            cmd.args(["-accel", "kvm", "-accel", "tcg"]);
        } else if cfg!(target_os = "macos") {
            cmd.args(["-accel", "hvf", "-accel", "tcg"]);
        }
    }
    cmd.arg("-m")
        .arg(cfg.memory.to_string())
        .args(["-nographic", "-kernel"])
        .arg(&cfg.kernel)
        .args(["-initrd"])
        .arg(&cfg.initrd)
        // rdinit=/sbin/init hands control to the firmware's init, which brings up
        // networking and the ArkOS services.
        .args(["-append"])
        .arg(format!("console={} rdinit=/sbin/init", arch.console()))
        .args(["-netdev"])
        .arg(format!(
            "user,id=net0,hostfwd=tcp:{}-:{GUEST_PORT}",
            cfg.host_addr
        ))
        .args(["-device", "virtio-net-pci,netdev=net0", "-drive"])
        .arg(format!(
            "file={},if=none,id=disk0,format=qcow2,discard=unmap,detect-zeroes=unmap",
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
        eprintln!("failed to spawn {}: {e}", arch.qemu_binary());
        std::process::exit(1);
    })
}
