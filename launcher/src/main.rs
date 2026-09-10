//! Spawns QEMU with the firmware image and hosts a Tauri window for the
//! emulator UI. Window and QEMU are lifecycle-bound, so closing either tears
//! down the other on every platform, even on a hard kill:
//!
//!   ws clients ──TCP──▶ host:1818N ──QEMU SLIRP hostfwd──▶ guest:18181 ──▶ firmware
//!
//!   - QEMU exits, and the wait thread below exits the launcher.
//!   - The launcher exits, and QEMU dies with it via the OS-specific
//!     protection in `orphan`, which survives SIGKILL and force-quit.
//!
//! Several emulators can run at once, each on a host port and a disk image of
//! its own. Finding them is what the registry is for: whichever launcher holds
//! its port serves it, and the rest publish themselves into it.
//!
//! The backing disk image is an unencrypted qcow2 file that starts small and
//! grows on demand. There is no encryption at the qemu layer. The emulator is
//! a dev/test convenience, not a vault; see README.
//!
//!   - `qemu`:         the QEMU command line, the guest it builds, and its disk.
//!   - `bundle`:       where a packaged build's firmware, sidecars and libs live.
//!   - `settings`:     what the launcher remembers between runs.
//!   - `disk`:         which disk image the guest boots from, asking if need be.
//!   - `platform`:     OS-specific quirks, so the other three stay cfg-free.
//!   - `orphan`:       ties QEMU's lifetime to this process.
//!   - `registry`:     the registry of running emulators, and who serves it.
//!   - `discovery`:    keeping this emulator listed in that registry.
//!   - `diagnostics`:  the log ring and facts a crash report is built from.
//!   - `error_dialog`: turns an error into a window the user can copy out of.
//!
//! Everything that can fail lives in `start`, which runs inside Tauri's setup
//! hook so that a failure has a window to be shown in.

// Release builds link as a GUI app on Windows so launching the app doesn't pop
// up a console alongside the UI. Debug builds keep the console subsystem so
// `cargo run`'s diagnostics still print. See `platform` for the rest of the
// Windows console story, including the one QEMU would otherwise get.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bundle;
mod diagnostics;
mod discovery;
mod disk;
mod error_dialog;
mod orphan;
mod platform;
mod qemu;
mod registry;
mod settings;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Parser;
use tauri::{Manager, WindowEvent};

use bundle::{app_data_dir, resolve_firmware, resolve_qemu_libs};
use diagnostics::log;
use qemu::{ensure_disk, spawn_qemu, GuestArch, HostPort};
use settings::Settings;

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

    /// Path to the backing disk image; auto-allocated if it does not exist.
    /// Defaults to the image remembered in the settings file, which the
    /// launcher asks for the first time it needs one.
    #[arg(long)]
    disk: Option<PathBuf>,

    /// Cloud environment the device gets bound to when its disk is first
    /// created; ignored for existing disks (the binding is burnt in).
    #[arg(long, default_value = "release", value_parser = ["develop", "staging", "release"])]
    env: String,

    /// Host address that SLIRP forwards into the guest's port. Defaults to the
    /// first free loopback port from 18181 up, so that several emulators can
    /// run at once without being told about each other.
    #[arg(long)]
    host_addr: Option<SocketAddr>,

    /// Guest RAM in MiB. Lower it on memory-constrained hosts.
    #[arg(long, default_value_t = 8192)]
    memory: u32,
}

/// Label of the device face window, hidden until startup succeeds.
pub(crate) const MAIN_WINDOW: &str = "main";

/// How far apart, in logical pixels, consecutive emulators' windows sit.
const STAGGER_STEP: u32 = 32;

/// Set once the user closes the device window, which takes QEMU down with it.
/// The wait thread reads it to tell an expected teardown from a crash, since
/// both arrive as the same dead child process.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

fn main() {
    let cfg: Config = Config::parse();

    // The UI dials the hardware bus at startup, so the port has to be settled
    // before the builder runs. Reserving it can fail, and that failure travels
    // into `start` with every other one.
    let host_port = match cfg.host_addr {
        Some(addr) => Ok(HostPort::fixed(addr)),
        None => HostPort::reserve(),
    };

    // The /v1/hw address is fixed at launch, so hand it to the UI as a
    // constant injected before page scripts run rather than over a command.
    // The UI reads window.__HW_ADDR__ and dials it, so it follows this
    // emulator's port.
    let hw_addr = host_port
        .as_ref()
        .map(|port| port.addr().to_string())
        .unwrap_or_default();
    let hw_addr = format!("window.__HW_ADDR__ = {hw_addr:?};");
    tauri::Builder::default()
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("hw-addr")
                .js_init_script(hw_addr)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            error_dialog::report_issue,
            disk::disk_path,
            discovery::nameplate
        ])
        .setup(move |app| {
            if let Err(err) = start(app, &cfg, host_port) {
                error_dialog::show(app.handle(), "could not start", err);
            }
            // Deliberately Ok even when startup failed. An Err here propagates
            // out of run(), and then there is no event loop left to show the
            // error in and nothing but a panic message nobody can read.
            Ok(())
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|err| {
            // The webview runtime itself did not come up, so no window of ours
            // can either. Stderr is all that is left.
            let err = anyhow!(err).context("the window system could not be started");
            eprintln!("{}", diagnostics::report("could not start", &err));
            std::process::exit(1);
        })
        .run(|_handle, event| platform::on_run_event(&event));
}

/// Bring up the emulated device: work out what to boot, prepare its disk,
/// start QEMU, bind the two lifetimes together, and only then show the window.
///
/// Every fallible step is here rather than in `main` so that all of them reach
/// the same reporting path. That includes resolving the guest architecture and
/// reserving a host port, neither of which needs a Tauri app but both of which
/// would otherwise be failures with nowhere to be displayed.
fn start(app: &tauri::App, cfg: &Config, host_port: Result<HostPort>) -> Result<()> {
    // The host's architecture is also the only one that gets hardware
    // acceleration, so it is the default.
    let arch = match cfg.arch {
        Some(arch) => arch,
        None => match std::env::consts::ARCH {
            "aarch64" => GuestArch::Arm64,
            "x86_64" => GuestArch::Amd64,
            other => bail!("no firmware exists for {other} hosts; pass --arch explicitly"),
        },
    };
    diagnostics::record("Guest", arch.name());

    let mut host_port = host_port?;
    diagnostics::record("Host address", host_port.addr().to_string());

    let data_dir = app_data_dir(app)?;
    let mut settings = Settings::load(&data_dir)?;
    diagnostics::record_path("Settings", settings.path());

    // Discovery is a convenience, never a precondition: whatever it answers,
    // including nothing at all, this emulator still boots.
    discovery::ensure_registry();
    let booted: disk::Booted = discovery::list()
        .into_iter()
        .map(|instance| (instance.disk_id, instance.port))
        .collect();

    let (kernel, initrd) = resolve_firmware(app, cfg, arch)?;
    diagnostics::record_path("Kernel", &kernel);
    diagnostics::record_path("Initrd", &initrd);

    let qemu_libs = resolve_qemu_libs(app);

    let Some(disk) = disk::resolve(app, cfg, &mut settings, &booted, host_port.port())? else {
        // The picker was dismissed, which is an answer rather than a failure:
        // nothing to report and nothing to boot. Exiting the process directly
        // for the reason `error_dialog` gives, that setup may still be running
        // with no event loop to carry an app.exit. QEMU is spawned below this
        // point, so there is nothing running to tear down.
        std::process::exit(0);
    };
    diagnostics::record_path("Disk", &disk);
    ensure_disk(&disk, qemu_libs.as_deref())
        .with_context(|| format!("failed to prepare the disk image at {}", disk.display()))?;

    let mut child = spawn_qemu(
        cfg,
        arch,
        &kernel,
        &initrd,
        &disk,
        qemu_libs.as_deref(),
        &mut host_port,
    )?;

    // Published only now that the port and image are settled, so the registry
    // never advertises an emulator that turned out not to start.
    discovery::register(host_port.port(), &disk);

    // Piped in `spawn_qemu`, so it has to be drained here or QEMU stalls once
    // the pipe fills. Teeing it into the log ring is what makes "QEMU refused
    // to start" diagnosable from a packaged build, which has no console.
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => log!("[qemu] {line}"),
                    Err(_) => break,
                }
            }
        });
    }

    // A background thread watches QEMU and exits the process if it dies on its
    // own, which closes Tauri's window with us.
    let handle = app.handle().clone();
    thread::spawn(move || match child.wait() {
        Ok(status) => {
            log!("[launcher] QEMU exited with {status}");
            if status.success() || SHUTTING_DOWN.load(Ordering::SeqCst) {
                handle.exit(status.code().unwrap_or(0));
                return;
            }
            let err =
                anyhow!("the emulated device stopped unexpectedly: QEMU exited with {status}");
            error_dialog::show_from_thread(&handle, "stopped unexpectedly", err);
        }
        Err(e) => {
            let err = anyhow!(e).context("lost track of the QEMU process");
            error_dialog::show_from_thread(&handle, "stopped unexpectedly", err);
        }
    });

    // The device face is hidden until there is a device behind it, so a launch
    // that fails shows the error window rather than an enclosure that never
    // lights up.
    let window = app
        .get_webview_window(MAIN_WINDOW)
        .context("the main window is missing from the Tauri configuration")?;
    window.on_window_event(|event| {
        if matches!(event, WindowEvent::CloseRequested { .. }) {
            SHUTTING_DOWN.store(true, Ordering::SeqCst);
            // The entry would expire on its own; this only saves it lingering
            // in somebody's list for those few seconds.
            discovery::deregister();
        }
    });
    stagger(&window, host_port.slot());
    window.show().context("could not show the main window")?;
    Ok(())
}

/// Offset the window by its place in the port range, so emulators started one
/// after another cascade. They are undecorated, fixed-size and always on top,
/// so identical positions would leave only the last one visible.
///
/// Best effort: a window system that will not place a window is not a reason to
/// refuse to boot.
fn stagger(window: &tauri::WebviewWindow, slot: u32) {
    if slot == 0 {
        return;
    }
    let Ok(tauri::PhysicalPosition { x, y }) = window.outer_position() else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let step = (f64::from(STAGGER_STEP) * scale).round() as i32;
    let _ = window.set_position(tauri::PhysicalPosition {
        x: x + step * slot as i32,
        y: y + step * slot as i32,
    });
}
