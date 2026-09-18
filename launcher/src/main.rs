// ark-emulator: boots the Ark firmware in a virtual machine on this computer
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

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
//!   - `panel`:        the settings panel's state and the commands behind it.
//!   - `platform`:     OS-specific quirks, so the other three stay cfg-free.
//!   - `orphan`:       ties QEMU's lifetime to this process.
//!   - `registry`:     the registry of running emulators, and who serves it.
//!   - `discovery`:    keeping this emulator listed in that registry.
//!   - `diagnostics`:  the log ring and facts a crash report is built from.
//!   - `error_dialog`: turns an error into a window the user can copy out of.
//!
//! Everything that can fail lives under `start`, which runs inside Tauri's
//! setup hook so that a failure has a window to be shown in. It splits in two:
//! `prepare` settles everything that needs nobody's input, and `launch` starts
//! the guest. When `prepare` cannot work out which image to boot, `launch` is
//! deferred to the settings panel's Start button instead of running here, and
//! the window comes up with that panel covering the device face.

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
mod output;
mod panel;
mod platform;
mod qemu;
mod registry;
mod settings;
mod verbs;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Parser;
use tauri::{Manager, WindowEvent};

use bundle::{resolve_firmware, resolve_qemu_libs, Paths};
use diagnostics::log;
use disk::Resolved;
use panel::{Launcher, Pending};
use qemu::{ensure_disk, spawn_qemu, GuestArch, HostPort};
use settings::Settings;

/// The command line as parsed.
#[derive(Parser)]
#[command(
    name = "ark-emulator",
    about = "Ark Emulator: boots the Ark firmware in a virtual machine on this computer"
)]
struct Cli {
    /// Everything a guest needs to be booted.
    #[command(flatten)]
    boot: Boot,

    /// The options that apply whatever is being run.
    #[command(flatten)]
    global: Global,

    /// Emulator, bundled firmware and QEMU versions
    #[arg(short = 'V', long)]
    version: bool,

    /// What to do, or nothing at all, which opens the device window.
    #[command(subcommand)]
    command: Option<verbs::Command>,
}

impl Cli {
    /// Reject the combinations clap cannot express. The boot options belong
    /// to the bare run and to `start`, so naming one beside a command is a
    /// mistake rather than something to guess at.
    fn validate(&self) -> Result<(), output::Error> {
        let message = if self.global.quiet && self.global.verbose {
            "--quiet cannot be combined with --verbose"
        } else if self.version && self.command.is_some() {
            "--version cannot be combined with a command"
        } else if self.command.is_some() && self.boot.named() {
            "the boot options belong to a bare run or to `ark-emulator start`"
        } else {
            return Ok(());
        };
        Err(output::Error::new(2, "usage", message))
    }
}

/// The options every command carries, spelled the way the house tools spell
/// them. They are accepted at any level, so `--no-input` before or after a
/// command name means the same thing.
#[derive(clap::Args)]
pub(crate) struct Global {
    /// Print results as JSON and stderr events as JSON Lines
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Longest wait for the device or the registry, never a person
    #[arg(long, global = true, default_value_t = DEFAULT_TIMEOUT, value_name = "SECONDS", value_parser = parse_timeout)]
    pub(crate) timeout: u64,

    /// Never open a dialog; use the default image, and exit on failure
    ///
    /// A launch with nobody at the keyboard, such as a test run, takes the
    /// default image instead of asking where to keep one, and a failure
    /// prints its report and exits instead of opening a window.
    #[arg(long, global = true)]
    pub(crate) no_input: bool,

    /// Diagnostics: debug for the launcher, trace adds registry traffic
    #[arg(long, global = true, value_name = "LEVEL", value_enum)]
    pub(crate) log: Option<Log>,

    /// Hide progress and notes; keep errors and hints
    #[arg(short = 'q', long, global = true)]
    pub(crate) quiet: bool,

    /// Show steps
    #[arg(short = 'v', long, global = true)]
    pub(crate) verbose: bool,
}

/// How much diagnostic detail a run asks for, independent of step narration.
#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub(crate) enum Log {
    /// The launcher's own lines.
    Debug,
    /// Those and every registry request.
    Trace,
}

/// Longest wait for a machine, in seconds. One number for the whole tool, so
/// there is one to remember, and it is generous enough to cover a boot with no
/// hardware acceleration behind it.
const DEFAULT_TIMEOUT: u64 = 120;

/// Reject a wait that cannot be waited out.
fn parse_timeout(value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(seconds) if seconds > 0 => Ok(seconds),
        _ => Err("the timeout is a positive number of seconds".to_owned()),
    }
}

/// What an emulator boots from, shared by the bare run and by the command that
/// boots one in the background.
#[derive(clap::Args)]
pub(crate) struct Boot {
    /// Image to boot, created if missing [default: the remembered one]
    ///
    /// Read for this run only. It neither consults nor updates the settings
    /// file, so a one-off boot from another image leaves the remembered choice
    /// alone.
    #[arg(long, value_name = "PATH")]
    pub(crate) disk: Option<PathBuf>,

    /// Cloud environment for a new image: release, staging, develop [default: remembered, else release]
    ///
    /// An existing image keeps the environment it was created with, since the
    /// firmware burns that binding in on its first boot.
    #[arg(long, value_name = "ENV", value_parser = settings::ENVS)]
    pub(crate) env: Option<String>,

    /// Guest RAM [default: remembered, else 8192]
    ///
    /// In MiB. Lower it on a machine with little memory to spare.
    #[arg(long, value_name = "MIB")]
    pub(crate) memory: Option<u32>,

    /// Firmware architecture: arm64, amd64 [default: this computer's]
    ///
    /// Only this computer's own architecture gets hardware acceleration, and
    /// it is the only one a packaged build carries firmware and QEMU for.
    #[arg(long, value_name = "ARCH", value_enum)]
    pub(crate) arch: Option<GuestArch>,

    /// Kernel image, with --initrd; a source build has no bundled firmware
    #[arg(long, value_name = "PATH")]
    pub(crate) kernel: Option<PathBuf>,

    /// Initramfs, with --kernel
    #[arg(long, value_name = "PATH")]
    pub(crate) initrd: Option<PathBuf>,

    /// Loopback address forwarded into the guest [default: first free port from 18181]
    ///
    /// The guest's own port is fixed, so this is the host side of the forward
    /// and the number an emulator is known by.
    #[arg(long, value_name = "ADDR")]
    pub(crate) host_addr: Option<SocketAddr>,
}

impl Boot {
    /// Whether any of these was typed.
    fn named(&self) -> bool {
        self.disk.is_some()
            || self.env.is_some()
            || self.memory.is_some()
            || self.arch.is_some()
            || self.kernel.is_some()
            || self.initrd.is_some()
            || self.host_addr.is_some()
    }
}

/// Label of the device face window, hidden until startup succeeds.
pub(crate) const MAIN_WINDOW: &str = "main";

/// How far apart, in logical pixels, consecutive emulators' windows sit.
const STAGGER_STEP: u32 = 32;

/// Set once the user closes the device window, which takes QEMU down with it.
/// The wait thread reads it to tell an expected teardown from a crash, since
/// both arrive as the same dead child process.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Take this emulator down the way closing its window does. Withdraw it from
/// the registry, then go, which lets go of QEMU: the orphan guard kills it
/// with this process.
pub(crate) fn shut_down() -> ! {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    discovery::deregister();
    std::process::exit(0);
}

fn main() {
    let cli = Cli::parse();
    let output = output::Output::new(&cli.global);
    error_dialog::reporting(&output, cli.global.no_input);

    // Compiled in rather than read at run time, so a command line run never
    // opens a window or touches the display to find out where things are.
    let context = tauri::generate_context!();
    if let Err(err) = cli.validate() {
        output.error(&err);
        std::process::exit(err.exit);
    }
    if cli.version || cli.command.is_some() {
        std::process::exit(verbs::run(
            cli.command,
            &cli.global,
            &context.config().identifier,
            context.package_info(),
        ));
    }

    // From here the window is the product. A source build's guest console has
    // stdout, so nothing this layer would put there is written, and the
    // launcher's own lines are the only thing on stderr.
    output.release_stdout();
    if output.json() {
        diagnostics::log_sink(diagnostics::Sink::Events(output));
    }

    // The UI dials the hardware bus at startup, so the port has to be settled
    // before the builder runs. Reserving it can fail, and that failure travels
    // into `start` with every other one.
    let host_port = match cli.boot.host_addr {
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
            disk::pick_disk,
            discovery::nameplate,
            panel::settings_state,
            panel::save_settings,
            panel::start_emulator
        ])
        .setup(move |app| {
            let no_input = cli.global.no_input;
            if let Err(err) = platform::install_menus(app)
                .and_then(|()| start(app, cli.boot, no_input, host_port))
            {
                error_dialog::show(app.handle(), error_dialog::COULD_NOT_START, err);
            }
            // Deliberately Ok even when startup failed. An Err here propagates
            // out of run(), and then there is no event loop left to show the
            // error in and nothing but a panic message nobody can read.
            Ok(())
        })
        .run(context)
        .unwrap_or_else(|err| {
            // The webview runtime itself did not come up, so no window of ours
            // can either. Stderr is all that is left.
            let err = anyhow!(err).context("the window system could not be started");
            eprintln!(
                "{}",
                diagnostics::report(error_dialog::COULD_NOT_START, &err)
            );
            std::process::exit(1);
        });
}

/// Bring up the emulated device: work out what to boot, then either boot it or
/// put the settings panel up asking for what is missing.
///
/// Every fallible step is under here rather than in `main` so that all of them
/// end up on the same reporting path. That includes resolving the guest
/// architecture and reserving a host port, neither of which needs a Tauri app
/// but both of which would otherwise be failures with nowhere to be displayed.
fn start(app: &tauri::App, boot: Boot, no_input: bool, host_port: Result<HostPort>) -> Result<()> {
    let (pending, settings, resolved) = prepare(app, boot, no_input, host_port)?;
    let mut launcher = Launcher::booting(pending, settings);
    let slot = launcher.slot();

    match resolved {
        Resolved::Boot(disk) => {
            let (memory, env) = launcher.effective();
            let pending = launcher.take().expect("nothing has taken it yet");
            app.manage(Mutex::new(launcher));
            if pending.boot.disk.is_some() || no_input {
                ensure_disk(&disk, pending.qemu_libs.as_deref()).with_context(|| {
                    format!("failed to prepare the disk image at {}", disk.display())
                })?;
            } else {
                disk::require_existing(&disk)?;
            }
            launch(app.handle(), pending, &disk, memory, &env)?;
        }
        Resolved::Ask { suggestion, reason } => {
            launcher.ask(suggestion, reason);
            app.manage(Mutex::new(launcher));
            // The panel covers the whole device face, so this still does not
            // put an enclosure on screen that has nothing behind it. QEMU is
            // spawned from `panel::start_emulator` once the user says so.
            reveal(app.handle(), slot)?;
        }
    }
    Ok(())
}

/// Settle everything that needs nobody's input, and work out which image to
/// boot. Answers with the ingredients a start needs, the settings they were
/// read out of, and that decision.
fn prepare(
    app: &tauri::App,
    boot: Boot,
    no_input: bool,
    host_port: Result<HostPort>,
) -> Result<(Pending, Settings, Resolved)> {
    // The host's architecture is also the only one that gets hardware
    // acceleration, so it is the default.
    let arch = match boot.arch {
        Some(arch) => arch,
        None => match std::env::consts::ARCH {
            "aarch64" => GuestArch::Arm64,
            "x86_64" => GuestArch::Amd64,
            other => bail!("no firmware exists for {other} hosts; pass --arch explicitly"),
        },
    };
    diagnostics::record("Guest", arch.name());

    let host_port = host_port?;
    diagnostics::record("Host address", host_port.addr().to_string());

    let paths = Paths::resolve(&app.config().identifier, app.package_info())?;
    let data_dir = paths.data.clone();

    // Best effort, and early, so that everything a failing startup says lands
    // in the file a second process can read.
    if let Err(e) = diagnostics::log_to(&data_dir, host_port.port()) {
        log!("[launcher] could not open a log file: {e:#}");
    }

    let settings = Settings::load(&data_dir)?;
    diagnostics::record_path("Settings", settings.path());

    // Discovery is a convenience, never a precondition: whatever it answers,
    // including nothing at all, this emulator still boots.
    discovery::ensure_registry();
    let booted: disk::Booted = discovery::list()
        .unwrap_or_default()
        .into_iter()
        .map(|instance| (instance.disk_id, instance.port))
        .collect();

    let firmware = resolve_firmware(paths.resources.as_deref(), &boot, arch)?;
    diagnostics::record_path("Kernel", &firmware.kernel);
    diagnostics::record_path("Initrd", &firmware.initrd);

    let qemu_libs = resolve_qemu_libs(paths.resources.as_deref());

    let resolved = disk::decide(
        boot.disk.as_deref(),
        settings.disk(),
        settings.autostart(),
        &booted,
        &data_dir,
        host_port.port(),
        no_input,
    )?;

    let pending = Pending {
        boot,
        arch,
        host_port,
        firmware,
        qemu_libs,
    };
    Ok((pending, settings, resolved))
}

/// Start the guest on `disk`, tie QEMU's lifetime to the window's, and put the
/// device face on screen.
fn launch(
    app: &tauri::AppHandle,
    mut pending: Pending,
    disk: &Path,
    memory: u32,
    env: &str,
) -> Result<()> {
    diagnostics::record_path("Disk", disk);
    let mut child = spawn_qemu(
        pending.arch,
        &pending.firmware,
        disk,
        memory,
        env,
        pending.qemu_libs.as_deref(),
        &mut pending.host_port,
    )?;
    disk::mark_booted(disk);

    // Published only now that the port and image are settled, so the registry
    // never advertises an emulator that turned out not to start.
    discovery::register(pending.host_port.port(), disk);

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
    let handle = app.clone();
    thread::spawn(move || match child.wait() {
        Ok(status) => {
            log!("[launcher] QEMU exited with {status}");
            if status.success() || SHUTTING_DOWN.load(Ordering::SeqCst) {
                handle.exit(status.code().unwrap_or(0));
                return;
            }
            let err =
                anyhow!("the emulated device stopped unexpectedly: QEMU exited with {status}");
            error_dialog::show_from_thread(&handle, error_dialog::STOPPED, err);
        }
        Err(e) => {
            let err = anyhow!(e).context("lost track of the QEMU process");
            error_dialog::show_from_thread(&handle, error_dialog::STOPPED, err);
        }
    });

    // The device face is hidden until there is a device behind it, so a launch
    // that fails shows the error window rather than an enclosure that never
    // lights up.
    reveal(app, pending.host_port.slot())
}

/// Put the window on screen, with the close handler that takes QEMU down with
/// it. Shared by a straight-through boot and by the settings panel's startup
/// form, which is on screen before there is any QEMU to take down.
fn reveal(app: &tauri::AppHandle, slot: u32) -> Result<()> {
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
    stagger(&window, slot);
    window.show().context("could not show the main window")
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
