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

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::bundle::app_data_dir;
use crate::diagnostics::log;
use crate::error_dialog::NO_DIALOG;
use crate::settings::Settings;
use crate::Config;

/// Name given to the disk image the launcher allocates for itself, both as the
/// picker's suggestion and as the answer when there is nobody to ask.
const DEFAULT_DISK: &str = "ark-disk.img";

/// Work out which image to boot, asking the user if nothing is known yet and
/// remembering the answer. `Ok(None)` means the picker was dismissed, which is
/// a decision to not start rather than a failure.
pub(crate) fn resolve(
    app: &tauri::App,
    cfg: &Config,
    settings: &mut Settings,
) -> Result<Option<PathBuf>> {
    if let Some(disk) = &cfg.disk {
        let disk = std::path::absolute(disk)
            .with_context(|| format!("could not resolve --disk {}", disk.display()))?;
        return Ok(Some(disk));
    }

    let dir = app_data_dir(app)?;

    if let Some(disk) = settings.disk() {
        if disk.exists() {
            return Ok(Some(disk.to_path_buf()));
        }
        log!(
            "[launcher] the remembered disk image {} is gone, asking for another",
            disk.display()
        );
    }

    // The same switch the error window honours: no window anybody has to
    // dismiss. CI launches a packaged build with no flags at all and expects
    // it to boot unattended, so fall back to the image the launcher would have
    // allocated for itself before there was anything to ask.
    if std::env::var_os(NO_DIALOG).is_some() {
        return Ok(Some(dir.join(DEFAULT_DISK)));
    }

    let Some(disk) = pick(&dir) else {
        log!("[launcher] no disk image chosen");
        return Ok(None);
    };
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
