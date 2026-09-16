// launcher: Ark device emulator
// Copyright 2026 Dark Bio AG. All rights reserved.

//! An explicit new-instance action in the macOS app menu.

use std::process::{Command, Stdio};

use anyhow::{bail, Context as _, Result};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};

use crate::diagnostics::log;

const NEW_WINDOW: &str = "new-window";

/// Add an action to the app menu for starting another emulator.
pub(super) fn install(app: &tauri::App) -> Result<()> {
    let menu = Menu::default(app.handle())?;
    let items = menu.items()?;
    let app_menu = items
        .first()
        .and_then(|item| item.as_submenu())
        .context("the macOS app menu is missing")?;
    let new = MenuItem::with_id(app, NEW_WINDOW, "New Window", true, Some("Cmd+N"))?;
    app_menu.prepend_items(&[&new, &PredefinedMenuItem::separator(app)?])?;
    app.set_menu(menu)?;
    app.on_menu_event(|_, event| {
        if event.id().as_ref() == NEW_WINDOW {
            std::thread::spawn(|| {
                if let Err(err) = spawn_new_instance() {
                    log!("[launcher] could not start another emulator: {err:#}");
                    rfd::MessageDialog::new()
                        .set_title("Could not start another emulator")
                        .set_description(format!("{err:#}"))
                        .set_level(rfd::MessageLevel::Error)
                        .show();
                }
            });
        }
    });
    Ok(())
}

fn spawn_new_instance() -> Result<()> {
    let exe = std::env::current_exe()?;
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .context("not running from an app bundle")?;
    let status = Command::new("open")
        .args(["-n", "-a"])
        .arg(bundle)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not launch another emulator")?;
    if !status.success() {
        bail!("macOS could not launch another emulator ({status})");
    }
    Ok(())
}
