//! The QEMU command line: which system emulator to run, how the guest is
//! wired up, and the qcow2 disk it boots from.
//!
//! A native-architecture guest is the fast path. It is the only one that gets
//! hardware acceleration (see [`crate::platform::accel_flags`]) and the only
//! one a packaged build ships a QEMU for. A cross-architecture guest always
//! runs under TCG emulation and always needs a QEMU on `PATH`.
//!
//! The guest is minimal on purpose: virtio net and block, a serial console on
//! stdio, no monitor and no graphics. One host port forwarded through SLIRP is
//! the entire interface the UI and any host-side client talk to.
//!
//! The guest side of that forward is fixed: the firmware listens on one port
//! and has no way to be told otherwise. The host side is not, which is what
//! lets several emulators run at once, each holding a port of its own out of
//! the range starting at [`FIRST_HOST_PORT`].

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{bail, Context as _, Result};

use crate::bundle::resolve_sidecar;
use crate::diagnostics::{self, log};
use crate::orphan;
use crate::platform::{
    accel_flags, library_path_var, prepend_library_path, suppress_child_console,
};

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

    /// Name of this architecture in the docker-style vocabulary the firmware
    /// build uses, which is also the value `--arch` takes.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::Amd64 => "amd64",
        }
    }

    /// Subdirectory under the bundled `firmware` resource holding this
    /// architecture's kernel/initrd, matching the CI layout.
    pub(crate) fn firmware_dir(self) -> &'static str {
        self.name()
    }
}

/// The port the firmware uses to communicate with the host. Fixed inside the
/// guest, so it is only ever the far end of the forward.
const GUEST_PORT: u16 = 18181;

/// First host port an emulator takes when left to pick one. Chosen to match
/// the guest port, so a single emulator forwards 18181 to 18181.
const FIRST_HOST_PORT: u16 = 18181;

/// How many ports past [`FIRST_HOST_PORT`] to try before giving up. Far more
/// emulators than a machine could run at once, so exhausting it means something
/// else is holding the range.
const HOST_PORT_RANGE: u16 = 100;

/// Virtual ceiling of the backing qcow2 disk. The host file starts tiny and
/// grows on demand as the guest writes, never exceeding this size.
const DISK_BYTES: u64 = 127_731_564_544;

/// `externalBin` name the host-native QEMU system emulator is bundled under.
/// Generic because which real `qemu-system-*` binary that is depends on the
/// build host.
const QEMU_SIDECAR: &str = "qemu-system-guest";

/// Where QEMU looks for the accelerators, block drivers and UI backends that
/// some distributions build as `dlopen`'d modules rather than linking in. The
/// bundling scripts drop them in alongside the shared libraries, so this
/// points at the same directory. Harmless on a build that has no modules, and
/// on a source build there is nothing bundled to point at.
const QEMU_MODULE_DIR: &str = "QEMU_MODULE_DIR";

/// A host port reserved for an emulator, held until the moment QEMU takes it
/// over. Keeping the listener bound is what stops two launchers starting at
/// once from picking the same port: whichever one is second sees it taken.
pub(crate) struct HostPort {
    addr: SocketAddr,
    listener: Option<TcpListener>,
}

impl HostPort {
    /// Take `addr` exactly as asked for, without checking it is free. Used for
    /// an explicit `--host-addr`, where QEMU reports a collision perfectly well
    /// by itself.
    pub(crate) fn fixed(addr: SocketAddr) -> Self {
        Self {
            addr,
            listener: None,
        }
    }

    /// Reserve the first free loopback port at or above [`FIRST_HOST_PORT`].
    pub(crate) fn reserve() -> Result<Self> {
        for offset in 0..HOST_PORT_RANGE {
            let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, FIRST_HOST_PORT + offset);
            if let Ok(listener) = TcpListener::bind(addr) {
                return Ok(Self {
                    addr: addr.into(),
                    listener: Some(listener),
                });
            }
        }
        bail!(
            "no free port between {FIRST_HOST_PORT} and {}; pass --host-addr to choose one",
            FIRST_HOST_PORT + HOST_PORT_RANGE - 1
        )
    }

    /// The address SLIRP will forward from.
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The port on its own, which is how an emulator is identified.
    pub(crate) fn port(&self) -> u16 {
        self.addr.port()
    }

    /// How far this port is into the range, which is distinct between
    /// emulators running at once and is therefore what staggers their windows.
    /// Zero for a port outside the range, including an explicit one.
    pub(crate) fn slot(&self) -> u32 {
        u32::from(self.addr.port().saturating_sub(FIRST_HOST_PORT)).min(u32::from(HOST_PORT_RANGE))
    }

    /// Give the port up so QEMU can bind it. Something else could take it in
    /// the moment before QEMU does, which QEMU reports as a startup failure.
    fn release(&mut self) {
        self.listener = None;
    }
}

/// Lazily creates the backing qcow2 disk image if missing. Idempotent; to
/// reset device state, delete the file and re-launch.
pub(crate) fn ensure_disk(path: &Path, qemu_libs: Option<&Path>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    log!("[launcher] disk image missing, creating qcow2 (grows on demand)");
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
        cmd.env(QEMU_MODULE_DIR, libs);
    }
    let output = cmd
        .args(["create", "-f", "qcow2"])
        .arg(path)
        .arg(DISK_BYTES.to_string())
        .output()
        .context("could not run qemu-img; is it bundled or installed and on PATH?")?;
    if !output.status.success() {
        // qemu-img says what it could not do and why on stderr; its stdout is
        // just the format line, which belongs in the log rather than in front
        // of the user.
        log!(
            "[qemu-img] {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        match stderr.trim() {
            "" => bail!("qemu-img create failed ({})", output.status),
            reason => bail!("qemu-img create failed ({}): {reason}", output.status),
        }
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_qemu(
    arch: GuestArch,
    kernel: &Path,
    initrd: &Path,
    disk: &Path,
    memory: u32,
    env: &str,
    qemu_libs: Option<&Path>,
    host_port: &mut HostPort,
) -> Result<Child> {
    let native = arch.host();
    // Only the host-native architecture is ever bundled, so a cross-arch
    // request always falls through to a PATH-installed QEMU, which a packaged
    // build will not have.
    let mut cmd = match native.then(|| resolve_sidecar(QEMU_SIDECAR)).flatten() {
        Some(bundled) => {
            log!(
                "[launcher] using bundled QEMU sidecar at {}",
                bundled.display()
            );
            diagnostics::record("QEMU", format!("{} (bundled)", bundled.display()));
            Command::new(bundled)
        }
        None => {
            log!(
                "[launcher] no bundled QEMU sidecar found, falling back to {} on PATH",
                arch.qemu_binary()
            );
            diagnostics::record("QEMU", format!("{} (on PATH)", arch.qemu_binary()));
            Command::new(arch.qemu_binary())
        }
    };
    suppress_child_console(&mut cmd);
    if let Some(libs) = qemu_libs {
        log!("[launcher] passing -L {} to QEMU", libs.display());
        cmd.env(library_path_var(), prepend_library_path(libs));
        cmd.env(QEMU_MODULE_DIR, libs);
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
        .arg(memory.to_string())
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
            env
        ))
        .args(["-netdev"])
        .arg(format!(
            "user,id=net0,hostfwd=tcp:{}-:{GUEST_PORT}",
            host_port.addr()
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

    // Captured rather than inherited so a packaged build, which has no console
    // to print to, can still put QEMU's own complaint in a crash report. The
    // caller must drain it or QEMU blocks once the pipe fills. Only stderr is
    // taken: `-serial stdio` above is the guest console and needs stdout.
    cmd.stderr(Stdio::piped());

    // From here it is QEMU that owns the port.
    host_port.release();
    orphan::guard(cmd)
        .spawn()
        .with_context(|| format!("could not start {}", arch.qemu_binary()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_reservation_skips_a_port_that_is_taken() {
        let Ok(first) = HostPort::reserve() else {
            // The whole range is busy, which says nothing about the code.
            return;
        };
        let second = HostPort::reserve().unwrap();
        assert_ne!(first.port(), second.port());
        assert!(second.port() > first.port());
    }

    #[test]
    fn test_a_released_port_can_be_bound() {
        let Ok(mut reserved) = HostPort::reserve() else {
            return;
        };
        let addr = reserved.addr();
        assert!(TcpListener::bind(addr).is_err());
        reserved.release();
        assert!(TcpListener::bind(addr).is_ok());
    }

    #[test]
    fn test_a_fixed_address_is_taken_as_given() {
        // Not probed and not reserved: an explicit --host-addr is the user's
        // call, including a port nothing could bind.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let fixed = HostPort::fixed(addr);
        assert_eq!(fixed.addr(), addr);
        assert_eq!(fixed.port(), 1);
    }
}
