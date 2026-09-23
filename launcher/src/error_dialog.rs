// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Telling the user the emulator is not going to work.
//!
//! This is a GUI-first app that people are meant to download and double-click,
//! and in exactly that mode there is no terminal to print to. So a fatal error
//! opens a real window: the failure in plain words, the full chain and the
//! launcher's recent log in selectable text, and a button that puts all of it
//! on the clipboard for the user to send us.
//!
//! Deliberately not an OS message box. GTK, NSAlert and Win32 `MessageBox`
//! disagree about whether their text can even be selected, and Windows' cannot,
//! which defeats the point. A second webview window renders the same everywhere
//! and can carry a copy button.
//!
//! The report always reaches the log and stderr first, so a developer at a
//! terminal, CI, and the command that started this emulator all still see it
//! when no window can be shown at all.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::MAIN_WINDOW;
use crate::diagnostics::{self, log};
use crate::error::{Code, Error};
use crate::output::Output;

/// Label of the error window this module creates.
const ERROR_WINDOW: &str = "error";

/// Where the "Report issue" button sends the user.
const ISSUES_URL: &str = "https://github.com/dark-bio/emulator/issues";

/// Fixed size of the error window, in logical pixels. Wide enough that neither
/// a long path nor a QEMU log line wraps, and tall enough that a typical report
/// is readable without scrolling at all.
const WIDTH: f64 = 1000.0;
const HEIGHT: f64 = 750.0;

/// What a failure before the guest is up is reported under. Dying during
/// startup and dying an hour in read very differently to somebody who had a
/// working emulator a moment ago.
pub(crate) const COULD_NOT_START: &str = "could not start";

/// What a failure after the guest is up is reported under.
pub(crate) const STOPPED: &str = "stopped unexpectedly";

/// Whether a failure exits after printing its report instead of opening a
/// window. Also what stands for "nobody is here to ask" wherever else the
/// launcher would put a dialog in front of somebody (see [`crate::disk`]).
static NO_INPUT: AtomicBool = AtomicBool::new(false);

/// The output layer a failure goes through when this run answers in JSON.
/// Absent for a run that reports in plain words.
static EVENTS: Mutex<Option<Output>> = Mutex::new(None);

/// Record how this run reports a failure. The reporting paths below are
/// reached from threads that have no command line in hand, so they read it
/// from here.
pub(crate) fn reporting(output: &Output, no_input: bool) {
    NO_INPUT.store(no_input, Ordering::SeqCst);
    if output.json()
        && let Ok(mut events) = EVENTS.lock()
    {
        *events = Some(output.clone());
    }
}

/// Whether a failure has already been reported. Startup failing and QEMU dying
/// are not mutually exclusive, and the second one to arrive must not stack a
/// window on top of the first.
static REPORTED: AtomicBool = AtomicBool::new(false);

/// Open the issue tracker in the user's browser, for the error window's
/// "Report issue" button.
///
/// Takes no argument on purpose. The page could just as well pass the address,
/// but then the launcher would be offering to open anything a page asked it
/// to, and this way the only address it can ever open is the one above.
/// Failure is logged rather than surfaced: the dialog it would be reported in
/// is the one already on screen.
#[tauri::command]
pub(crate) fn report_issue() {
    if let Err(e) = crate::platform::open_url(ISSUES_URL) {
        log!("[launcher] could not open {ISSUES_URL}: {e}");
    }
}

/// Report a fatal error from the main thread, which is where Tauri's `setup`
/// hook runs. `title` is [`COULD_NOT_START`] or [`STOPPED`].
pub(crate) fn show(app: &AppHandle, title: &str, err: anyhow::Error) {
    let Some(report) = prepare(title, err) else {
        return;
    };
    open_or_exit(app, &report);
}

/// Report a fatal error from a background thread. Window creation has to happen
/// on the main thread, so the work is handed to the event loop; only the stderr
/// half runs here.
pub(crate) fn show_from_thread(app: &AppHandle, title: &str, err: anyhow::Error) {
    let Some(report) = prepare(title, err) else {
        return;
    };
    let app = app.clone();
    let handle = app.clone();
    if handle
        .run_on_main_thread(move || open_or_exit(&app, &report))
        .is_err()
    {
        // The event loop is gone, so no window is possible and nothing will
        // exit the process for us.
        std::process::exit(1);
    }
}

/// Write the report to stderr and decide whether a window should follow.
/// Returns the report to show, or `None` if this failure is not getting a
/// window: either something already reported one, or dialogs are switched off,
/// in which case the process is exiting instead.
fn prepare(title: &str, err: anyhow::Error) -> Option<String> {
    // Logged before the report is built, so the cause is in the log file a
    // command that started this emulator reads back, and in the report too.
    log!("[launcher] {title}: {err:#}");
    let report = diagnostics::report(title, &err);
    match EVENTS.lock().ok().and_then(|events| events.clone()) {
        Some(output) => output.error(&Error::new(code(title), report.clone())),
        None => eprintln!("{report}"),
    }

    if REPORTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    if NO_INPUT.load(Ordering::SeqCst) {
        // Not app.exit: there is no UI state worth unwinding here, and the
        // caller may still be inside `setup` with no event loop yet to carry
        // the request. QEMU dies with us either way, via `orphan`.
        std::process::exit(1);
    }
    Some(report)
}

/// The stable code for a failure, taken from the words it is reported under.
fn code(title: &str) -> Code {
    match title {
        STOPPED => Code::StoppedUnexpectedly,
        _ => Code::CouldNotStart,
    }
}

/// Build the error window, falling back to exiting if even that fails.
fn open_or_exit(app: &AppHandle, report: &str) {
    if let Err(e) = open(app, report) {
        eprintln!("could not open the error window: {e}");
        std::process::exit(1);
    }
}

fn open(app: &AppHandle, report: &str) -> tauri::Result<()> {
    // Same trick the hw-addr plugin uses for __HW_ADDR__: Rust's Debug for str
    // is a valid JS string literal, so the report crosses into the page as a
    // constant with no IPC and no capability to grant. An init script is not
    // inline HTML, so a `</script>` inside a QEMU error message is inert.
    // __WIDTH__ lets the page solve its enclosure geometry for whatever size
    // this window was given, so WIDTH and HEIGHT above stay the only place a
    // size is written down.
    let script = format!("window.__REPORT__ = {report:?}; window.__WIDTH__ = {WIDTH};");

    // Built before the device face is closed. Tauri exits once the last window
    // goes, so closing that one first would take the error window with it.
    // Transparent and undecorated, like the device face, so the page can paint
    // the enclosure itself with nothing square behind its rounded corners.
    // That leaves no OS close button, so the page carries its own Close button
    // and a drag region to move the window by.
    //
    // Fixed size, so the page lays itself out once and only the report scrolls.
    let window = WebviewWindowBuilder::new(app, ERROR_WINDOW, WebviewUrl::App("error.html".into()))
        .title("Ark Emulator")
        .inner_size(WIDTH, HEIGHT)
        .resizable(false)
        .zoom_hotkeys_enabled(false)
        .devtools(cfg!(debug_assertions))
        .maximizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .center()
        .initialization_script(script)
        .build()?;

    let handle = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::CloseRequested { .. }) {
            handle.exit(1);
        }
    });

    if let Some(main) = app.get_webview_window(MAIN_WINDOW) {
        let _ = main.close();
    }
    Ok(())
}
