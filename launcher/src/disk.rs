//! Deciding which disk image the guest boots from.
//!
//! Three sources, in descending order of how deliberate they are:
//!
//!   - `--disk`, typed at a shell for this one run. Never consults or updates
//!     the settings, so a one-off boot from some other image leaves the
//!     remembered choice alone.
//!   - The image remembered in `settings`, as long as it is still there. A
//!     path whose file has since been deleted falls through to asking, which
//!     is also how a device that failed to allocate gets a second try.
//!   - Whatever the user picks in the settings panel, which is then remembered
//!     unless they say otherwise.
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
//! The picker is a save dialog rather than an open one because an open dialog
//! cannot name a file that does not exist yet, and creating the first image is
//! the whole of the first-run story. Picking an existing image works too, and
//! [`crate::qemu::ensure_disk`] tells the two apart afterwards by whether the
//! file is there. The cost is that macOS and Windows ask about replacing a
//! file that gets loaded rather than replaced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context as _, Result};
use serde::Serialize;

use crate::diagnostics::log;
use crate::discovery::disk_id;

/// Name given to the disk image the launcher allocates for itself, both as the
/// picker's suggestion and as the answer when there is nobody to ask.
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
            Self::FirstRun => "You do not have an emulator yet. Choose where to keep it, \
                 and it will be made there the first time it starts."
                .to_owned(),
            Self::Missing(disk) => format!(
                "Your emulator has gone missing. {} is not where it was. Pick another, \
                 or choose where to keep a new one.",
                name_of(disk)
            ),
            Self::InUse(disk) => format!(
                "Your emulator is already running in another window. Two cannot share \
                 {}, so this one needs its own. Choose where to keep it.",
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
    Ask { suggestion: PathBuf, reason: Reason },
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
                    suggestion: default,
                    reason,
                });
            }
        } else if disk.exists() {
            return Ok(Resolved::Boot(disk.to_path_buf()));
        } else {
            let reason = Reason::Missing(disk.to_path_buf());
            log!(
                "[launcher] the remembered disk image {} is gone",
                disk.display()
            );
            if !no_dialog {
                return Ok(Resolved::Ask {
                    suggestion: default,
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
        suggestion: default,
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

/// Ask for a disk image, starting where `current` sits and with its name
/// filled in. `None` if the dialog was dismissed.
///
/// Async on purpose. A sync command runs on the main thread, and the save panel
/// has to run there too on macOS, so the dialog is handed to the event loop and
/// the answer waited for off it. Blocking on that answer is the point: a modal
/// is a modal, and the panel behind it is disabled until it closes.
#[tauri::command]
pub(crate) async fn pick_disk(app: tauri::AppHandle, current: String) -> Option<Picked> {
    let current = PathBuf::from(current);
    let dir = current.parent().unwrap_or(Path::new("")).to_path_buf();
    let name = current
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_DISK.to_owned());

    let (tx, rx) = std::sync::mpsc::channel();
    if let Err(e) = app.run_on_main_thread(move || {
        let _ = tx.send(pick(&dir, &name));
    }) {
        log!("[launcher] could not raise the disk picker: {e}");
        return None;
    }
    let picked = tauri::async_runtime::spawn_blocking(move || rx.recv().ok().flatten())
        .await
        .ok()
        .flatten()?;

    Some(Picked {
        name: name_of(&picked),
        path: picked.display().to_string(),
    })
}

/// Raise the save dialog itself.
fn pick(dir: &Path, name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Where should this emulator be kept?")
        .set_directory(dir)
        .set_file_name(name)
        .save_file()
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
    fn test_an_explicit_disk_wins() {
        let tmp = TempDir::new().unwrap();
        let disk = tmp.path().join("explicit.img");
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);

        let resolved = decide(
            Some(&disk),
            Some(&remembered),
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

        let Err(err) = decide(Some(&disk), None, &booted, tmp.path(), PORT, false) else {
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
    fn test_a_remembered_disk_that_is_gone_asks() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");

        let resolved = decide(
            None,
            Some(&remembered),
            &Booted::new(),
            tmp.path(),
            PORT,
            false,
        )
        .unwrap();
        let Resolved::Ask { suggestion, reason } = resolved else {
            panic!("a deleted remembered image was booted");
        };
        assert_eq!(suggestion, tmp.path().join(DEFAULT_DISK));
        assert!(matches!(reason, Reason::Missing(_)));
    }

    #[test]
    fn test_a_remembered_disk_that_is_booted_asks() {
        let tmp = TempDir::new().unwrap();
        let remembered = tmp.path().join("remembered.img");
        touch(&remembered);
        let booted = Booted::from([(disk_id(&remembered), 18181)]);

        let resolved = decide(None, Some(&remembered), &booted, tmp.path(), PORT, false).unwrap();
        let Resolved::Ask { reason, .. } = resolved else {
            panic!("an image booted elsewhere was booted again");
        };
        assert!(matches!(reason, Reason::InUse(_)));
    }

    #[test]
    fn test_a_first_run_asks() {
        let tmp = TempDir::new().unwrap();

        let resolved = decide(None, None, &Booted::new(), tmp.path(), PORT, false).unwrap();
        let Resolved::Ask { suggestion, reason } = resolved else {
            panic!("a first run booted something");
        };
        assert_eq!(suggestion, tmp.path().join(DEFAULT_DISK));
        assert!(matches!(reason, Reason::FirstRun));
    }

    #[test]
    fn test_nobody_to_ask_allocates_an_image() {
        let tmp = TempDir::new().unwrap();

        let resolved = decide(None, None, &Booted::new(), tmp.path(), PORT, true).unwrap();
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

        let resolved = decide(None, None, &booted, tmp.path(), PORT, true).unwrap();
        let Resolved::Boot(disk) = resolved else {
            panic!("an unattended launch asked anyway");
        };
        assert_eq!(disk, tmp.path().join(format!("ark-disk-{PORT}.img")));
    }
}
