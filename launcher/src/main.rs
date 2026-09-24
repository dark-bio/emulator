// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Runs an emulated Ark with an optional Tauri device face.
//!
//! `runtime` owns the guest lifecycle and `hardware` owns its hardware socket.
//! `face` adapts hardware state and user inputs for the webview. Both launch
//! modes use the same disk selection, QEMU, registry and shutdown paths.
//! Management commands in `verbs` return before either mode starts.
//!
//! The backing image is an unencrypted qcow2 file. The emulator is for
//! development and demos; real data belongs on hardware.

// A packaged Windows app borrows a console only for command line use
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod args;
mod bundle;
mod control;
mod diagnostics;
mod discovery;
mod disk;
mod doctor;
mod error;
mod error_dialog;
mod face;
mod hardware;
mod help;
mod orphan;
mod output;
mod panel;
mod platform;
mod qemu;
mod registry;
mod runtime;
mod settings;
mod style;
mod update;
mod verbs;
mod webview;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use clap::{FromArgMatches as _, Parser};
use tauri::{Manager as _, WindowEvent};

use args::{Boot, Cli};
use bundle::Paths;
use disk::Resolved;
use error::{Code, Error};
use panel::Launcher;
use qemu::ensure_disk;
use runtime::{Pending, Runtime};

/// Label of the device face window, hidden until startup succeeds.
pub(crate) const MAIN_WINDOW: &str = "main";
/// How far apart, in logical pixels, consecutive emulators' windows sit.
const STAGGER_STEP: u32 = 32;

/// Render clap's complaint in the command line's error format.
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

/// Dispatch management commands before creating a runtime or a window.
fn main() {
    // Handle the detached copy before parsing or initializing command or window state
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() == 2 && arguments[1] == update::ENTRY_POINT {
        update::run();
        return;
    }

    // Parse through the help tree before attaching a console for command output
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
    if cli.version || cli.command.is_some() || cli.boot.headless {
        platform::attach_console();
    }
    let output = output::Output::new(&cli.global);
    error_dialog::reporting(&output, cli.global.no_input || cli.boot.headless);
    diagnostics::level(cli.global.log);

    // This reads compiled metadata without initializing a window system
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

    // A source build gives stdout to the guest console in either launch mode
    output.release_stdout();
    if output.json() {
        diagnostics::log_sink(diagnostics::Sink::Events(output.clone()));
    }
    if cli.boot.headless {
        let result = Paths::resolve(&context.config().identifier, context.package_info())
            .and_then(|paths| headless(&paths, cli.boot, output.clone()));
        if let Err(err) = result {
            diagnostics::log!("[launcher] could not start: {err:#}");
            output.error(&Error::io(format!("{err:#}")));
            runtime::shut_down(1);
        }
        return;
    }

    tauri::Builder::default()
        .plugin(webview::plugin())
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                face::release_button(webview.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            error_dialog::report_issue,
            disk::disk_path,
            disk::pick_disk,
            face::device_state,
            face::set_button_pressed,
            panel::settings_state,
            panel::save_settings,
            panel::start_emulator
        ])
        .setup(move |app| {
            if let Err(err) = platform::install_menus(app)
                .and_then(|()| start(app, cli.boot, cli.global.no_input))
            {
                error_dialog::show(app.handle(), error_dialog::COULD_NOT_START, err);
            }
            // Keep the event loop alive to display startup failures
            Ok(())
        })
        .run(context)
        .unwrap_or_else(|err| {
            output.error(&Error::io(format!(
                "{:#}",
                anyhow!(err).context("the window system could not be started")
            )));
            runtime::shut_down(1);
        });
}

/// Run in the foreground until QEMU exits or shutdown is requested.
fn headless(paths: &Paths, boot: Boot, output: output::Output) -> Result<()> {
    let runtime = Runtime::new()?;
    let (pending, settings, resolved) = runtime::prepare(paths, boot, true)?;
    let Resolved::Boot(disk) = resolved else {
        unreachable!("headless disk selection never asks for input")
    };
    let (memory, env) = runtime::effective(Some(&pending.boot), &settings);
    ensure_disk(&disk, pending.qemu_libs.as_deref())?;
    runtime.launch(pending, &disk, memory, &env, move |result| match result {
        Ok(status) => {
            runtime::shut_down(platform::interrupted(status).or(status.code()).unwrap_or(0))
        }
        Err(err) => {
            output.error(&Error::new(Code::StoppedUnexpectedly, format!("{err:#}")));
            runtime::shut_down(1);
        }
    })?;
    loop {
        std::thread::park();
    }
}

/// Prepare the guest, then boot it or show the settings panel for missing input.
fn start(app: &tauri::App, boot: Boot, no_input: bool) -> Result<()> {
    let runtime = Runtime::new()?;
    face::attach(app.handle(), runtime.hardware.clone());
    app.manage(runtime);
    let paths = Paths::resolve(&app.config().identifier, app.package_info())?;
    let (pending, settings, resolved) = runtime::prepare(&paths, boot, no_input)?;
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
            reveal(app.handle(), slot)?;
        }
    }
    Ok(())
}

/// Start a guest for the graphical adapter and show its face.
fn launch(
    app: &tauri::AppHandle,
    pending: Pending,
    disk: &Path,
    memory: u32,
    env: &str,
) -> Result<()> {
    let slot = pending.host_port.slot();
    let handle = app.clone();
    app.state::<Runtime>()
        .launch(pending, disk, memory, env, move |result| match result {
            Ok(status) => {
                runtime::shut_down(platform::interrupted(status).or(status.code()).unwrap_or(0))
            }
            Err(err) => error_dialog::show_from_thread(&handle, error_dialog::STOPPED, err),
        })?;
    reveal(app, slot)
}

/// Show the window, sharing the same shutdown path as signals and registry stop.
fn reveal(app: &tauri::AppHandle, slot: u32) -> Result<()> {
    let window = app
        .get_webview_window(MAIN_WINDOW)
        .context("the main window is missing from the Tauri configuration")?;
    let handle = app.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::CloseRequested { .. } => runtime::shut_down(0),
        WindowEvent::Focused(false) => face::release_button(&handle),
        _ => {}
    });
    stagger(&window, slot);
    window.show().context("could not show the main window")
}

/// Offset consecutive windows so each device face stays visible.
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
