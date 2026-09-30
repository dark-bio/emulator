// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Prepares and runs one emulator without depending on a window system.
//!
//! The runtime owns hardware I/O while QEMU owns the disk lock. Control and
//! compatibility registry stops, signals and window closure share shutdown.
//! Orphan protection takes QEMU down even when the launcher cannot run cleanup.

pub(crate) mod disk;
pub(crate) mod hardware;
pub(crate) mod qemu;

use std::io::{BufRead as _, BufReader};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};

use crate::bundle::{Firmware, Paths, resolve_firmware, resolve_qemu_libs};
use crate::cli::args::Boot;
use crate::diagnostics::{self, log};
use crate::ipc::{control, discovery, registry};
use crate::platform;
use crate::settings::{DEFAULT_ENV, DEFAULT_MEMORY, Settings};
use disk::Resolved;
use hardware::Controller;
use qemu::{GuestArch, HostPort, spawn_qemu};

/// Prevents a requested shutdown from being reported as a QEMU crash.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Maximum delay before noticing that QEMU exited between heartbeats.
const CHILD_POLL: Duration = Duration::from_millis(100);

/// Whether shutdown has been requested.
pub(crate) fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// Withdraw the device and exit with its lifecycle outcome.
pub(crate) fn shut_down(code: i32) -> ! {
    if STOPPING.swap(true, Ordering::SeqCst) {
        loop {
            thread::park();
        }
    }
    discovery::deregister();
    std::process::exit(code);
}

/// Resources held until the user or an unattended launch starts the guest.
pub(crate) struct Pending {
    /// Explicit settings for this launch.
    pub(crate) boot: Boot,
    /// Architecture of the firmware and QEMU.
    pub(crate) arch: GuestArch,
    /// Reserved host port, released as QEMU starts.
    pub(crate) host_port: HostPort,
    /// Kernel and initramfs to boot.
    pub(crate) firmware: Firmware,
    /// Bundled QEMU libraries, if present.
    pub(crate) qemu_libs: Option<PathBuf>,
}

/// Resolve guest settings with explicit options ahead of remembered values.
pub(crate) fn effective(boot: Option<&Boot>, settings: &Settings) -> (u32, String) {
    let memory = boot
        .and_then(|boot| boot.memory)
        .or_else(|| settings.memory())
        .unwrap_or(DEFAULT_MEMORY);
    let env = boot
        .and_then(|boot| boot.env.clone())
        .or_else(|| settings.env().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_ENV.to_owned());
    (memory, env)
}

/// Settle boot resources and select a disk without opening any interface.
pub(crate) fn prepare(
    paths: &Paths,
    boot: Boot,
    no_input: bool,
) -> Result<(Pending, Settings, Resolved)> {
    let arch = match boot.arch.or_else(GuestArch::native) {
        Some(arch) => arch,
        None => bail!(
            "no firmware exists for {} hosts; pass --arch explicitly",
            std::env::consts::ARCH
        ),
    };
    diagnostics::record("Guest", arch.name());
    let host_port = match boot.port {
        Some(port) => HostPort::fixed(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
        None => HostPort::reserve()?,
    };
    diagnostics::record("Host address", host_port.addr().to_string());
    let data_dir = &paths.data;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("could not create the data directory {}", data_dir.display()))?;
    if let Err(err) = diagnostics::log_to(data_dir, host_port.port()) {
        log!("[launcher] could not open a log file: {err:#}");
    }
    let settings = Settings::load(data_dir)?;
    diagnostics::record_path("Settings", settings.path());
    registry::host(discovery::CLIENT.address).context("could not host the emulator registry")?;
    let booted = discovery::CLIENT
        .list()
        .context("could not discover running emulators")?;
    let firmware = resolve_firmware(paths.resources.as_deref(), &boot, arch)?;
    diagnostics::record_path("Kernel", &firmware.kernel);
    diagnostics::record_path("Initrd", &firmware.initrd);
    let qemu_libs = resolve_qemu_libs(paths.resources.as_deref());
    let resolved = if boot.headless {
        let image = disk::select(boot.image.as_deref(), settings.disk(), data_dir)?;
        disk::require_available(&image)?;
        Resolved::Boot(image)
    } else {
        disk::decide(
            boot.image.as_deref(),
            settings.disk(),
            settings.autostart(),
            &booted,
            data_dir,
            host_port.port(),
            no_input,
        )?
    };
    Ok((
        Pending {
            boot,
            arch,
            host_port,
            firmware,
            qemu_libs,
        },
        settings,
        resolved,
    ))
}

/// Hardware state and lifecycle shared by either launch mode.
pub(crate) struct Runtime {
    /// The hardware state shown by an optional frontend.
    pub(crate) hardware: Controller,
}

impl Runtime {
    /// Install lifecycle handling before starting QEMU.
    pub(crate) fn new() -> Result<Self> {
        platform::install_shutdown()?;
        Ok(Self {
            hardware: Controller::default(),
        })
    }

    /// Start QEMU, its hardware connection, logging and registry publication.
    pub(crate) fn launch(
        &self,
        mut pending: Pending,
        disk: &Path,
        memory: u32,
        env: &str,
        exited: impl FnOnce(Result<ExitStatus>) + Send + 'static,
    ) -> Result<()> {
        diagnostics::record_path("Disk", disk);
        let control = control::Control::start(self.hardware.clone(), || shut_down(0))?;
        let mut child = spawn_qemu(
            pending.arch,
            &pending.firmware,
            disk,
            memory,
            env,
            pending.qemu_libs.as_deref(),
            &mut pending.host_port,
        )?;
        disk::mark_booted(disk);
        self.hardware.start(pending.host_port.addr());
        if let Err(err) = discovery::register(
            pending.host_port.port(),
            disk,
            self.hardware.clone(),
            control.endpoint.clone(),
        ) {
            // A refused registration must not leave an undiscoverable guest running
            stop_child(&mut child);
            self.hardware.stop();
            discovery::deregister();
            return Err(err);
        }
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(line) => log!("[qemu] {line}"),
                        Err(_) => break,
                    }
                }
            });
        }
        let hardware = self.hardware.clone();
        thread::spawn(move || {
            let result = wait(&mut child, &hardware);
            if result.is_err() {
                stop_child(&mut child);
            }
            hardware.stop();
            drop(control);
            discovery::deregister();
            let result = result.and_then(|status| {
                log!("[launcher] QEMU exited with {status}");
                if status.success() || stopping() || platform::interrupted(status).is_some() {
                    Ok(status)
                } else {
                    Err(anyhow!(
                        "the emulated device stopped unexpectedly: QEMU exited with {status}"
                    ))
                }
            });
            exited(result);
        });
        Ok(())
    }
}

/// Wait for QEMU while publishing heartbeats and surfacing registry failures.
fn wait(child: &mut Child, hardware: &Controller) -> Result<ExitStatus> {
    let mut next_beat = Instant::now() + discovery::HEARTBEAT;
    loop {
        // Notice guest exit without waiting for another heartbeat interval
        if let Some(status) = child.try_wait().context("lost track of the QEMU process")? {
            return Ok(status);
        }

        // Publication retries connection loss but returns permanent refusals
        if Instant::now() >= next_beat {
            discovery::publish(hardware)?;
            next_beat = Instant::now() + discovery::HEARTBEAT;
        }
        thread::sleep(CHILD_POLL);
    }
}

/// Stop and reap the guest after a launcher failure, preserving the original error.
fn stop_child(child: &mut Child) {
    if let Err(err) = child.kill() {
        log!("[launcher] could not stop QEMU after a failure: {err}");
        return;
    }
    if let Err(err) = child.wait() {
        log!("[launcher] could not reap QEMU after a failure: {err}");
    }
}
