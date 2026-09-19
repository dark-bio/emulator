// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

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

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use tauri::PackageInfo;

use crate::Boot;
use crate::diagnostics::log;
use crate::platform::strip_verbatim_prefix;
use crate::qemu::GuestArch;

/// Where this build keeps its files, resolved without a window so that a
/// command line run never has to start one.
pub(crate) struct Paths {
    /// Everything the launcher writes: the settings file, the logs, and the
    /// disk images it allocates for itself.
    pub(crate) data: PathBuf,

    /// What a packaged build ships beside the executable. `None` in a source
    /// build, which ships nothing.
    pub(crate) resources: Option<PathBuf>,
}

impl Paths {
    /// Work out both. Nothing is created: a command that only reads leaves no
    /// directory behind, and the window makes the data directory itself.
    pub(crate) fn resolve(identifier: &str, package: &PackageInfo) -> Result<Self> {
        Ok(Self {
            data: app_data_dir(identifier)?,
            resources: resource_dir(package),
        })
    }
}

/// The firmware a guest boots, and where it came from.
pub(crate) struct Firmware {
    /// Kernel image the guest boots.
    pub(crate) kernel: PathBuf,

    /// Initramfs that goes with it.
    pub(crate) initrd: PathBuf,

    /// Whether this build carries the pair itself, rather than being pointed
    /// at one with `--kernel` and `--initrd`. A bundled firmware is a release
    /// and boots like the hardware does, silently.
    pub(crate) bundled: bool,
}

/// Resolve the kernel/initrd paths to boot. Explicit `--kernel`/`--initrd`
/// take priority, and are the only option in a source build.
pub(crate) fn resolve_firmware(
    resources: Option<&Path>,
    boot: &Boot,
    arch: GuestArch,
) -> Result<Firmware> {
    match (&boot.kernel, &boot.initrd) {
        (Some(kernel), Some(initrd)) => {
            return Ok(Firmware {
                kernel: kernel.clone(),
                initrd: initrd.clone(),
                bundled: false,
            });
        }
        (None, None) => {}
        _ => bail!("--kernel and --initrd must be passed together"),
    }

    bundled_firmware(resources, arch).with_context(|| {
        format!(
            "no bundled firmware for {}: pass --kernel and --initrd explicitly \
             (a development build has no bundled firmware)",
            arch.name()
        )
    })
}

/// The firmware this build ships for `arch`, if it ships one at all.
pub(crate) fn bundled_firmware(resources: Option<&Path>, arch: GuestArch) -> Option<Firmware> {
    let dir = resources?.join("firmware").join(arch.name());
    let kernel = dir.join("kernel");
    let initrd = dir.join("initrd.gz");
    (kernel.exists() && initrd.exists()).then_some(Firmware {
        kernel,
        initrd,
        bundled: true,
    })
}

/// The release the bundled firmware was taken from, written beside the images
/// by whatever fetched them. Absent in a build that ships no firmware.
pub(crate) fn firmware_version(resources: Option<&Path>, arch: GuestArch) -> Option<String> {
    let path = resources?
        .join("firmware")
        .join(arch.name())
        .join("version");
    let tag = std::fs::read_to_string(path).ok()?;
    let tag = tag.trim().to_owned();
    (!tag.is_empty()).then_some(tag)
}

/// Resolve this app's own data directory. It is the platform's data
/// directory joined with this app's bundle identifier, which is what the
/// window side resolves to as well.
fn app_data_dir(identifier: &str) -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("could not locate this computer's data directory")?
        .join(identifier))
}

/// Resolve where a packaged build's resources sit, which is beside or above
/// the executable depending on the platform. `None` when there is no such
/// directory, which is the ordinary state of a source build.
fn resource_dir(package: &PackageInfo) -> Option<PathBuf> {
    let dir = match tauri::utils::platform::resource_dir(package, &tauri::Env::default()) {
        Ok(dir) => strip_verbatim_prefix(&dir),
        Err(e) => {
            log!("[launcher] could not resolve the resource directory: {e}");
            return None;
        }
    };
    dir.exists().then_some(dir)
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
pub(crate) fn resolve_qemu_libs(resources: Option<&Path>) -> Option<PathBuf> {
    let Some(dir) = resources.map(|dir| dir.join("qemu-libs")) else {
        log!("[launcher] no bundled resources, using QEMU's own search paths");
        return None;
    };
    if !dir.exists() {
        log!("[launcher] no bundled qemu-libs, using QEMU's own search paths");
        return None;
    }
    let count = std::fs::read_dir(&dir).map(|it| it.count()).unwrap_or(0);
    log!(
        "[launcher] using bundled qemu-libs at {} ({count} files)",
        dir.display()
    );
    Some(dir)
}
