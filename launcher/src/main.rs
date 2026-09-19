// ark-emulator: emulated Ark enclave for development and demos
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
//! its port serves it, and the rest publish themselves into it. The same
//! executable is also a command line that manages emulators without a window
//! of its own; `args` is that surface and `verbs` is what it does.
//!
//! The backing disk image is an unencrypted qcow2 file that starts small and
//! grows on demand. There is no encryption at the qemu layer. The emulator is
//! a dev/test convenience, not a vault; see README.
//!
//!   - `args`:         the command line as typed.
//!   - `verbs`:        what each command does with an emulator.
//!   - `doctor`:       the checklist that says what to fix on this computer.
//!   - `help`:         the manual, which is what an agent plans from.
//!   - `output`:       what a command prints, for reading or as JSON.
//!   - `style`:        what a terminal adds to that, and the escaping.
//!   - `error`:        the codes a command can exit with.
//!   - `qemu`:         the QEMU command line, the guest it builds, and its disk.
//!   - `bundle`:       where a packaged build's firmware, sidecars and libs live.
//!   - `settings`:     what the launcher remembers between runs.
//!   - `disk`:         which disk image the guest boots from, asking if need be.
//!   - `panel`:        the settings panel's state and the commands behind it.
//!   - `platform`:     OS-specific quirks, so the other modules stay cfg-free.
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

mod args;
mod bundle;
mod diagnostics;
mod discovery;
mod disk;
mod doctor;
mod error;
mod error_dialog;
mod help;
mod orphan;
mod output;
mod panel;
mod platform;
mod qemu;
mod registry;
mod settings;
mod style;
mod verbs;

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{FromArgMatches as _, Parser};
use tauri::{Manager, WindowEvent};

use args::{Boot, Cli};
use bundle::{Paths, resolve_firmware, resolve_qemu_libs};
use diagnostics::log;
use disk::Resolved;
use error::{Code, Error};
use panel::{Launcher, Pending};
use qemu::{GuestArch, HostPort, ensure_disk, spawn_qemu};
use settings::Settings;

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

/// clap's complaint as the house error: its first paragraph without the
/// prefix clap puts on it, under the usage code.
fn usage(error: &clap::Error) -> Error {
    let message = error.to_string();
    let message = message
        .split("\n\n")
        .next()
        .unwrap_or(&message)
        .trim_start_matches("error: ")
        .trim();
    Error::new(Code::Usage, message).hint("`ark-emulator help` lists the commands and the topics")
}

fn main() {
    // Parsed through the help tree, so that -h and --help after a command
    // print the page `help <command>` prints, and a mistake typed at the
    // command line comes back in the house error shape, JSON included. The
    // console is attached before anything is printed, since a Windows release
    // build has none of its own.
    let arguments: Vec<_> = std::env::args_os().collect();
    let json = arguments
        .iter()
        .skip(1)
        .take_while(|argument| *argument != "--")
        .any(|argument| argument == "--json");
    let matches = match help::parser().try_get_matches_from_mut(&arguments) {
        Ok(matches) => matches,
        Err(error) => {
            platform::attach_console();
            if error.exit_code() == 0 {
                let _ = error.print();
                std::process::exit(0);
            }
            let mut global = Cli::parse_from(["ark-emulator"]).global;
            global.json = json;
            output::Output::new(&global).error(&usage(&error));
            std::process::exit(2);
        }
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| {
        platform::attach_console();
        let _ = error.print();
        std::process::exit(2);
    });
    if cli.version || cli.command.is_some() {
        platform::attach_console();
    }
    let output = output::Output::new(&cli.global);
    error_dialog::reporting(&output, cli.global.no_input);
    diagnostics::level(cli.global.log);

    // Compiled in rather than read at run time, so a command line run never
    // opens a window or touches the display to find out where things are.
    let context = tauri::generate_context!();
    if let Err(err) = cli.validate() {
        output.error(&err);
        std::process::exit(err.exit());
    }
    if cli.version || cli.command.is_some() {
        std::process::exit(verbs::run(
            cli.command,
            &cli.global,
            &output,
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
    let host_port = match cli.boot.port {
        Some(port) => Ok(HostPort::fixed(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            port,
        )))),
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
            if pending.boot.image.is_some() || no_input {
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
    let arch = match boot.arch.or_else(GuestArch::native) {
        Some(arch) => arch,
        None => bail!(
            "no firmware exists for {} hosts; pass --arch explicitly",
            std::env::consts::ARCH
        ),
    };
    diagnostics::record("Guest", arch.name());

    let host_port = host_port?;
    diagnostics::record("Host address", host_port.addr().to_string());

    // The window writes here, so the directory is made now rather than by
    // every command that only reads.
    let paths = Paths::resolve(&app.config().identifier, app.package_info())?;
    let data_dir = paths.data.clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("could not create the data directory {}", data_dir.display()))?;

    // Best effort, and early, so that everything a failing startup says lands
    // in the file a second process can read.
    if let Err(e) = diagnostics::log_to(&data_dir, host_port.port()) {
        log!("[launcher] could not open a log file: {e:#}");
    }

    let settings = Settings::load(&data_dir)?;
    diagnostics::record_path("Settings", settings.path());

    // Discovery is a convenience, never a precondition: whatever it answers,
    // including nothing at all, this emulator still boots.
    registry::host();
    let booted = discovery::list().unwrap_or_default();

    let firmware = resolve_firmware(paths.resources.as_deref(), &boot, arch)?;
    diagnostics::record_path("Kernel", &firmware.kernel);
    diagnostics::record_path("Initrd", &firmware.initrd);

    let qemu_libs = resolve_qemu_libs(paths.resources.as_deref());

    let resolved = disk::decide(
        boot.image.as_deref(),
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
