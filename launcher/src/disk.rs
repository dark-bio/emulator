//! Deciding which disk image the guest boots from.
//!
//! Three sources, in descending order of how deliberate they are:
//!
//!   - `--disk`, typed at a shell for this one run. Never consults or updates
//!     the settings, so a one-off boot from some other image leaves the
//!     remembered choice alone.
//!   - The image remembered in `settings`, as long as it is still there. A
//!     path whose file has since been deleted falls through to the picker,
//!     which is also how a device that failed to allocate gets a second try.
//!   - Whatever the user picks, which is then remembered.
//!
//! Crossing all three is the question of whether an image is already booted by
//! another emulator, since two guests writing one qcow2 would corrupt it. The
//! registry knows which images are in use, and an image that is answers
//! differently depending on how it was arrived at: an explicit `--disk` is an
//! error, because the user named a specific file and substituting another one
//! silently is worse than saying no, while a remembered image simply falls
//! through to the picker the way a deleted one does.
//!
//! The picker is a save dialog rather than an open one because an open dialog
//! cannot name a file that does not exist yet, and creating the first image is
//! the whole of the first-run story. Picking an existing image works too, and
//! [`crate::qemu::ensure_disk`] tells the two apart afterwards by whether the
//! file is there. The cost is that macOS and Windows ask about replacing a
//! file that gets loaded rather than replaced.
//!
//! Running from `setup` on the main thread is deliberate. Nothing is on screen
//! yet, the event loop has not started, and QEMU has not been spawned, so a
//! modal here blocks nothing and a dismissal has nothing to tear down.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context as _, Result};

use crate::bundle::app_data_dir;
use crate::diagnostics::log;
use crate::discovery::disk_id;
use crate::error_dialog::NO_DIALOG;
use crate::settings::Settings;
use crate::Config;

/// Name given to the disk image the launcher allocates for itself, both as the
/// picker's suggestion and as the answer when there is nobody to ask.
const DEFAULT_DISK: &str = "ark-disk.img";

/// The image this run settled on, so that the device face can name it.
static BOOTED: OnceLock<PathBuf> = OnceLock::new();

/// Full path of the disk image backing the emulated device, for the info tray
/// on the device face. `None` until one has been chosen, which the page can
/// still observe if it asks while the picker is up.
#[tauri::command]
pub(crate) fn disk_path() -> Option<String> {
    BOOTED.get().map(|disk| disk.display().to_string())
}

/// Images already booted by other emulators, keyed by [`disk_id`] and mapped to
/// the port holding them so a refusal can say which emulator is in the way.
pub(crate) type Booted = HashMap<String, u16>;

/// Work out which image to boot, asking the user if nothing is known yet and
/// remembering the answer. `Ok(None)` means the picker was dismissed, which is
/// a decision to not start rather than a failure.
pub(crate) fn resolve(
    app: &tauri::App,
    cfg: &Config,
    settings: &mut Settings,
    booted: &Booted,
    port: u16,
) -> Result<Option<PathBuf>> {
    let disk = choose(app, cfg, settings, booted, port)?;
    if let Some(disk) = &disk {
        let _ = BOOTED.set(disk.clone());
    }
    Ok(disk)
}

/// The decision itself, split out so every path it can take ends up published
/// through [`resolve`].
fn choose(
    app: &tauri::App,
    cfg: &Config,
    settings: &mut Settings,
    booted: &Booted,
    port: u16,
) -> Result<Option<PathBuf>> {
    if let Some(disk) = &cfg.disk {
        let disk = std::path::absolute(disk)
            .with_context(|| format!("could not resolve --disk {}", disk.display()))?;
        if let Some(port) = booted.get(&disk_id(&disk)) {
            bail!(
                "the disk image {} is already booted by the emulator on port {port}; \
                 two emulators cannot share one image",
                disk.display()
            );
        }
        return Ok(Some(disk));
    }

    let dir = app_data_dir(app)?;

    if let Some(disk) = settings.disk() {
        if booted.contains_key(&disk_id(disk)) {
            log!(
                "[launcher] the remembered disk image {} is already booted, asking for another",
                disk.display()
            );
        } else if disk.exists() {
            return Ok(Some(disk.to_path_buf()));
        } else {
            log!(
                "[launcher] the remembered disk image {} is gone, asking for another",
                disk.display()
            );
        }
    }

    // The same switch the error window honours: no window anybody has to
    // dismiss. CI launches a packaged build with no flags at all and expects
    // it to boot unattended, so fall back to the image the launcher would have
    // allocated for itself before there was anything to ask. A second unattended
    // emulator cannot share that one, so it gets an image named after the port
    // it holds, which is unique for as long as it is running.
    if std::env::var_os(NO_DIALOG).is_some() {
        let default = dir.join(DEFAULT_DISK);
        if !booted.contains_key(&disk_id(&default)) {
            return Ok(Some(default));
        }
        return Ok(Some(dir.join(format!("ark-disk-{port}.img"))));
    }

    let Some(disk) = pick(&dir) else {
        log!("[launcher] no disk image chosen");
        return Ok(None);
    };
    if let Some(port) = booted.get(&disk_id(&disk)) {
        bail!(
            "the disk image {} is already booted by the emulator on port {port}; \
             pick one that is not in use",
            disk.display()
        );
    }
    settings.set_disk(&disk)?;
    Ok(Some(disk))
}

/// Ask for a disk image, starting in `dir` with the default name filled in.
/// `None` if the dialog was dismissed.
fn pick(dir: &Path) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose a disk image location for the emulated Ark")
        .set_directory(dir)
        .set_file_name(DEFAULT_DISK)
        .save_file()
}
