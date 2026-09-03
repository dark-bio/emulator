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
//!   - `qemu`:         the QEMU command line, the guest it builds, and its disk.
//!   - `bundle`:       where a packaged build's firmware, sidecars and libs live.
//!   - `settings`:     what the launcher remembers between runs.
//!   - `disk`:         which disk image the guest boots from, asking if need be.
//!   - `platform`:     OS-specific quirks, so the other three stay cfg-free.
//!   - `orphan`:       ties QEMU's lifetime to this process.
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
mod disk;
mod error_dialog;
mod orphan;
mod platform;
mod qemu;
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
use qemu::{ensure_disk, spawn_qemu, GuestArch};
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

    /// Host address that SLIRP forwards into the guest's port.
    #[arg(long, default_value = "127.0.0.1:18181")]
    host_addr: SocketAddr,

    /// Guest RAM in MiB. Lower it on memory-constrained hosts.
    #[arg(long, default_value_t = 8192)]
    memory: u32,
}

/// Label of the device face window, hidden until startup succeeds.
pub(crate) const MAIN_WINDOW: &str = "main";

/// Set once the user closes the device window, which takes QEMU down with it.
/// The wait thread reads it to tell an expected teardown from a crash, since
/// both arrive as the same dead child process.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

fn main() {
    let cfg: Config = Config::parse();

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
        .invoke_handler(tauri::generate_handler![
            error_dialog::report_issue,
            disk::disk_path
        ])
        .setup(move |app| {
            if let Err(err) = start(app, &cfg) {
                error_dialog::show(app.handle(), "could not start", err);
            }
            // Deliberately Ok even when startup failed. An Err here propagates
            // out of run(), and then there is no event loop left to show the
            // error in and nothing but a panic message nobody can read.
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap_or_else(|err| {
            // The webview runtime itself did not come up, so no window of ours
            // can either. Stderr is all that is left.
            let err = anyhow!(err).context("the window system could not be started");
            eprintln!("{}", diagnostics::report("could not start", &err));
            std::process::exit(1);
        });
}

/// Bring up the emulated device: work out what to boot, prepare its disk,
/// start QEMU, bind the two lifetimes together, and only then show the window.
///
/// Every fallible step is here rather than in `main` so that all of them reach
/// the same reporting path. That includes resolving the guest architecture,
/// which needs no Tauri app but would otherwise be the one failure with nowhere
/// to be displayed.
fn start(app: &tauri::App, cfg: &Config) -> Result<()> {
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

    let mut settings = Settings::load(&app_data_dir(app)?)?;
    diagnostics::record_path("Settings", settings.path());

    let (kernel, initrd) = resolve_firmware(app, cfg, arch)?;
    diagnostics::record_path("Kernel", &kernel);
    diagnostics::record_path("Initrd", &initrd);

    let qemu_libs = resolve_qemu_libs(app);

    let Some(disk) = disk::resolve(app, cfg, &mut settings)? else {
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

    let mut child = spawn_qemu(cfg, arch, &kernel, &initrd, &disk, qemu_libs.as_deref())?;

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
        }
    });
    window.show().context("could not show the main window")?;
    Ok(())
}
