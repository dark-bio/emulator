// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Runs the desktop interface and connects its windows to the guest runtime.

mod disk_picker;
mod error_dialog;
mod face;
#[cfg(target_os = "macos")]
mod macos_menu;
mod panel;
mod webview;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use tauri::{Manager as _, WindowEvent};

use crate::bundle::Paths;
use crate::cli::args::Boot;
use crate::cli::output::Output;
use crate::error::Error;
use crate::platform;
use crate::runtime::disk::{self, Resolved};
use crate::runtime::qemu::ensure_disk;
use crate::runtime::{self, Pending, Runtime};
use panel::Launcher;

pub(crate) use error_dialog::reporting;

/// Label of the device face window, hidden until startup succeeds.
const MAIN_WINDOW: &str = "main";
/// How far apart, in logical pixels, consecutive emulators' windows sit.
const STAGGER_STEP: u32 = 32;

/// Run the window event loop and report startup or window system failures.
pub(crate) fn run(
    context: tauri::Context<tauri::Wry>,
    boot: Boot,
    no_input: bool,
    output: &Output,
) {
    tauri::Builder::default()
        .plugin(webview::plugin())
        .on_page_load(|webview, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                face::release_button(webview.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            error_dialog::report_issue,
            disk_picker::disk_path,
            disk_picker::pick_disk,
            face::device_state,
            face::set_button_pressed,
            panel::settings_state,
            panel::save_settings,
            panel::start_emulator
        ])
        .setup(move |app| {
            if let Err(err) = install_menus(app).and_then(|()| start(app, boot, no_input)) {
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

/// Install the platform's native actions for starting another emulator.
#[cfg(target_os = "macos")]
fn install_menus(app: &tauri::App) -> Result<()> {
    macos_menu::install(app)
}

/// Leave menu setup to Tauri on platforms without custom native actions.
#[cfg(not(target_os = "macos"))]
fn install_menus(_app: &tauri::App) -> Result<()> {
    Ok(())
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
