// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The settings panel: what it is shown, and what it is allowed to do.
//!
//! The panel is the same tray that hangs off the device's bottom edge, grown to
//! cover the whole face. It has two jobs, and which one it is doing comes down
//! to whether the guest has been started yet:
//!
//!   - Before the start, it is the startup form. Autostart is disabled or the
//!     launcher needs an image, so the panel says why, offers one, and waits
//!     for [`start_emulator`]. Nothing else in the app is reachable until then.
//!   - After the start, it is reached from the gear in the info tray and edits
//!     what the *next* launch will use. A running guest is never disturbed:
//!     restarting QEMU underneath a device the dashboard may already be talking
//!     to would be a second, much larger feature.
//!
//! [`Launcher`] is the app-wide state both go through. It holds the guest-start
//! ingredients until they are used, and the settings for as long as the app
//! runs, since the gear panel keeps rewriting them.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

use crate::bundle::Firmware;
use crate::disk::{self, Reason};
use crate::qemu::{GuestArch, HostPort};
use crate::settings::{Settings, DEFAULT_ENV, DEFAULT_MEMORY, ENVS, MIN_MEMORY};
use crate::Boot;

/// Everything the guest needs to be started, held until it is. Taken by the
/// start, so a second press of the button has nothing to work with and no
/// second QEMU can be spawned.
pub(crate) struct Pending {
    pub(crate) boot: Boot,
    pub(crate) arch: GuestArch,
    pub(crate) host_port: HostPort,
    pub(crate) firmware: Firmware,
    pub(crate) qemu_libs: Option<PathBuf>,
}

/// What the panel reads and writes, behind one lock.
pub(crate) struct Launcher {
    pending: Option<Pending>,
    settings: Settings,
    qemu_libs: Option<PathBuf>,
    /// The image the startup form offers and the reason it is being asked for.
    /// `None` once the guest has been started, which is what puts the panel in
    /// its other mode.
    ask: Option<(Option<PathBuf>, Reason)>,
}

impl Launcher {
    /// Hold `pending` and `settings` for the panel, with no question to ask.
    pub(crate) fn booting(pending: Pending, settings: Settings) -> Self {
        Self {
            qemu_libs: pending.qemu_libs.clone(),
            pending: Some(pending),
            settings,
            ask: None,
        }
    }

    /// Read the panel's values, refreshing saved preferences after startup.
    fn state(&mut self) -> Result<State, String> {
        if self.pending.is_none() {
            self.settings.reload().map_err(|e| format!("{e:#}"))?;
        }
        let (memory, env) = self.effective();

        // In the startup form the disk is the one being offered. Afterwards the
        // panel edits what the next launch will use, which is the remembered image
        // rather than whatever this run happens to be booted from.
        let (mode, note, path) = match &self.ask {
            Some((suggestion, reason)) => ("startup", reason.message(), suggestion.clone()),
            None => (
                "running",
                NEXT_BOOT.to_owned(),
                self.settings.disk().map(std::path::Path::to_path_buf),
            ),
        };

        Ok(State {
            mode,
            note,
            name: path.as_deref().map(disk::name_of).unwrap_or_default(),
            path: path
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            autostart: self.settings.autostart(),
            memory,
            env,
            envs: ENVS,
            min_memory: MIN_MEMORY,
        })
    }

    /// Libraries needed to create images, including after the guest starts.
    pub(crate) fn qemu_libs(&self) -> Option<&std::path::Path> {
        self.qemu_libs.as_deref()
    }

    /// Put the startup form up, offering `suggestion` and saying why.
    pub(crate) fn ask(&mut self, suggestion: Option<PathBuf>, reason: Reason) {
        self.ask = Some((suggestion, reason));
    }

    /// Give up the guest-start ingredients, leaving the panel in its
    /// next-launch mode.
    pub(crate) fn take(&mut self) -> Option<Pending> {
        self.ask = None;
        self.pending.take()
    }

    /// How far this emulator's port is into the range, which is what staggers
    /// its window against the ones already open.
    pub(crate) fn slot(&self) -> u32 {
        self.pending
            .as_ref()
            .map_or(0, |pending| pending.host_port.slot())
    }

    /// The guest RAM and environment to launch with when nobody is asked, and
    /// the values the panel's form starts on when somebody is.
    ///
    /// A flag beats the settings file, and the panel beats the flag: what it is
    /// showing is a choice the user is looking at, so pressing start or save
    /// settles it for the launch and writes it down. A flag therefore seeds the
    /// form rather than locking it, and only survives untouched on a launch
    /// that never puts the form up.
    pub(crate) fn effective(&self) -> (u32, String) {
        let boot = self.pending.as_ref().map(|pending| &pending.boot);
        let memory = boot
            .and_then(|boot| boot.memory)
            .or_else(|| self.settings.memory())
            .unwrap_or(DEFAULT_MEMORY);
        let env = boot
            .and_then(|boot| boot.env.clone())
            .or_else(|| self.settings.env().map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_ENV.to_owned());
        (memory, env)
    }
}

/// What the user is told once the guest is running. Nothing written here
/// touches the device already up: every one of these values is read at a start,
/// and this one has already happened.
const NEXT_BOOT: &str = "These settings take effect the next time an emulator is started.";

/// The panel's whole view of the world, answered in one call.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct State {
    /// `startup` while the guest has not been started, `running` after.
    mode: &'static str,
    /// The paragraph the panel opens with: why it is gating the start, or that
    /// what is typed into it lands at the next one.
    note: String,
    /// Full path of the image the panel starts on.
    path: String,
    /// Its file name, which is what the disk row shows.
    name: String,
    /// Whether the next launch is to boot this image without asking.
    autostart: bool,
    memory: u32,
    env: String,
    /// The environments to offer, so the panel does not carry its own copy of
    /// a list the launcher already validates against.
    envs: [&'static str; 3],
    min_memory: u32,
}

/// What the panel needs at page load and whenever the settings are opened.
///
/// Cannot race the decision it reports. Commands are dispatched from the event
/// loop, which only starts once `setup` has returned, and `setup` is where the
/// decision is made. The one thing that used to pump events from inside it was
/// the disk picker's own modal, and the picker no longer runs there.
#[tauri::command]
pub(crate) fn settings_state(launcher: tauri::State<'_, Mutex<Launcher>>) -> Result<State, String> {
    launcher.lock().unwrap().state()
}

/// Write the panel's values into the settings file, for the next launch.
#[tauri::command]
pub(crate) fn save_settings(
    launcher: tauri::State<'_, Mutex<Launcher>>,
    disk: String,
    autostart: bool,
    memory: u32,
    env: String,
) -> Result<(), String> {
    let mut launcher = launcher.lock().unwrap();
    check(memory, &env)?;
    if disk.is_empty() {
        return Err("Use Open to select an image or New to create one first.".to_owned());
    }
    let disk = PathBuf::from(disk);

    launcher
        .settings
        .apply(Some(&disk), autostart, memory, &env)
        .map_err(|e| format!("{e:#}"))
}

/// Boot the guest on the image the startup form settled on. `save` is the
/// difference between the form's two ways of doing that: it writes what the
/// form is showing down for the launches after this one, where plain start
/// takes it for this one only and leaves the settings file as it was.
///
/// Failures split two ways. An image another emulator holds, or a settings file
/// that will not take the choice, comes back here for the panel to show: the
/// user can pick something else and press the button again. QEMU refusing to
/// start is fatal and goes to the error window, which is where every other
/// fatal startup failure already ends up.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) fn start_emulator(
    app: tauri::AppHandle,
    launcher: tauri::State<'_, Mutex<Launcher>>,
    disk: String,
    autostart: bool,
    save: bool,
    memory: u32,
    env: String,
) -> Result<(), String> {
    let mut launcher = launcher.lock().unwrap();
    if launcher.pending.is_none() {
        return Err("This emulator has already started.".to_owned());
    }
    check(memory, &env)?;
    if disk.is_empty() {
        return Err("Use Open to select an image or New to create one first.".to_owned());
    }

    let disk = std::path::absolute(PathBuf::from(disk))
        .map_err(|e| format!("That location cannot be used: {e}"))?;

    disk::require_existing(&disk).map_err(|e| format!("{e:#}"))?;
    disk::require_available(&disk).map_err(|e| format!("{e:#}"))?;

    if save {
        launcher
            .settings
            .apply(Some(&disk), autostart, memory, &env)
            .map_err(|e| format!("{e:#}"))?;
    }

    let pending = launcher.take().expect("checked just above");

    // Past this point the ingredients are spent: a failure is QEMU's, and the
    // error window replaces the device face rather than the panel offering a
    // retry that has nothing left to retry with.
    if let Err(err) = crate::launch(&app, pending, &disk, memory, &env) {
        crate::error_dialog::show_from_thread(&app, crate::error_dialog::COULD_NOT_START, err);
    }
    Ok(())
}

/// Reject what the settings should not have been able to send. The webview is
/// the launcher's own page, but it is still the one place a value arrives from
/// outside Rust, and the guest is spawned from these.
fn check(memory: u32, env: &str) -> Result<(), String> {
    if memory < MIN_MEMORY {
        return Err(format!(
            "That is too little memory. The least is {MIN_MEMORY} MiB."
        ));
    }
    if !ENVS.contains(&env) {
        return Err(format!("{env} is not one of {}.", ENVS.join(", ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::Path;
    use tempfile::TempDir;

    /// A launcher that has not started its guest, carrying the flags given.
    fn waiting(dir: &Path, memory: Option<u32>, env: Option<&str>) -> Launcher {
        let pending = Pending {
            boot: Boot {
                disk: None,
                env: env.map(str::to_owned),
                memory,
                arch: None,
                kernel: None,
                initrd: None,
                host_addr: None,
            },
            arch: GuestArch::Amd64,
            host_port: HostPort::fixed("127.0.0.1:18181".parse::<SocketAddr>().unwrap()),
            firmware: Firmware {
                kernel: PathBuf::new(),
                initrd: PathBuf::new(),
                bundled: false,
            },
            qemu_libs: None,
        };
        Launcher::booting(pending, Settings::load(dir).unwrap())
    }

    #[test]
    fn test_a_flag_seeds_the_form_over_the_settings_file() {
        let tmp = TempDir::new().unwrap();
        Settings::load(tmp.path())
            .unwrap()
            .apply(None, true, 2048, "staging")
            .unwrap();

        let launcher = waiting(tmp.path(), Some(4096), Some("develop"));
        assert_eq!(launcher.effective(), (4096, "develop".to_owned()));
    }

    #[test]
    fn test_the_settings_file_answers_when_no_flag_does() {
        let tmp = TempDir::new().unwrap();
        Settings::load(tmp.path())
            .unwrap()
            .apply(None, true, 2048, "staging")
            .unwrap();

        let launcher = waiting(tmp.path(), None, None);
        assert_eq!(launcher.effective(), (2048, "staging".to_owned()));
    }

    #[test]
    fn test_the_built_in_defaults_answer_when_nothing_else_does() {
        let tmp = TempDir::new().unwrap();
        let launcher = waiting(tmp.path(), None, None);
        assert_eq!(
            launcher.effective(),
            (DEFAULT_MEMORY, DEFAULT_ENV.to_owned())
        );
    }

    #[test]
    fn test_running_panel_reloads_the_saved_preferences() {
        let tmp = TempDir::new().unwrap();
        let mut launcher = waiting(tmp.path(), Some(4096), Some("develop"));
        launcher.take();

        let mut external = Settings::load(tmp.path()).unwrap();
        external
            .apply(Some(Path::new("saved.ark")), false, 2048, "staging")
            .unwrap();
        let state = launcher.state().unwrap();
        assert_eq!(state.mode, "running");
        assert_eq!(state.path, "saved.ark");
        assert!(!state.autostart);
        assert_eq!(state.memory, 2048);
        assert_eq!(state.env, "staging");

        external.apply(None, true, 8192, "release").unwrap();
        let state = launcher.state().unwrap();
        assert!(state.path.is_empty());
        assert!(state.name.is_empty());
        assert!(state.autostart);
        assert_eq!(state.memory, 8192);
        assert_eq!(state.env, "release");
    }

    #[test]
    fn test_running_panel_reports_a_broken_file_and_can_retry() {
        let tmp = TempDir::new().unwrap();
        let mut launcher = waiting(tmp.path(), None, None);
        launcher.take();
        let file = launcher.settings.path().to_path_buf();
        std::fs::write(&file, "invalid toml").unwrap();
        let err = launcher.state().err().expect("a broken file was accepted");
        assert!(err.contains("could not parse"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "invalid toml");

        std::fs::write(&file, "version = 1\nmemory = 3072\n").unwrap();
        assert_eq!(launcher.state().unwrap().memory, 3072);
    }

    #[test]
    fn test_startup_panel_keeps_the_launch_overrides() {
        let tmp = TempDir::new().unwrap();
        let mut launcher = waiting(tmp.path(), Some(4096), Some("develop"));
        launcher.ask(None, Reason::FirstRun);
        Settings::load(tmp.path())
            .unwrap()
            .apply(Some(Path::new("other.ark")), false, 2048, "staging")
            .unwrap();
        let state = launcher.state().unwrap();
        assert_eq!(state.mode, "startup");
        assert!(state.path.is_empty());
        assert_eq!(state.memory, 4096);
        assert_eq!(state.env, "develop");
    }

    /// The panel reads these by name, so a rename here is a silently blank
    /// control there.
    #[test]
    fn test_the_state_carries_every_field_the_panel_reads() {
        let state = State {
            mode: "startup",
            note: "why".to_owned(),
            path: "/tmp/a.ark".to_owned(),
            name: "a.ark".to_owned(),
            autostart: true,
            memory: 4096,
            env: "develop".to_owned(),
            envs: ENVS,
            min_memory: MIN_MEMORY,
        };
        let json: serde_json::Value = serde_json::to_value(&state).unwrap();
        for field in [
            "mode",
            "note",
            "path",
            "name",
            "autostart",
            "memory",
            "env",
            "envs",
            "minMemory",
        ] {
            assert!(
                json.get(field).is_some(),
                "{field} is missing from the state"
            );
        }
    }
}
