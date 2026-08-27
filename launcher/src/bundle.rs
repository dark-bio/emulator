//! Locating what a packaged build ships alongside the launcher: firmware
//! images, the QEMU sidecar binaries, and QEMU's shared libraries and datadir.
//!
//! A packaged build bundles QEMU and firmware for the build host's own
//! architecture only (see `tauri.release.conf.json`), so end users need
//! neither installed. Everything here therefore resolves to `None` or an
//! error in a plain source build, and callers fall back to
//! `--kernel`/`--initrd` and a QEMU on `PATH`. A cross-architecture guest
//! takes that same fallback even from a packaged build, since the bundled
//! architecture is not the one being asked for.
//!
//! Nothing here resolves against the current directory. A packaged app's
//! working directory is not reliably writable or even meaningful: a Tauri
//! AppImage's own AppRun moves it inside the read-only FUSE mount before the
//! launcher runs.

use std::error::Error;
use std::path::PathBuf;

use tauri::{path::BaseDirectory, Manager};

use crate::platform::strip_verbatim_prefix;
use crate::qemu::GuestArch;
use crate::Config;

/// Resolve the kernel/initrd paths to boot. Explicit `--kernel`/`--initrd`
/// take priority, and are the only option in a source build.
pub(crate) fn resolve_firmware(
    app: &tauri::App,
    cfg: &Config,
    arch: GuestArch,
) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    match (&cfg.kernel, &cfg.initrd) {
        (Some(kernel), Some(initrd)) => return Ok((kernel.clone(), initrd.clone())),
        (None, None) => {}
        _ => return Err("--kernel and --initrd must be passed together".into()),
    }

    let dir = arch.firmware_dir();
    let kernel = strip_verbatim_prefix(
        &app.path()
            .resolve(format!("firmware/{dir}/kernel"), BaseDirectory::Resource)?,
    );
    let initrd = strip_verbatim_prefix(
        &app.path()
            .resolve(format!("firmware/{dir}/initrd.gz"), BaseDirectory::Resource)?,
    );
    for (label, path) in [("kernel", &kernel), ("initrd", &initrd)] {
        if !path.exists() {
            return Err(format!(
                "no bundled {label} for {dir}: pass --kernel and --initrd explicitly \
                 (a development build has no bundled firmware)"
            )
            .into());
        }
    }
    Ok((kernel, initrd))
}

/// Resolve the backing disk image's path, defaulting to `disk.img` under this
/// app's own data directory and creating that directory if missing. An
/// explicit `--disk` is absolutized against the current directory, which is a
/// deliberate override typed at a shell.
pub(crate) fn resolve_disk_path(app: &tauri::App, cfg: &Config) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(disk) = &cfg.disk {
        return Ok(std::path::absolute(disk)?);
    }
    let dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("disk.img"))
}

/// Resolve a bundled sidecar binary next to the launcher's own executable,
/// where a packaged build's `externalBin` entries land. `None` in a source
/// build, which declares no `externalBin` at all, leaving callers to fall
/// back to whatever the developer has on `PATH`.
pub(crate) fn resolve_sidecar(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?.to_path_buf();
    // Sidecars land beside the top-level target dir, not in `deps/`.
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir = dir.parent()?.to_path_buf();
    }
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = dir.join(file);
    path.exists().then_some(path)
}

/// Resolve the bundled directory of QEMU's shared libraries and firmware/BIOS
/// datadir. `None` in a source build, where the installed QEMU already knows
/// where its own live.
///
/// Logs unconditionally: a wrong directory here surfaces later as an opaque
/// library or firmware error, so this is the one place that can say what it
/// picked.
pub(crate) fn resolve_qemu_libs(app: &tauri::App) -> Option<PathBuf> {
    let dir = match app.path().resolve("qemu-libs", BaseDirectory::Resource) {
        Ok(dir) => strip_verbatim_prefix(&dir),
        Err(e) => {
            eprintln!("[launcher] could not resolve qemu-libs resource directory: {e}");
            return None;
        }
    };
    if !dir.exists() {
        eprintln!("[launcher] no bundled qemu-libs, using QEMU's own search paths");
        return None;
    }
    let count = std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(0);
    eprintln!(
        "[launcher] using bundled qemu-libs at {} ({count} files)",
        dir.display()
    );
    Some(dir)
}
