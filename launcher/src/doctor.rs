// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The checklist that says what to fix on this computer and in this build:
//! a newer release, QEMU, the firmware, hardware acceleration, the data
//! directory, the settings, the remembered image, the registry and a free
//! port. Every check runs and the whole list prints; the first failure then
//! sets the exit, so a caller sees everything that is wrong at once.

use std::path::Path;

use serde_json::{Value, json};

use crate::bundle::{self, Paths};
use crate::diagnostics;
use crate::discovery;
use crate::error::{Code, Error};
use crate::output::{self, Output};
use crate::qemu::{self, GuestArch, HostPort};
use crate::registry::REGISTRY_PORT;
use crate::settings::Settings;
use crate::update;

/// Run every check and print the list, then fail with the first failure.
pub(crate) fn doctor(output: &Output, paths: &Paths, timeout: u64) -> Result<(), Error> {
    let mut checks = Checks::default();
    checks.update(timeout);
    let arch = GuestArch::native().ok_or_else(|| {
        Error::new(
            Code::FirmwareMissing,
            format!(
                "no firmware exists for {} computers",
                std::env::consts::ARCH
            ),
        )
    })?;
    let libs = bundle::resolve_qemu_libs(paths.resources.as_deref());

    let emulator = qemu::resolve_qemu(arch);
    let qemu_version = emulator.version(libs.as_deref());
    let origin = if emulator.bundled {
        "bundled"
    } else {
        "on PATH"
    };
    match &qemu_version {
        Some(version) => checks.ok(
            "qemu",
            &format!("{version} {origin} at {}", emulator.binary.display()),
        ),
        None => checks.fail(
            "qemu",
            Error::new(
                Code::QemuMissing,
                format!("{} could not be run", emulator.binary.display()),
            )
            .hint(if emulator.bundled {
                "the bundled QEMU is damaged; reinstall the emulator"
            } else {
                "install QEMU, or use a packaged build, which carries its own"
            }),
        ),
    }

    let firmware = bundle::bundled_firmware(paths.resources.as_deref(), arch);
    let firmware_version = bundle::firmware_version(paths.resources.as_deref(), arch);
    match &firmware {
        Some(_) => checks.ok(
            "firmware",
            &format!(
                "{} for {}",
                firmware_version.as_deref().unwrap_or("unversioned"),
                arch.name()
            ),
        ),
        None => checks.fail(
            "firmware",
            Error::new(
                Code::FirmwareMissing,
                format!("no firmware bundled for {}", arch.name()),
            )
            .hint("pass --kernel and --initrd to boot one"),
        ),
    }

    let accel = qemu::accelerator(arch);
    if accel == "tcg" {
        checks.fail(
            "acceleration",
            Error::new(
                Code::NoAcceleration,
                "software emulation only, so a boot takes minutes",
            )
            .hint(acceleration_hint()),
        );
    } else {
        checks.ok("acceleration", accel);
    }

    match writable(&paths.data) {
        Ok(()) => checks.ok("data", &paths.data.display().to_string()),
        Err(err) => checks.fail(
            "data",
            Error::io(format!("{} is not writable: {err}", paths.data.display()))
                .hint("check the permissions on the data directory"),
        ),
    }

    let settings = match Settings::load(&paths.data) {
        Ok(settings) => {
            checks.ok("settings", &settings.path().display().to_string());
            Some(settings)
        }
        Err(err) => {
            checks.fail(
                "settings",
                Error::io(format!("{err:#}")).hint("move the settings file aside to start over"),
            );
            None
        }
    };

    let image = settings
        .as_ref()
        .and_then(|settings| settings.disk().map(Path::to_path_buf));
    match &image {
        None => checks.skip(
            "image",
            "none chosen yet; the window asks, and start allocates one",
        ),
        Some(image) => match std::fs::metadata(image) {
            Ok(meta) if meta.is_file() => checks.ok(
                "image",
                &format!("{} ({})", image.display(), output::bytes(meta.len())),
            ),
            _ => checks.fail(
                "image",
                Error::new(
                    Code::DiskMissing,
                    format!("{} is not there", image.display()),
                )
                .hint("open or create one in the window, or start with --image"),
            ),
        },
    }

    let registry = discovery::list();
    match &registry {
        Ok(instances) if instances.is_empty() => checks.ok("registry", "no emulator running"),
        Ok(instances) => checks.ok("registry", &format!("{} running", instances.len())),
        Err(err) => checks.fail(
            "registry",
            Error::new(Code::RegistryUnreachable, format!("{err:#}"))
                .hint("another program may be holding the emulator registry's port"),
        ),
    }

    match HostPort::reserve() {
        Ok(port) => checks.ok("ports", &format!("{} free", port.port())),
        Err(err) => checks.fail(
            "ports",
            Error::new(Code::PortExhausted, format!("{err:#}"))
                .hint("stop an emulator, or pass --port to choose one"),
        ),
    }

    let document = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "firmware": firmware.as_ref().map(|_| json!({ "version": firmware_version })),
        "qemu": {
            "binary": emulator.binary.display().to_string(),
            "version": qemu_version,
            "bundled": emulator.bundled,
        },
        "accel": accel,
        "arch": arch.name(),
        "data_dir": paths.data.display().to_string(),
        "settings": settings.as_ref().map(|settings| settings.path().display().to_string()),
        "disk": image.as_ref().map(|image| image.display().to_string()),
        "logs_dir": diagnostics::logs_dir(&paths.data).display().to_string(),
        "registry": {
            "reachable": discovery::answering(REGISTRY_PORT),
            "instances": registry.as_ref().map_or(0, Vec::len),
        },
        "checks": checks.rows,
    });
    output.checklist(&document, &checks.rows)?;
    match checks.failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The diagnostics of one run, in the order they ran, and the first failure
/// among them, which is what the command exits with.
#[derive(Default)]
struct Checks {
    /// Every check as the document carries it.
    rows: Vec<Value>,

    /// The earliest failure, kept while the later checks still run.
    failure: Option<Error>,
}

impl Checks {
    /// Looks up the newest release afresh. A newer one is a warn and a failed
    /// lookup a skip, so neither sets the exit code.
    fn update(&mut self, timeout: u64) {
        // Under CI nothing is looked up
        if update::disabled() {
            self.skip("update", "CI is set");
            return;
        }

        // Look up within --timeout, keeping the answer for later commands
        let running = update::running();
        let result = update::refresh(
            update::directory().as_deref(),
            time::OffsetDateTime::now_utc(),
            std::time::Duration::from_secs(timeout),
        );

        // An unpublished local build newer than the release is current too
        match result {
            Ok(newest) if newest.cmp_precedence(&running).is_gt() => self.warn(
                "update",
                &format!("Ark Emulator {newest} is available, this is {running}"),
                &update::hint(),
            ),
            Ok(_) => self.ok(
                "update",
                &format!("Ark Emulator {running} is the newest release"),
            ),
            Err(error) => self.skip("update", error),
        }
    }

    /// Record a check that passed, with what it saw.
    fn ok(&mut self, name: &str, detail: &str) {
        self.add(name, "ok", detail, None);
    }

    /// Record something to act on with its hint, without failing the command.
    fn warn(&mut self, name: &str, detail: &str, hint: &str) {
        self.add(name, "warn", detail, Some(hint));
    }

    /// Record a check that could not run, without failing the command.
    fn skip(&mut self, name: &str, detail: &str) {
        self.add(name, "skip", detail, None);
    }

    /// Record a failure with its first hint, keeping the earliest as the exit.
    fn fail(&mut self, name: &str, error: Error) {
        self.add(
            name,
            "fail",
            &error.message,
            error.hints.first().map(String::as_str),
        );
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }

    /// Append one check.
    fn add(&mut self, name: &str, result: &str, detail: &str, hint: Option<&str>) {
        self.rows
            .push(json!({"name": name, "result": result, "detail": detail, "hint": hint}));
    }
}

/// What to do about a computer without hardware acceleration, by platform.
fn acceleration_hint() -> &'static str {
    match std::env::consts::OS {
        "linux" => "add your user to the kvm group and log in again",
        "macos" => {
            "Hypervisor.framework is unavailable, which is usual inside another virtual machine"
        }
        "windows" => "enable Windows Hypervisor Platform, then reboot",
        _ => "this platform has no hardware acceleration",
    }
}

/// Whether a directory can be written to, proven by writing to it.
fn writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".doctor-{}", std::process::id()));
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)
}
