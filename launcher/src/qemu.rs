//! The QEMU command line: which system emulator to run, how the guest is
//! wired up, and the qcow2 disk it boots from.
//!
//! A native-architecture guest is the fast path. It is the only one that gets
//! hardware acceleration (see [`crate::platform::accel_flags`]) and the only
//! one a packaged build ships a QEMU for. A cross-architecture guest always
//! runs under TCG emulation and always needs a QEMU on `PATH`.
//!
//! The guest is minimal on purpose: virtio net and block, a serial console on
//! stdio, no monitor and no graphics. Host port 18181 forwarded through SLIRP
//! is the entire interface the UI and the dashboard talk to.

use std::error::Error;
use std::path::Path;
use std::process::{Child, Command};

use crate::bundle::resolve_sidecar;
use crate::orphan;
use crate::platform::{
    accel_flags, library_path_var, prepend_library_path, suppress_child_console,
};
use crate::Config;

/// CPU architecture of the firmware being booted, in the same docker-style
/// vocabulary the firmware build names its artifacts with.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum GuestArch {
    #[value(name = "arm64")]
    Arm64,
    #[value(name = "amd64")]
    Amd64,
}

impl GuestArch {
    /// Whether this architecture is the host's own, which decides if QEMU can
    /// use hardware acceleration instead of pure emulation.
    fn host(self) -> bool {
        match self {
            Self::Arm64 => std::env::consts::ARCH == "aarch64",
            Self::Amd64 => std::env::consts::ARCH == "x86_64",
        }
    }

    /// QEMU system emulator that boots this architecture, as installed on
    /// `PATH`. Only ever used for the fallback, since a bundled build ships
    /// its emulator under [`QEMU_SIDECAR`] instead.
    fn qemu_binary(self) -> &'static str {
        match self {
            Self::Arm64 => "qemu-system-aarch64",
            Self::Amd64 => "qemu-system-x86_64",
        }
    }

    /// Serial console device of the guest: the arm virt machine exposes a
    /// PL011 at ttyAMA0, the x86 q35 machine a 16550 at ttyS0.
    fn console(self) -> &'static str {
        match self {
            Self::Arm64 => "ttyAMA0",
            Self::Amd64 => "ttyS0",
        }
    }

    /// Subdirectory under the bundled `firmware` resource holding this
    /// architecture's kernel/initrd, matching the CI layout.
    pub(crate) fn firmware_dir(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::Amd64 => "amd64",
        }
    }
}

/// The port the firmware uses to communicate with the host.
const GUEST_PORT: u16 = 18181;

/// Virtual ceiling of the backing qcow2 disk. The host file starts tiny and
/// grows on demand as the guest writes, never exceeding this size.
const DISK_BYTES: u64 = 127_731_564_544;

/// `externalBin` name the host-native QEMU system emulator is bundled under.
/// Generic because which real `qemu-system-*` binary that is depends on the
/// build host.
const QEMU_SIDECAR: &str = "qemu-system-guest";

/// Lazily creates the backing qcow2 disk image if missing. Idempotent; to
/// reset device state, delete the file and re-launch.
pub(crate) fn ensure_disk(path: &Path, qemu_libs: Option<&Path>) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        return Ok(());
    }
    println!("[launcher] disk image missing, creating qcow2 (grows on demand)");
    // qcow2 is sparse on every host, including Windows NTFS where a raw
    // set_len would zero-fill the whole file. Delegated to qemu-img rather
    // than hand-writing the format. The bare byte count is read as bytes.
    let mut cmd = match resolve_sidecar("qemu-img") {
        Some(bundled) => Command::new(bundled),
        None => Command::new("qemu-img"),
    };
    suppress_child_console(&mut cmd);
    if let Some(libs) = qemu_libs {
        cmd.env(library_path_var(), prepend_library_path(libs));
    }
    let output = cmd
        .args(["create", "-f", "qcow2"])
        .arg(path)
        .arg(DISK_BYTES.to_string())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "qemu-img create failed ({:?}); is qemu-img bundled or installed and on PATH?\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }
    Ok(())
}

/// Spawn the guest arch's QEMU system emulator: paravirt net and disk, host
/// port 18181 forwarded into the guest. Goes through [`orphan::guard`] so the
/// child cannot outlive the launcher.
///
/// Resolves the binary itself rather than using `tauri-plugin-shell`'s
/// sidecar API, which exposes no pre-exec hook, and the Linux orphan
/// protection needs one to arm `PR_SET_PDEATHSIG`.
pub(crate) fn spawn_qemu(
    cfg: &Config,
    arch: GuestArch,
    kernel: &Path,
    initrd: &Path,
    disk: &Path,
    qemu_libs: Option<&Path>,
) -> Child {
    let native = arch.host();
    // Only the host-native architecture is ever bundled, so a cross-arch
    // request always falls through to a PATH-installed QEMU, which a packaged
    // build will not have.
    let mut cmd = match native.then(|| resolve_sidecar(QEMU_SIDECAR)).flatten() {
        Some(bundled) => {
            eprintln!(
                "[launcher] using bundled QEMU sidecar at {}",
                bundled.display()
            );
            Command::new(bundled)
        }
        None => {
            eprintln!(
                "[launcher] no bundled QEMU sidecar found, falling back to {} on PATH",
                arch.qemu_binary()
            );
            Command::new(arch.qemu_binary())
        }
    };
    suppress_child_console(&mut cmd);
    if let Some(libs) = qemu_libs {
        eprintln!("[launcher] passing -L {} to QEMU", libs.display());
        cmd.env(library_path_var(), prepend_library_path(libs));
        // -L points QEMU at its firmware/BIOS/keymap datadir, e.g.
        // bios-256k.bin, which the q35 machine model needs even for a direct
        // -kernel boot since SeaBIOS still runs first. QEMU looks up only the
        // filenames it needs there and ignores the rest, so sharing the
        // directory with the bundled libraries is harmless. The arm64 virt
        // board needs no firmware at all, making this a no-op on that path.
        cmd.args(["-L"]).arg(libs);
    }
    // A native guest runs -cpu max: the host CPU under KVM/HVF, the maximal
    // emulated one under the TCG fallback. Named foreign models are rejected
    // by KVM/HVF and -cpu host by TCG, so max is the only value valid across
    // the whole accel fallback list. A cross-arch arm guest keeps cortex-a72
    // for fidelity with the real device's SoC.
    match arch {
        GuestArch::Arm64 if native => cmd.args(["-M", "virt", "-cpu", "max"]),
        GuestArch::Arm64 => cmd.args(["-M", "virt", "-cpu", "cortex-a72"]),
        GuestArch::Amd64 => cmd.args(["-M", "q35", "-cpu", "max"]),
    };
    cmd.args(accel_flags(native));
    cmd.arg("-m")
        .arg(cfg.memory.to_string())
        .args(["-nographic", "-kernel"])
        .arg(kernel)
        .args(["-initrd"])
        .arg(initrd)
        // rdinit=/sbin/init hands control to the firmware's init, which brings
        // up networking and the ArkOS services. arkos_env seeds the
        // environment binding the firmware burns into its OTP analog on first
        // boot.
        .args(["-append"])
        .arg(format!(
            "console={} rdinit=/sbin/init arkos_env={}",
            arch.console(),
            cfg.env
        ))
        .args(["-netdev"])
        .arg(format!(
            "user,id=net0,hostfwd=tcp:{}-:{GUEST_PORT}",
            cfg.host_addr
        ))
        .args(["-device", "virtio-net-pci,netdev=net0", "-drive"])
        .arg(format!(
            "file={},if=none,id=disk0,format=qcow2,discard=unmap,detect-zeroes=unmap",
            disk.display()
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
