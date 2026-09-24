// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Connects the optional device face to the Rust hardware controller.
//!
//! At most one change notification waits for a snapshot request. A suspended
//! webview cannot build up a queue of LED frames or block the hardware socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{Emitter as _, Manager as _};

use crate::hardware::{ButtonSource, Controller, State};

/// Minimum spacing between presentation updates.
const FRAME_TIME: Duration = Duration::from_millis(33);
/// Tells the face to fetch the latest complete state.
const STATE_EVENT: &str = "device-state";

/// Per-window delivery state, independent of hardware I/O.
pub(crate) struct View {
    /// Source of device state and target of button inputs.
    hardware: Controller,
    /// Whether a notification still awaits a snapshot request.
    pending: Arc<AtomicBool>,
}

/// Attach the device face to the controller without opening another socket.
pub(crate) fn attach(app: &tauri::AppHandle, hardware: Controller) {
    let pending = Arc::new(AtomicBool::new(false));
    app.manage(View {
        hardware: hardware.clone(),
        pending: pending.clone(),
    });
    let app = app.clone();
    std::thread::spawn(move || {
        let mut sent = 0;
        while !crate::runtime::stopping() {
            std::thread::sleep(FRAME_TIME);
            let revision = hardware.snapshot().revision;
            if revision != sent && !pending.swap(true, Ordering::SeqCst) {
                if app.emit_to(crate::MAIN_WINDOW, STATE_EVENT, ()).is_err() {
                    pending.store(false, Ordering::SeqCst);
                } else {
                    sent = revision;
                }
            }
        }
    });
}

/// Acknowledge a notification and return the latest complete state.
#[tauri::command]
pub(crate) fn device_state(view: tauri::State<'_, View>) -> State {
    // Clear first so an update concurrent with this read gets a notification
    view.pending.store(false, Ordering::SeqCst);
    view.hardware.snapshot()
}

/// Release a held button when a view reloads or loses focus.
pub(crate) fn release_button(app: &tauri::AppHandle) {
    let Some(view) = app.try_state::<View>() else {
        return;
    };
    view.hardware.release_button();
}

/// Apply a user button interaction without exposing GPIO or bus envelopes.
#[tauri::command]
pub(crate) fn set_button_pressed(
    view: tauri::State<'_, View>,
    pressed: bool,
    generation: u64,
) -> Result<(), String> {
    view.hardware
        .button(ButtonSource::Ui, pressed, generation)
        .map(|_| ())
}
