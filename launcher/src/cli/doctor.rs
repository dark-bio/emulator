// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The report that says what to fix on this computer and in this build.
//!
//! Every check runs and the whole report prints, so a caller sees everything
//! that is wrong at once. The first failure then sets the exit.

use std::path::Path;

use semver::Version;
use serde_json::{Value, json};

use super::output::{self, Output};
use crate::bundle::{self, Paths};
use crate::diagnostics;
use crate::error::{Code, Error};
use crate::ipc::discovery;
use crate::runtime::qemu::{self, GuestArch, HostPort};
use crate::settings::Settings;
use crate::update;

/// Run every check and print the list, then fail with the first failure.
pub(crate) fn doctor(output: &Output, paths: &Paths, timeout: u64) -> Result<(), Error> {
    let mut checks = Checks::default();
    let notice = output.checking("ark-emulator");
    checks.ark_emulator(
        env!("ARK_EMULATOR_VERSION"),
        cfg!(emulator_local),
        update::disabled(),
        || {
            update::refresh(
                update::directory().as_deref(),
                time::OffsetDateTime::now_utc(),
                std::time::Duration::from_secs(timeout),
            )
        },
    );
    drop(notice);
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

    let notice = output.checking("qemu");
    let emulator = qemu::resolve_qemu(arch);
    let qemu_version = emulator.version(libs.as_deref());
    match &qemu_version {
        Some(version) => checks.ok(
            "qemu",
            &format!("{version} at {}", emulator.binary.display()),
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
    checks.shorten_path(&emulator.binary);
    drop(notice);

    let notice = output.checking("firmware");
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

    drop(notice);

    let notice = output.checking("acceleration");
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

    drop(notice);

    let notice = output.checking("data");
    checks.directory("data", &paths.data);
    drop(notice);

    let notice = output.checking("settings");
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
    checks.shorten_path(&paths.data);
    drop(notice);

    let notice = output.checking("image");
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
                &format!("{}, {}", image.display(), output::bytes(meta.len())),
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
    if let Some(image) = &image {
        checks.shorten_path(image);
    }

    drop(notice);

    let notice = output.checking("logs");
    checks.directory("logs", &diagnostics::logs_dir(&paths.data));
    drop(notice);

    let notice = output.checking("registry");
    let mut registry_warnings = Vec::new();
    let registry = discovery::CLIENT.list(|warning| registry_warnings.push(warning));
    match &registry {
        Ok(_) if !registry_warnings.is_empty() => checks.fail(
            "registry",
            Error::new(Code::RegistryUnreachable, registry_warnings.join("; "))
                .hint("update Ark Emulator and restart all running emulators"),
        ),
        Ok(instances) if instances.is_empty() => checks.ok("registry", "no emulator running"),
        Ok(instances) => checks.ok("registry", &format!("{} running", instances.len())),
        Err(err) => checks.fail(
            "registry",
            Error::new(Code::RegistryUnreachable, format!("{err:#}"))
                .hint("another program may be holding the emulator registry's port"),
        ),
    }

    drop(notice);

    let notice = output.checking("ports");
    match HostPort::reserve() {
        Ok(port) => checks.ok("ports", &format!("{} free", port.port())),
        Err(err) => checks.fail(
            "ports",
            Error::new(Code::PortExhausted, format!("{err:#}"))
                .hint("stop an emulator, or pass --port to choose one"),
        ),
    }

    drop(notice);

    let document = json!({
        "version": env!("ARK_EMULATOR_VERSION"),
        "checks": checks.rows,
    });
    output.checklist(&document, &checks.reading_rows)?;
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

    /// The same checks with the home directory as `~`, for the reading view.
    reading_rows: Vec<Value>,

    /// The earliest failure, kept while the later checks still run.
    failure: Option<Error>,
}

impl Checks {
    /// Record the `ark-emulator` check, with this build's version and what the
    /// release lookup found.
    ///
    /// A newer release is a warn and a failed lookup a skip, so neither sets
    /// the exit code.
    fn ark_emulator(
        &mut self,
        version: &str,
        local: bool,
        ci: bool,
        lookup: impl FnOnce() -> Result<Version, &'static str>,
    ) {
        // Local builds and CI skip the release lookup
        if local {
            self.skip(
                "ark-emulator",
                &format!("{version}, not looked up for a local build"),
            );
            return;
        }
        if ci {
            self.skip(
                "ark-emulator",
                &format!("{version}, not looked up under CI"),
            );
            return;
        }

        // Compare the stamped build with the newest release
        let running = Version::parse(version).expect("build version is semver");
        match lookup() {
            Ok(newest) if newest.cmp_precedence(&running).is_gt() => self.warn(
                "ark-emulator",
                &format!("{version}, and {newest} is out"),
                &update::hint(),
            ),
            Ok(_) => self.ok("ark-emulator", &format!("{version}, the newest release")),
            Err(error) => self.skip(
                "ark-emulator",
                &format!("{version}, release lookup failed: {error}"),
            ),
        }
    }

    /// Record whether a directory takes writes, creating it when it is missing.
    fn directory(&mut self, name: &str, path: &Path) {
        match writable(path) {
            Ok(()) => self.ok(name, &path.display().to_string()),
            Err(err) => self.fail(
                name,
                Error::io(format!("{} is not writable: {err}", path.display()))
                    .hint(format!("check the permissions on the {name} directory")),
            ),
        }
        self.shorten_path(path);
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
        let row = json!({"name": name, "result": result, "detail": detail, "hint": hint});
        self.reading_rows.push(row.clone());
        self.rows.push(row);
    }

    /// Show `path` with the home directory as `~` in the latest check's
    /// reading detail.
    fn shorten_path(&mut self, path: &Path) {
        if let Some(row) = self.reading_rows.last_mut()
            && let Some(detail) = row["detail"].as_str()
        {
            row["detail"] =
                json!(detail.replace(&path.display().to_string(), &output::home_path(path)));
        }
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

/// Tests of release states, writable directories and the first failure.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::style::{Color, Theme};
    use tempfile::TempDir;

    /// The `ark-emulator` check reads right in every state, with no network
    /// and no build cfg.
    #[test]
    fn test_ark_emulator_release_states() {
        for (case, version, local, ci, newest, result, detail, mark, summary) in [
            (
                "newest release",
                "0.3.1",
                false,
                false,
                Ok("0.3.1"),
                "ok",
                "0.3.1, the newest release",
                "✓",
                "No problems found",
            ),
            (
                "newer release",
                "0.3.1",
                false,
                false,
                Ok("0.3.2"),
                "warn",
                "0.3.1, and 0.3.2 is out",
                "!",
                "1 warning",
            ),
            (
                "older release",
                "0.3.1",
                false,
                false,
                Ok("0.3.0"),
                "ok",
                "0.3.1, the newest release",
                "✓",
                "No problems found",
            ),
            (
                "metadata precedence",
                "0.3.1+build.1",
                false,
                false,
                Ok("0.3.1+build.2"),
                "ok",
                "0.3.1+build.1, the newest release",
                "✓",
                "No problems found",
            ),
            (
                "release candidate",
                "0.3.1-rc.2",
                false,
                false,
                Ok("0.3.1"),
                "warn",
                "0.3.1-rc.2, and 0.3.1 is out",
                "!",
                "1 warning",
            ),
            (
                "lookup failure",
                "0.3.1",
                false,
                false,
                Err("GitHub could not be reached"),
                "skip",
                "0.3.1, release lookup failed: GitHub could not be reached",
                "·",
                "No problems found",
            ),
            (
                "CI",
                "0.3.1",
                false,
                true,
                Ok("0.3.2"),
                "skip",
                "0.3.1, not looked up under CI",
                "·",
                "No problems found",
            ),
            (
                "local build",
                "0.3.1-local",
                true,
                false,
                Ok("0.3.2"),
                "skip",
                "0.3.1-local, not looked up for a local build",
                "·",
                "No problems found",
            ),
            (
                "local build under CI",
                "0.3.1-local",
                true,
                true,
                Ok("0.3.2"),
                "skip",
                "0.3.1-local, not looked up for a local build",
                "·",
                "No problems found",
            ),
        ] {
            // Answer the lookup directly, noting whether the check asked
            let mut checks = Checks::default();
            let mut called = false;
            checks.ark_emulator(version, local, ci, || {
                called = true;
                newest.map(|version| Version::parse(version).unwrap())
            });
            assert_eq!(called, !local && !ci, "{case}");
            assert!(checks.failure.is_none(), "{case}");

            // A test executable has no recognized install method
            let hint = (result == "warn")
                .then_some("download it from https://github.com/dark-bio/emulator");
            assert_eq!(
                checks.rows,
                vec![json!({"name":"ark-emulator", "result":result, "detail":detail, "hint":hint})],
                "{case}"
            );
            let hint = hint.map_or(String::new(), |hint| format!("\n    hint: {hint}"));
            assert_eq!(
                output::checklist(&Theme::fixed(120, Color::Off, true), &checks.reading_rows),
                format!("This computer\n  {mark} ark-emulator  {detail}{hint}\n\n{summary}"),
                "{case}"
            );
        }
    }

    /// Data and logs are created when absent and fail when a file blocks them.
    #[test]
    fn test_directory_checks_probe_data_and_logs() {
        let temporary = TempDir::new().unwrap();
        for name in ["data", "logs"] {
            let path = temporary.path().join(name);
            for blocked in [false, true] {
                // A file in the directory's place prevents writes on every platform
                if blocked {
                    std::fs::remove_dir(&path).unwrap();
                    std::fs::write(&path, b"blocked").unwrap();
                }
                let mut checks = Checks::default();
                checks.directory(name, &path);
                let row = &checks.rows[0];
                assert_eq!(row["name"], name, "{name}, blocked={blocked}");
                assert_eq!(
                    row["result"],
                    if blocked { "fail" } else { "ok" },
                    "{name}, blocked={blocked}"
                );

                // The check keeps the full path, and the failure its hint
                let rendered =
                    output::checklist(&Theme::fixed(240, Color::Off, true), &checks.reading_rows);
                if blocked {
                    assert!(
                        row["detail"]
                            .as_str()
                            .unwrap()
                            .starts_with(&format!("{} is not writable: ", path.display())),
                        "{name}: {row}"
                    );
                    assert_eq!(
                        row["hint"],
                        format!("check the permissions on the {name} directory"),
                        "{name}"
                    );
                    let failure = checks.failure.as_ref().unwrap();
                    assert_eq!(failure.code, Code::Io, "{name}");
                    assert!(
                        rendered.ends_with(&format!(
                            "\n    hint: check the permissions on the {name} directory\n\n1 problem"
                        )),
                        "{name}: {rendered}"
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), b"blocked", "{name}");
                } else {
                    assert_eq!(row["detail"], path.display().to_string(), "{name}");
                    assert!(row["hint"].is_null(), "{name}");
                    assert!(checks.failure.is_none(), "{name}");
                    assert_eq!(
                        rendered,
                        format!(
                            "This computer\n  ✓ {name}  {}\n\nNo problems found",
                            path.display()
                        ),
                        "{name}"
                    );
                    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0, "{name}");
                }
            }
        }
    }

    /// A later failure keeps its row, and the first one stays the error.
    #[test]
    fn test_first_failure_survives_later_checks() {
        let mut checks = Checks::default();
        checks.fail(
            "qemu",
            Error::new(Code::QemuMissing, "qemu could not be run")
                .hint("install QEMU")
                .hint("or use a packaged build"),
        );
        checks.fail(
            "logs",
            Error::io("logs are not writable").hint("check permissions"),
        );
        checks.ok("ports", "18181 free");

        // The whole report comes before the first error and its hints
        assert_eq!(
            output::checklist(&Theme::fixed(80, Color::Off, true), &checks.reading_rows),
            "This computer\n  ✗ qemu   qemu could not be run\n    hint: install QEMU\n  ✗ logs   logs are not writable\n    hint: check permissions\n  ✓ ports  18181 free\n\n2 problems",
            "later rows survive"
        );
        let failure = checks.failure.unwrap();
        assert_eq!(
            failure.code,
            Code::QemuMissing,
            "first failure sets the exit"
        );
        assert_eq!(failure.message, "qemu could not be run", "first message");
        assert_eq!(
            failure.hints,
            ["install QEMU", "or use a packaged build"],
            "all first-error hints survive"
        );
    }
}
