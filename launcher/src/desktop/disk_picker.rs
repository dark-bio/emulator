// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Opens image dialogs and supplies image paths to the desktop interface.
//!
//! The settings panel opens the picker when the user presses a button.
//! Open selects an existing image. New uses a save dialog and creates the
//! image immediately, replacing existing contents only after the native
//! dialog confirms that choice. Starting from the panel never creates an image.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use super::panel::Launcher;
use crate::runtime::{disk, qemu};

/// Return the backing image's full path for the device face's info tray.
/// `None` means the guest has not started yet.
#[tauri::command]
pub(super) fn disk_path() -> Option<String> {
    disk::booted().map(|disk| disk.display().to_string())
}

/// An image the user chose in the picker, as the settings panel needs it.
#[derive(Serialize)]
pub(super) struct Picked {
    /// Full path, which is what goes back to the launcher on start.
    path: String,
    /// File name, which is what the panel shows.
    name: String,
}

/// Select an existing image, or create one immediately through a save dialog.
/// `None` means the user dismissed the dialog.
#[tauri::command]
pub(super) async fn pick_disk(
    app: tauri::AppHandle,
    launcher: tauri::State<'_, Mutex<Launcher>>,
    current: String,
    create: bool,
) -> Result<Option<Picked>, String> {
    let qemu_libs = launcher.lock().unwrap().qemu_libs().map(Path::to_path_buf);
    let picked = choose_disk(&app, Path::new(&current), create).await?;
    let Some(path) = picked else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || {
        let path = disk::settle(&path).map_err(|e| format!("That location cannot be used: {e}"))?;
        if create {
            disk::require_available(&path).map_err(|e| format!("{e:#}"))?;
            qemu::create_disk(&path, qemu_libs.as_deref())
                .map_err(|e| format!("Could not create {}: {e:#}", disk::name_of(&path)))?;
        } else {
            disk::require_existing(&path).map_err(|e| format!("{e:#}"))?;
        }
        Ok(Some(Picked {
            name: disk::name_of(&path),
            path: path.display().to_string(),
        }))
    })
    .await
    .map_err(|e| format!("Could not prepare the image: {e}"))?
}

/// Run native dialogs on the main thread, as required by macOS, while the
/// command awaits their result without blocking the event loop.
async fn choose_disk(
    app: &tauri::AppHandle,
    current: &Path,
    create: bool,
) -> Result<Option<PathBuf>, String> {
    let dir = current.parent().unwrap_or(Path::new("")).to_path_buf();
    let name = disk::name_of(current);
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        let mut dialog = rfd::FileDialog::new().add_filter("Ark emulator", &["ark"]);
        if !dir.as_os_str().is_empty() {
            dialog = dialog.set_directory(dir);
        }
        if !name.is_empty() {
            dialog = dialog.set_file_name(name);
        }
        let picked = if create {
            dialog
                .set_file_name("emulator.ark")
                .set_title("Create a new emulator")
                .save_file()
        } else {
            dialog
                .add_filter("All files", &["*"])
                .set_title("Open an existing emulator")
                .pick_file()
        };
        let _ = tx.send(picked);
    })
    .map_err(|e| format!("Could not open the file picker: {e}"))?;
    tauri::async_runtime::spawn_blocking(move || rx.recv())
        .await
        .map_err(|e| format!("Could not read the file picker result: {e}"))?
        .map_err(|e| format!("The file picker closed unexpectedly: {e}"))
}
