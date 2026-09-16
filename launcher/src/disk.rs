//! Deciding which disk image the guest boots from.
//!
//! Three sources, in descending order of how deliberate they are:
//!
//!   - `--disk`, typed at a shell for this one run. Never consults or updates
//!     the settings, so a one-off boot from some other image leaves the
//!     remembered choice alone.
//!   - The image remembered in `settings`, when autostart is enabled and it
//!     is still there. With autostart disabled, the panel offers that image. A
//!     path whose file has since been deleted falls through to asking, which
//!     is also how a device that failed to allocate gets a second try.
//!   - Whatever the user picks in the settings panel, which is then remembered
//!     when they save the form.
//!
//! Crossing all three is whether an image is already booted by another
//! emulator, since two guests writing one qcow2 would corrupt it. An explicit
//! `--disk` naming a booted image is an error, because substituting another
//! file silently is worse than saying no, while a remembered one falls through
//! to asking the way a deleted one does.
//!
//! [`decide`] never puts anything on screen. It answers either with an image to
//! boot or with a suggestion and the reason it cannot proceed, and the launcher
//! turns the second into the settings panel's startup form. The picker below is
//! then raised by that panel, from a button the user pressed, rather than as a
//! modal appearing out of nowhere before the app has drawn anything.
//!
//! Open selects an existing image. New uses a save dialog and creates the
//! image immediately, replacing existing contents only after the native
//! dialog confirms that choice. Starting from the panel never creates an image.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context as _, Result};
use serde::Serialize;

use crate::diagnostics::log;
use crate::discovery::disk_id;

/// Name used when an unattended launch needs to allocate an image.
pub(crate) const DEFAULT_DISK: &str = "ark-disk.img";

/// The image this run settled on, so that the device face can name it.
static BOOTED: OnceLock<PathBuf> = OnceLock::new();

/// Full path of the disk image backing the emulated device, for the info tray
/// on the device face. `None` until the guest has been started, which is the
/// whole time the settings panel's startup form is up.
#[tauri::command]
pub(crate) fn disk_path() -> Option<String> {
    BOOTED.get().map(|disk| disk.display().to_string())
}

/// Publish the image the guest was started on. Called once, as QEMU is spawned.
pub(crate) fn mark_booted(disk: &Path) {
    let _ = BOOTED.set(disk.to_path_buf());
}

/// Images already booted by other emulators, keyed by [`disk_id`] and mapped to
/// the port holding them so a refusal can say which emulator is in the way.
pub(crate) type Booted = HashMap<String, u16>;

/// Why the launcher cannot pick an image on its own and has to ask.
pub(crate) enum Reason {
    /// Automatic startup is disabled.
    AutostartDisabled,
    /// Nothing has ever been chosen.
    FirstRun,
    /// The remembered image is no longer on disk.
    Missing(PathBuf),
    /// The remembered image is booted by another emulator.
    InUse(PathBuf),
}

impl Reason {
    /// What the user is told, in the settings the window opens with. Names the
    /// file rather than its full path, matching how the info tray words the
    /// same one, and because there is only a window's width to say it in.
    ///
    /// Written for somebody who wants an emulated device, not for somebody who
    /// wants to know how one is stored: no file formats, no ports, and nothing
    /// about where any of this lives in the app.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::AutostartDisabled => {
                "Choose an emulator and press start when you are ready.".to_owned()
            }
            Self::FirstRun => "Open an existing emulator or use New to create one, \
                 then press start."
                .to_owned(),
            Self::Missing(disk) => format!(
                "The default emulator ({}) is missing. \
                 Open another or use New to create one.",
                name_of(disk)
            ),
            Self::InUse(disk) => format!(
                "The default emulator ({}) is already running. \
                 Open another or use New to create one.",
                name_of(disk)
            ),
        }
    }
}

/// What the launcher works out about the disk before anything is on screen.
pub(crate) enum Resolved {
    /// Boot this image, without asking.
    Boot(PathBuf),
    /// Ask, offering `suggestion` and saying why.
    Ask {
        suggestion: Option<PathBuf>,
        reason: Reason,
    },
}

/// Work out which image to boot. `dir` is the app's data directory, where an
/// image the launcher allocates for itself lives, and `port` is the one this
/// emulator holds.
///
/// `no_dialog` is [`crate::error_dialog::NO_DIALOG`], which stands for "there
/// is nobody here to ask". CI launches a packaged build with no flags at all
/// and expects it to boot unattended, so it falls back to the image the
/// launcher would have allocated for itself. A second unattended emulator
/// cannot share that one, so it gets an image named after the port it holds.
pub(crate) fn decide(
    explicit: Option<&Path>,
    remembered: Option<&Path>,
    autostart: bool,
    booted: &Booted,
    dir: &Path,
    port: u16,
    no_dialog: bool,
) -> Result<Resolved> {
    if let Some(disk) = explicit {
        let disk = std::path::absolute(disk)
            .with_context(|| format!("could not resolve --disk {}", disk.display()))?;
        if let Some(port) = booted.get(&disk_id(&disk)) {
            bail!(
                "the disk image {} is already booted by the emulator on port {port}; \
                 two emulators cannot share one image",
                disk.display()
            );
        }
        return Ok(Resolved::Boot(disk));
    }

    let default = dir.join(DEFAULT_DISK);

    if let Some(disk) = remembered {
        if let Some(port) = booted.get(&disk_id(disk)) {
            let reason = Reason::InUse(disk.to_path_buf());
            log!(
                "[launcher] the remembered disk image {} is already booted on port {port}",
                disk.display()
            );
            if !no_dialog {
                return Ok(Resolved::Ask {
                    suggestion: None,
                    reason,
                });
            }
        } else if disk.is_file() {
            if !autostart && !no_dialog {
                return Ok(Resolved::Ask {
                    suggestion: Some(disk.to_path_buf()),
                    reason: Reason::AutostartDisabled,
                });
            }
            return Ok(Resolved::Boot(disk.to_path_buf()));
        } else {
            let reason = Reason::Missing(disk.to_path_buf());
            log!(
                "[launcher] the remembered disk image {} is gone",
                disk.display()
            );
            if !no_dialog {
                return Ok(Resolved::Ask {
                    suggestion: None,
                    reason,
                });
            }
        }
    }

    if no_dialog {
        if !booted.contains_key(&disk_id(&default)) {
            return Ok(Resolved::Boot(default));
        }
        return Ok(Resolved::Boot(dir.join(format!("ark-disk-{port}.img"))));
    }

    Ok(Resolved::Ask {
        suggestion: None,
        reason: Reason::FirstRun,
    })
}

/// An image the user chose in the picker, as the settings panel needs it.
#[derive(Serialize)]
pub(crate) struct Picked {
    /// Full path, which is what goes back to the launcher on start.
    path: String,
    /// File name, which is what the panel shows.
    name: String,
}

/// Select an existing image, or create one immediately through a save dialog.
/// `None` means the user dismissed the dialog.
#[tauri::command]
pub(crate) async fn pick_disk(
    app: tauri::AppHandle,
    launcher: tauri::State<'_, Mutex<crate::panel::Launcher>>,
    current: String,
    create: bool,
) -> Result<Option<Picked>, String> {
    let qemu_libs = launcher.lock().unwrap().qemu_libs().map(Path::to_path_buf);
    let picked = choose_disk(&app, Path::new(&current), create).await?;
    let Some(path) = picked else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || {
        let path =
            std::path::absolute(path).map_err(|e| format!("That location cannot be used: {e}"))?;
        if create {
            require_available(&path).map_err(|e| format!("{e:#}"))?;
            crate::qemu::create_disk(&path, qemu_libs.as_deref())
                .map_err(|e| format!("Could not create {}: {e:#}", name_of(&path)))?;
        } else {
            require_existing(&path).map_err(|e| format!("{e:#}"))?;
        }
        Ok(Some(Picked {
            name: name_of(&path),
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
    let name = name_of(current);
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

/// Require an existing image so a panel start cannot silently recreate one.
pub(crate) fn require_existing(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).with_context(|| {
        format!(
            "Could not open {}. Use Open to select an existing image or New to create one",
            name_of(path)
        )
    })?;
    if !metadata.is_file() {
        bail!(
            "{} is not a disk image file. Use Open to select an image.",
            name_of(path)
        );
    }
    Ok(())
}

/// Check current usage again after the dialog, since it may have stayed open
/// while another emulator started. The local image is checked even if the
/// registry is unavailable.
pub(crate) fn require_available(path: &Path) -> Result<()> {
    let booted: Booted = crate::discovery::list()
        .into_iter()
        .map(|instance| (instance.disk_id, instance.port))
        .collect();
    check_available(path, BOOTED.get().map(PathBuf::as_path), &booted)
}

/// Reject both this window's image and images reported by other launchers.
fn check_available(path: &Path, running: Option<&Path>, booted: &Booted) -> Result<()> {
    let id = disk_id(path);
    if running.is_some_and(|running| disk_id(running) == id) {
        bail!(
            "{} is running in this window. Stop this emulator before replacing its image.",
            name_of(path)
        );
    }
    if booted.contains_key(&id) {
        bail!(
            "{} is already running in another window. Close that emulator or choose a different image.",
            name_of(path)
        );
    }
    Ok(())
}

/// The file name of `disk`, for anything shown to the user. A path with no
/// final component is not something the picker can produce, so the fallback is
/// only there to keep this total.
pub(crate) fn name_of(disk: &Path) -> String {
    disk.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| disk.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The port this emulator would be holding. Only ever shows up in the name
    /// of an image allocated for a second unattended emulator.
    const PORT: u16 = 18182;

    fn touch(path: &Path) {
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn test_opening_a_missing_image_does_not_create_it() {
        let tmp = TempDir::new().unwrap();
        let disk = tmp.path().join("missing.img");
        let err = require_existing(&disk).unwrap_err().to_string();
        assert!(err.contains("Open"), "{err}");
        assert!(err.contains("New"), "{err}");
        assert!(!disk.exists());
        assert!(require_existing(tmp.path()).is_err());
    }

    #[test]
    fn test_replacing_an_image_in_use_is_refused() {
        let tmp = TempDir::new().unwrap();
        let disk = tmp.path().join("running.img");
        touch(&disk);
        let booted = Booted::from([(disk_id(&disk), PORT)]);
        let err = check_available(&disk, None, &booted)
            .unwrap_err()
            .to_string();
        assert!(err.contains("another window"), "{err}");

        let err = check_available(&disk, Some(&disk), &Booted::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("this window"), "{err}");
        assert!(check_available(&tmp.path().join("new.img"), Some(&disk), &booted).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_an_alias_of_a_running_image_is_also_refused() {
        let tmp = TempDir::new().unwrap();
        let disk = tmp.path().join("running.img");
        let alias = tmp.path().join("alias.img");
        touch(&disk);
        std::os::unix::fs::symlink(&disk, &alias).unwrap();
        assert!(check_available(&alias, Some(&disk), &Booted::new()).is_err());
        let booted = Booted::from([(disk_id(&disk), PORT)]);
        assert!(check_available(&alias, None, &booted).is_err());
    }

    #[test]
    fn test_an_explicit_disk_wins() {
        let tmp = TempDir::new().unwrap();
        let disk = tmp.path().join("explicit.img");
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);

        let resolved = decide(
            Some(&disk),
            Some(&remembered),
            false,
            &Booted::new(),
            tmp.path(),
            PORT,
            false,
        )
        .unwrap();
        let Resolved::Boot(booted) = resolved else {
            panic!("an explicit --disk was not taken");
        };
        assert_eq!(booted, disk);
    }

    #[test]
    fn test_an_explicit_disk_that_is_booted_is_refused() {
        let tmp = TempDir::new().unwrap();
        let disk = tmp.path().join("explicit.img");
        touch(&disk);
        let booted = Booted::from([(disk_id(&disk), 18181)]);

        let Err(err) = decide(Some(&disk), None, false, &booted, tmp.path(), PORT, false) else {
            panic!("a booted image was accepted");
        };
        assert!(err.to_string().contains("already booted"), "{err}");
    }

    #[test]
    fn test_a_remembered_disk_boots_without_asking() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);

        let resolved = decide(
            None,
            Some(&remembered),
            true,
            &Booted::new(),
            tmp.path(),
            PORT,
            false,
        )
        .unwrap();
        let Resolved::Boot(booted) = resolved else {
            panic!("a usable remembered image was not taken");
        };
        assert_eq!(booted, remembered);
    }

    #[test]
    fn test_autostart_disabled_offers_the_remembered_disk() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);

        let resolved = decide(
            None,
            Some(&remembered),
            false,
            &Booted::new(),
            tmp.path(),
            PORT,
            false,
        )
        .unwrap();
        let Resolved::Ask { suggestion, reason } = resolved else {
            panic!("autostart was disabled but the image booted");
        };
        assert_eq!(suggestion, Some(remembered));
        assert!(matches!(reason, Reason::AutostartDisabled));
    }

    #[test]
    fn test_unattended_launch_boots_with_autostart_disabled() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);

        let resolved = decide(
            None,
            Some(&remembered),
            false,
            &Booted::new(),
            tmp.path(),
            PORT,
            true,
        )
        .unwrap();
        let Resolved::Boot(booted) = resolved else {
            panic!("an unattended launch asked anyway");
        };
        assert_eq!(booted, remembered);
    }

    #[test]
    fn test_a_remembered_disk_that_is_gone_asks() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");

        let resolved = decide(
            None,
            Some(&remembered),
            true,
            &Booted::new(),
            tmp.path(),
            PORT,
            false,
        )
        .unwrap();
        let Resolved::Ask { suggestion, reason } = resolved else {
            panic!("a deleted remembered image was booted");
        };
        assert!(suggestion.is_none());
        assert!(matches!(reason, Reason::Missing(_)));
    }

    #[test]
    fn test_a_remembered_disk_that_is_booted_asks() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);
        let booted = Booted::from([(disk_id(&remembered), 18181)]);

        let resolved = decide(
            None,
            Some(&remembered),
            true,
            &booted,
            tmp.path(),
            PORT,
            false,
        )
        .unwrap();
        let Resolved::Ask { reason, .. } = resolved else {
            panic!("an image booted elsewhere was booted again");
        };
        assert!(matches!(reason, Reason::InUse(_)));
    }

    #[test]
    fn test_a_first_run_asks() {
        let tmp = TempDir::new().unwrap();

        let resolved = decide(None, None, true, &Booted::new(), tmp.path(), PORT, false).unwrap();
        let Resolved::Ask { suggestion, reason } = resolved else {
            panic!("a first run booted something");
        };
        assert!(suggestion.is_none());
        assert!(matches!(reason, Reason::FirstRun));
    }

    #[test]
    fn test_nobody_to_ask_allocates_an_image() {
        let tmp = TempDir::new().unwrap();

        let resolved = decide(None, None, true, &Booted::new(), tmp.path(), PORT, true).unwrap();
        let Resolved::Boot(booted) = resolved else {
            panic!("an unattended launch asked anyway");
        };
        assert_eq!(booted, tmp.path().join(DEFAULT_DISK));
    }

    #[test]
    fn test_nobody_to_ask_avoids_an_image_in_use() {
        let tmp = TempDir::new().unwrap();
        let default = tmp.path().join(DEFAULT_DISK);
        let booted = Booted::from([(disk_id(&default), 18181)]);

        let resolved = decide(None, None, true, &booted, tmp.path(), PORT, true).unwrap();
        let Resolved::Boot(disk) = resolved else {
            panic!("an unattended launch asked anyway");
        };
        assert_eq!(disk, tmp.path().join(format!("ark-disk-{PORT}.img")));
    }
}
