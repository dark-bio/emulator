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
//
// A packaged build bundles QEMU and firmware for the host's own architecture
// only (see tauri.conf.json's `bundle.externalBin`/`bundle.resources`), so
// end users need neither on their machine. Cross-arch guests and local
// development fall back to --kernel/--initrd/QEMU-on-PATH, where no bundle
// exists or the bundled arch doesn't match what was asked for.

mod orphan;

use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;

use clap::Parser;
use tauri::{path::BaseDirectory, Manager};
use tauri_plugin_shell::ShellExt;

/// The port the firmware uses to communicate with the host.
const GUEST_PORT: u16 = 18181;

/// Launch configuration parsed from command-line arguments.
#[derive(Parser)]
#[command(about = "Ark device emulator: boots ArkOS in QEMU behind a small UI.")]
struct Config {
    /// Path to the kernel image (vmlinuz). Defaults to the firmware bundled
    /// with this build for --arch; pass this to override it, e.g. for local
    /// development or testing a custom firmware build. Must be given
    /// together with --initrd.
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
    /// to a `disk.img` under this app's data directory (see
    /// `resolve_disk_path`), not the current directory: a packaged app's
    /// working directory isn't a reliable place to write to (e.g. it's
    /// inside a read-only mount for an AppImage).
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

    // Resolve the guest architecture; it decides which QEMU system emulator
    // to spawn, how to configure it, and which bundled firmware to boot.
    // Unless overridden via --arch, assume firmware built for the host's
    // architecture, which is also the variant that gets hardware
    // acceleration.
    let arch = cfg.arch.unwrap_or_else(|| match std::env::consts::ARCH {
        "aarch64" => GuestArch::Arm64,
        "x86_64" => GuestArch::Amd64,
        other => {
            eprintln!("no firmware exists for {other} hosts; pass --arch explicitly");
            std::process::exit(1);
        }
    });

    // The /v1/hw address is fixed at launch, so hand it to the UI as a constant
    // injected before page scripts run rather than over a command. The UI reads
    // window.__HW_ADDR__ and dials it, following --host-addr instead of
    // assuming a fixed port.
    let hw_addr = format!("window.__HW_ADDR__ = {:?};", cfg.host_addr.to_string());
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("hw-addr")
                .js_init_script(hw_addr)
                .build(),
        )
        .setup(move |app| {
            let (kernel, initrd) = resolve_firmware(app, &cfg, arch)?;
            let qemu_libs = resolve_qemu_libs(app);
            let disk = resolve_disk_path(app, &cfg)?;

            if let Err(e) = ensure_disk(app, &disk, qemu_libs.as_deref()) {
                eprintln!(
                    "[launcher] failed to prepare disk image at {}: {e}",
                    disk.display()
                );
                std::process::exit(1);
            }

            // Spawn QEMU; it's orphan-protected (see `spawn_qemu` / the
            // `orphan` module), and a background thread watches it and exits
            // the process if it dies on its own, which causes Tauri's window
            // to close with us.
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

/// CPU architecture of the firmware being booted, in the same docker-style
/// vocabulary the firmware build names its artifacts with. Selects the QEMU
/// binary, machine model, serial console, and bundled firmware subdirectory
/// to boot with.
#[derive(Clone, Copy, clap::ValueEnum)]
enum GuestArch {
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
    /// `PATH`. Only the host-native architecture's build is ever bundled
    /// (see `QEMU_SIDECAR`), so this name matters only for the PATH-lookup
    /// fallback: a system-installed QEMU (dev builds) or an explicit
    /// cross-arch `--arch` request against a packaged build, which has no
    /// bundled sidecar for the non-native architecture.
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
    fn firmware_dir(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::Amd64 => "amd64",
        }
    }
}

/// Resolve the kernel/initrd paths to boot. Explicit --kernel/--initrd flags
/// take priority (and are the only option in a local dev build, since
/// `cargo run` has no bundled resource directory); otherwise falls back to
/// the firmware bundled for `arch` under Tauri's resource directory.
fn resolve_firmware(
    app: &tauri::App,
    cfg: &Config,
    arch: GuestArch,
) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    match (&cfg.kernel, &cfg.initrd) {
        (Some(kernel), Some(initrd)) => return Ok((kernel.clone(), initrd.clone())),
        (None, None) => {}
        _ => return Err("--kernel and --initrd must be passed together".into()),
    }

    let dir = arch.firmware_dir();
    let kernel = app
        .path()
        .resolve(format!("firmware/{dir}/kernel"), BaseDirectory::Resource)?;
    let initrd = app
        .path()
        .resolve(format!("firmware/{dir}/initrd.gz"), BaseDirectory::Resource)?;
    for (label, path) in [("kernel", &kernel), ("initrd", &initrd)] {
        if !path.exists() {
            return Err(format!(
                "no bundled {label} for {dir}: pass --kernel and --initrd explicitly \
                 (a development build has no bundled firmware)"
            )
            .into());
        }
    }
    Ok((kernel, initrd))
}

/// Resolve the backing disk image's path. An explicit --disk is absolutized
/// against the current directory (a deliberate user override, presumably
/// typed at a shell with a real, intended working directory); otherwise
/// defaults to `disk.img` under this app's own data directory, creating it
/// if missing. This is never the bare current directory: a packaged app's
/// working directory isn't reliably writable, or even meaningful. A Tauri
/// AppImage's own AppRun, for instance, changes it to somewhere inside the
/// read-only FUSE-mounted image before the launcher ever runs.
fn resolve_disk_path(app: &tauri::App, cfg: &Config) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(disk) = &cfg.disk {
        return Ok(std::path::absolute(disk)?);
    }
    let dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("disk.img"))
}

/// `externalBin`/sidecar name the host-native QEMU system emulator is
/// bundled under (see `.github/scripts/fetch-qemu-*`). Only the guest
/// architecture matching the build host's own is ever bundled, to keep
/// installer size down. The name is generic because which real
/// `qemu-system-*` binary that is depends on the build host.
const QEMU_SIDECAR: &str = "qemu-system-guest";

/// Resolve a bundled sidecar binary next to the launcher's own executable,
/// where `cargo tauri build` copies `externalBin` entries at package time.
/// Returns `None` outside a bundled build (e.g. local `cargo run`), where
/// callers fall back to a bare `PATH` lookup, today's behavior.
fn resolve_sidecar(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    // Some `cargo` layouts (e.g. `cargo test`) put the executable under a
    // `deps/` subdirectory; sidecars are copied next to the top-level target
    // dir, not into `deps/`.
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir.parent()?.to_path_buf();
    }
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = dir.join(file);
    path.exists().then_some(path)
}

/// Resolve the bundled directory of QEMU's shared library dependencies and
/// firmware/BIOS datadir files (populated by
/// `.github/scripts/fetch-qemu-{linux,macos,windows}`), if present. `None`
/// in a local dev build with no bundled libs, where a system-installed QEMU
/// already has its dependencies satisfied normally.
///
/// Logs its resolution outcome unconditionally: a missing or empty
/// directory here surfaces later as an opaque QEMU firmware/library error
/// with no indication of where to look, so this is the one place that can
/// actually say why.
fn resolve_qemu_libs(app: &tauri::App) -> Option<PathBuf> {
    let dir = match app.path().resolve("qemu-libs", BaseDirectory::Resource) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("[launcher] could not resolve qemu-libs resource directory: {e}");
            return None;
        }
    };
    if !dir.exists() {
        eprintln!(
            "[launcher] resolved qemu-libs directory {} does not exist; \
             QEMU will fall back to its own default search paths",
            dir.display()
        );
        return None;
    }
    let count = std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(0);
    eprintln!(
        "[launcher] using bundled qemu-libs at {} ({count} files)",
        dir.display()
    );
    Some(dir)
}

/// Name of the platform's dynamic-linker library search-path environment
/// variable.
fn library_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        // Windows has no LD_LIBRARY_PATH equivalent; the default DLL search
        // order does include each directory on PATH, though, so prepending
        // there has the same effect.
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// `dir` prepended onto the current value of `library_path_var`, for
/// pointing a spawned QEMU sidecar at its bundled shared library
/// dependencies without patching the binaries themselves (see the
/// `fetch-qemu-*` scripts for why that's unnecessary: the dynamic
/// linker/loader consults this search path before a dependency's recorded
/// path, even an absolute one).
fn prepend_library_path(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os(library_path_var());
    let existing = existing.iter().flat_map(std::env::split_paths);
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(existing))
        .unwrap_or_else(|_| dir.as_os_str().to_owned())
}

/// Virtual ceiling of the backing qcow2 disk. The host file starts tiny and
/// grows on demand as the guest writes, never exceeding this size.
const DISK_BYTES: u64 = 127_731_564_544;

/// Lazily creates the backing qcow2 disk image if missing. Idempotent; to reset
/// device state, delete the file and re-launch.
fn ensure_disk(
    app: &tauri::App,
    path: &Path,
    qemu_libs: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        return Ok(());
    }
    println!("[launcher] disk image missing, creating qcow2 (grows on demand)");
    // qcow2 is sparse on every host (including Windows NTFS, where a raw
    // set_len would zero-fill the whole file): it starts at a few hundred KB and
    // grows as the guest writes. Delegated to qemu-img, which ships with QEMU,
    // rather than hand-writing the format. The bare byte count is read as bytes.
    //
    // This is a one-shot call (unlike the long-lived qemu-system-* process),
    // so it goes through the real tauri-plugin-shell sidecar API rather than
    // `resolve_sidecar`; the plugin resolves the bundled binary in a packaged
    // build and falls back to `qemu-img` on PATH otherwise.
    let mut sidecar = app
        .shell()
        .sidecar("qemu-img")?
        .args(["create", "-f", "qcow2"])
        .arg(path.display().to_string())
        .arg(DISK_BYTES.to_string());
    if let Some(libs) = qemu_libs {
        sidecar = sidecar.env(library_path_var(), prepend_library_path(libs));
    }
    let output = tauri::async_runtime::block_on(sidecar.output())?;
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

/// Spawn the guest arch's QEMU system emulator configured for the emulator:
/// paravirt net + disk, host port 18181 forwarded into the guest. Spawned
/// through `orphan::guard` so the child can't outlive the launcher.
///
/// Uses `resolve_sidecar` rather than `tauri-plugin-shell`'s sidecar API:
/// that plugin doesn't expose a pre-exec hook, which the Linux orphan
/// protection (`PR_SET_PDEATHSIG`, armed between fork and exec) needs.
fn spawn_qemu(
    cfg: &Config,
    arch: GuestArch,
    kernel: &Path,
    initrd: &Path,
    disk: &Path,
    qemu_libs: Option<&Path>,
) -> Child {
    let native = arch.host();
    // The bundled sidecar is only ever the host-native architecture's QEMU
    // (see QEMU_SIDECAR), so it's only tried for a native-arch guest; an
    // explicit cross-arch --arch request always falls through to a
    // PATH-installed QEMU, which a packaged build won't have.
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
    if let Some(libs) = qemu_libs {
        eprintln!("[launcher] passing -L {} to QEMU", libs.display());
        cmd.env(library_path_var(), prepend_library_path(libs));
        // -L points QEMU at its firmware/BIOS/keymap datadir (e.g.
        // bios-256k.bin, which the q35 machine model needs even for a
        // direct -kernel boot, since SeaBIOS still runs first). Bundled
        // alongside the .so dependencies in the same directory; QEMU only
        // looks up specific filenames it needs there and ignores the rest.
        // The arm64 virt board needs no firmware for a direct kernel boot,
        // so this is a no-op on that path, but harmless to pass regardless.
        cmd.args(["-L"]).arg(libs);
    }
    // A native guest runs -cpu max (the host CPU under KVM/HVF, the maximal
    // emulated one under the TCG fallback; named foreign models are rejected
    // by KVM/HVF and -cpu host by TCG, max is the only value valid across the
    // accel fallback list). A cross-arch arm guest keeps cortex-a72 for
    // fidelity with the real device's SoC.
    match arch {
        GuestArch::Arm64 if native => cmd.args(["-M", "virt", "-cpu", "max"]),
        GuestArch::Arm64 => cmd.args(["-M", "virt", "-cpu", "cortex-a72"]),
        GuestArch::Amd64 => cmd.args(["-M", "q35", "-cpu", "max"]),
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
        .arg(kernel)
        .args(["-initrd"])
        .arg(initrd)
        // rdinit=/sbin/init hands control to the firmware's init, which brings up
        // networking and the ArkOS services. arkos_env seeds the environment
        // binding that the firmware burns into its OTP analog on first boot.
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
