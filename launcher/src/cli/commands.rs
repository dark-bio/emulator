// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! What the command line does with an emulator: boot one, find them, stop one,
//! wipe an image, and say what this build is made of.
//!
//! None of it opens a window of its own. A command finds emulators through
//! the registry, asks one to stop through the same registry, and boots a new
//! one by starting this executable again with the image and the port it
//! settled on. That process owns its QEMU and shows the device face.
//!
//! Nothing here speaks to the firmware. Everything about an emulated Ark's
//! identity, pairing and data belongs to `ark`.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tauri::PackageInfo;

use super::args::{Boot, ButtonAction, Command, Global};
use super::output::{self, Output};
use crate::bundle::{self, Paths};
use crate::diagnostics::{self, Sink};
use crate::error::{Code, Error};
use crate::ipc::{control, discovery, registry::Instance};
use crate::platform;
use crate::runtime::disk;
use crate::runtime::qemu::{self, GuestArch, HostPort};
use crate::settings::Settings;

/// How often a wait asks the registry again. Short enough that a boot which
/// takes ten seconds is reported as soon as it happens.
const POLL: Duration = Duration::from_millis(250);

/// How much of a failed launcher's log an error carries, in lines. Enough to
/// hold what QEMU said about why it would not start.
const TAIL: usize = 20;

/// How long a loopback port is given to answer. Nothing on this computer is
/// far enough away to need more.
const DIAL: Duration = Duration::from_millis(500);

/// Run one command, print what it answers, and hand back the exit code. No
/// command at all is the root's own answer, which is what `--version` asks
/// for, since a bare run opens a window instead of coming through here.
pub(crate) fn run(
    command: Option<Command>,
    global: &Global,
    output: &Output,
    identifier: &str,
    package: &PackageInfo,
) -> i32 {
    diagnostics::log_sink(match global.log {
        Some(_) => Sink::Events(output.clone()),
        None => Sink::Quiet,
    });

    // Valid management commands start with the kept release note
    if matches!(
        command,
        Some(
            Command::Start { .. }
                | Command::List
                | Command::Stop { .. }
                | Command::Button { .. }
                | Command::Wipe { .. }
        )
    ) {
        crate::update::start(output, time::OffsetDateTime::now_utc());
    }

    match dispatch(command, global, output, identifier, package) {
        Ok(()) => 0,
        Err(error) => {
            output.error(&error);
            error.exit()
        }
    }
}

/// Send one command to the code that serves it.
fn dispatch(
    command: Option<Command>,
    global: &Global,
    output: &Output,
    identifier: &str,
    package: &PackageInfo,
) -> Result<(), Error> {
    // Answered before anything is resolved, since a reader asking what a
    // command does may be on a computer where nothing else would work.
    match command {
        Some(Command::Help { name, all }) => return super::help::run(name.as_slice(), all, true),
        Some(Command::Completions { shell }) => return completions(shell, output),
        _ => {}
    }
    let paths = Paths::resolve(identifier, package).map_err(|err| Error::io(format!("{err:#}")))?;
    match command {
        Some(Command::Start { boot }) => start(&boot, global, output, &paths),
        Some(Command::List) => list(output, &paths),
        Some(Command::Stop { emulator, all }) => stop(emulator.as_deref(), all, global, output),
        Some(Command::Button { action }) => button(action, global, output),
        Some(Command::Wipe { path, yes }) => wipe(path.as_deref(), yes, output, &paths),
        Some(Command::Doctor) => super::doctor::doctor(output, &paths, global.timeout),
        Some(Command::Completions { .. } | Command::Help { .. }) => {
            unreachable!("answered above")
        }
        None => version(output, &paths),
    }
}

/// Write a shell's completion script to stdout. It is text whatever the run
/// asked for, so stdout is released to it before anything else could claim
/// the result.
fn completions(shell: clap_complete::Shell, output: &Output) -> Result<(), Error> {
    output.release_stdout();
    let mut script = Vec::new();
    clap_complete::generate(
        shell,
        &mut super::help::parser(),
        "ark-emulator",
        &mut script,
    );
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&script)
        .and_then(|()| stdout.flush())
        .map_err(|err| Error::io(format!("could not write the completions: {err}")))
}

/// What this build is made of, which is what a bug report needs naming.
fn version(output: &Output, paths: &Paths) -> Result<(), Error> {
    let arch = architecture(None)?;
    let libs = bundle::resolve_qemu_libs(paths.resources.as_deref());
    output.block(
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "firmware": bundle::firmware_version(paths.resources.as_deref(), arch),
            "qemu": qemu::resolve_qemu(arch).version(libs.as_deref()),
        }),
        &[
            ("Version", "version"),
            ("Firmware", "firmware"),
            ("QEMU", "qemu"),
        ],
    )
}

/// Boot an emulator on the image this run settles on, and answer once the
/// firmware accepts clients. An emulator already holding that image is waited
/// for and reported rather than refused, since a start states a goal.
///
/// The emulator runs in a process of its own, which is the process that owns
/// its window and its QEMU. This command hands it the image and the port so
/// that both sides name the same device, then watches the registry for it.
fn start(boot: &Boot, global: &Global, output: &Output, paths: &Paths) -> Result<(), Error> {
    let arch = architecture(boot.arch)?;
    let settings = Settings::load(&paths.data).map_err(|err| Error::io(format!("{err:#}")))?;
    let image = disk::select(boot.image.as_deref(), settings.disk(), &paths.data)
        .map_err(|err| Error::io(format!("{err:#}")))?;

    if let Some(instance) = discovery::booted(&listing()?, &image) {
        output.event(
            "note",
            format!("{} is already booted", disk::name_of(&image)),
        );
        let wait = Wait::new(&image, instance.clone(), false, false);
        let instance = wait.ready(None, global, output, paths)?;
        warn_environment(output, boot, &instance);
        return report(output, paths, &instance, &image, false, false);
    }

    let reservation = match boot.port {
        Some(port) => HostPort::fixed(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
        None => HostPort::reserve().map_err(|err| {
            Error::new(Code::PortExhausted, format!("{err:#}"))
                .hint("pass `--host-addr` to choose the port yourself")
        })?,
    };
    let address = reservation.addr();
    output.event("step", format!("holding port {}", address.port()));

    // The emulator writes its own log, named by the port it was handed, so a
    // failed boot has something to quote and a caller has somewhere to look.
    // Emptied first, so nothing an earlier launcher on this port left there is
    // read back as this one's.
    let log = diagnostics::log_path(&paths.data, address.port());
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::File::create(&log);

    let created = !image.exists();
    output.event("step", format!("booting {}", image.display()));
    let child = spawn(boot, arch, &image, address).map_err(|err| Error::io(format!("{err:#}")))?;
    // Held until the child exists, so two starts issued together cannot both
    // settle on this port. The moment between here and the child's QEMU
    // binding it remains, and a collision there still fails loudly.
    drop(reservation);

    let unready = Instance {
        control: None,
        port: address.port(),
        disk: disk::name_of(&image),
        disk_id: discovery::disk_id(&image),
        ready: false,
        env: None,
        name: None,
        serial: None,
        expiry: None,
    };
    let wait = Wait::new(&image, unready, created, true);
    let instance = wait.ready(Some((child, &log)), global, output, paths)?;
    warn_environment(output, boot, &instance);
    report(output, paths, &instance, &image, created, true)
}

/// A start's wait for the registry to report its emulator ready.
struct Wait<'a> {
    /// The image the emulator holds, which is how its entry is found.
    image: &'a Path,

    /// The entry as last seen, which is the partial result on a timeout.
    last: Instance,

    /// Whether this run created the image.
    created: bool,

    /// Whether this run booted the emulator.
    started: bool,
}

impl<'a> Wait<'a> {
    /// A wait for the emulator holding `image`, starting from what is known.
    fn new(image: &'a Path, last: Instance, created: bool, started: bool) -> Self {
        Self {
            image,
            last,
            created,
            started,
        }
    }

    /// Watch the registry until the emulator is ready or the deadline passes.
    /// A child this run started is watched too, since a launcher that died
    /// leaves no entry to wait for. On the deadline the unready entry is
    /// printed as the partial result before the failure.
    fn ready(
        mut self,
        mut child: Option<(Child, &Path)>,
        global: &Global,
        output: &Output,
        paths: &Paths,
    ) -> Result<Instance, Error> {
        let followed = child
            .as_ref()
            .filter(|_| global.log.is_some())
            .map(|(_, log)| *log);
        let mut relay = Relay::new(followed);
        let deadline = Instant::now() + Duration::from_secs(global.timeout);
        loop {
            relay.pump(output);
            if let Some((child, log)) = &mut child
                && let Ok(Some(status)) = child.try_wait()
            {
                relay.pump(output);
                return Err(Error::new(
                    Code::StoppedUnexpectedly,
                    format!("the emulator exited with {status}\n{}", tail(log)),
                )
                .hint(format!("its log is at {}", log.display())));
            }
            if let Some(instance) = discovery::booted(&listing()?, self.image) {
                if instance.ready {
                    return Ok(instance.clone());
                }
                self.last = instance.clone();
            }
            if Instant::now() >= deadline {
                report(
                    output,
                    paths,
                    &self.last,
                    self.image,
                    self.created,
                    self.started,
                )?;
                return Err(Error::new(
                    Code::Timeout,
                    format!(
                        "the device was not ready within {} s and is still booting",
                        global.timeout
                    ),
                )
                .hint("watch `ark-emulator list` for it to become ready"));
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Show every emulator this computer is running.
fn list(output: &Output, paths: &Paths) -> Result<(), Error> {
    let mut instances = listing()?;
    instances.sort_by_key(|instance| instance.port);
    let rows: Vec<Value> = instances
        .iter()
        .map(|instance| row(paths, instance))
        .collect();
    output.table(
        &json!({ "emulators": rows }),
        &rows,
        &[
            ("LOCATOR", "locator"),
            ("IMAGE", "image"),
            ("READY", "ready"),
            ("ENV", "environment"),
            ("NAME", "name"),
            ("SERIAL", "serial"),
            ("EXPIRES", "expires"),
        ],
    )
}

/// Shut one emulator down, or every one of them. The emulator is named the
/// way `ark -d` names it, and the only one running needs no name. The
/// locators that went are the result, preserved as a partial result if a later
/// operation fails or the wait times out.
fn stop(selector: Option<&str>, all: bool, global: &Global, output: &Output) -> Result<(), Error> {
    let running = listing()?;
    let mut targets = if all {
        running
    } else {
        pick(&running, selector)?
    };
    targets.sort_by_key(|instance| instance.port);
    if targets.is_empty() {
        output.event("note", "no emulators are running");
    }
    for instance in &targets {
        output.event("step", format!("asking {} to stop", locator(instance)));
    }

    let mut progress = Stopping {
        pending: targets.iter().map(|instance| instance.port).collect(),
        done: Vec::new(),
        asked: None,
    };
    let deadline = Instant::now() + Duration::from_secs(global.timeout);
    while !progress.pending.is_empty() {
        // Preserve confirmed stops when a later request or listing is refused
        match progress.poll(discovery::CLIENT, Instant::now()) {
            Ok(true) => break,
            Ok(false) => {}
            Err(err) => {
                if !progress.done.is_empty() {
                    output.block(&stopped(&progress.done), &[("Stopped", "stopped")])?;
                }
                return Err(registry_error(err));
            }
        }
        if Instant::now() >= deadline {
            output.block(&stopped(&progress.done), &[("Stopped", "stopped")])?;
            return Err(Error::new(
                Code::Timeout,
                format!(
                    "emulator:{} was still up after {} s",
                    progress.pending[0], global.timeout
                ),
            )
            .hint("close its window, or interrupt its foreground headless process"));
        }
        std::thread::sleep(POLL);
    }
    output.block(&stopped(&progress.done), &[("Stopped", "stopped")])
}

/// Confirmed and pending stops across registry-host handovers.
struct Stopping {
    /// Emulator ports whose shutdown has not yet been confirmed.
    pending: Vec<u16>,
    /// Emulator ports absent from both discovery and the loopback listener.
    done: Vec<u16>,
    /// Last round of stop requests, limiting retries to the heartbeat interval.
    asked: Option<Instant>,
}

impl Stopping {
    /// Advance shutdown once, retaining partial progress on a permanent failure.
    fn poll(
        &mut self,
        client: discovery::Client,
        now: Instant,
    ) -> Result<bool, discovery::Failure> {
        // A replacement host can lose queued stops until launchers republish
        if self
            .asked
            .is_none_or(|at| now.duration_since(at) >= discovery::HEARTBEAT)
        {
            for port in &self.pending {
                match client.request_stop(*port) {
                    Ok(()) => {}
                    Err(err) if err.retryable() => {}
                    Err(discovery::Failure::Http { status: 404, .. }) => {}
                    Err(err) => return Err(err),
                }
            }
            self.asked = Some(now);
        }

        // Only a valid listing and a closed guest port together confirm shutdown
        let listed = match client.list() {
            Ok(instances) => instances,
            Err(err) if err.retryable() => return Ok(false),
            Err(err) => return Err(err),
        };
        let (gone, held): (Vec<u16>, Vec<u16>) = self.pending.iter().partition(|port| {
            !listed.iter().any(|instance| instance.port == **port) && !answering(**port)
        });
        self.done.extend(gone);
        self.pending = held;
        Ok(self.pending.is_empty())
    }
}

/// Set this emulator's CLI hold and report the state after hardware delivery.
fn button(action: ButtonAction, global: &Global, output: &Output) -> Result<(), Error> {
    // Select the same running emulator for either kind of button input
    let (pressed, selector, release_after) = match action {
        ButtonAction::Press {
            emulator,
            release_after,
        } => (true, emulator, release_after),
        ButtonAction::Release { emulator } => (false, emulator, None),
    };
    let targets = pick(&listing()?, selector.as_deref())?;
    let instance = &targets[0];
    let endpoint = instance.control.as_ref().ok_or_else(|| {
        Error::new(
            Code::ControlUnsupported,
            "this emulator does not advertise button control",
        )
        .hint("update Ark Emulator and restart all running emulators, including the registry host")
    })?;

    // The launcher acknowledges the timer together with the hardware write
    let outcome = control::button(
        endpoint,
        pressed,
        release_after,
        Duration::from_secs(global.timeout),
    )?;
    if !outcome.changed {
        output.event(
            "note",
            if pressed {
                "the CLI already holds the button"
            } else {
                "the CLI hold was already released"
            },
        );
    }
    if !outcome.cli_pressed && outcome.pressed {
        output.event("note", "the window still holds the button");
    }

    // Report the accepted schedule; the command does not wait for its expiry
    output.block(
        &json!({
            "locator": locator(instance), "pressed": outcome.pressed,
            "cli_pressed": outcome.cli_pressed, "changed": outcome.changed,
            "release_after_seconds": outcome.release_after_seconds,
        }),
        &[
            ("Locator", "locator"),
            ("Pressed", "pressed"),
            ("CLI held", "cli_pressed"),
            ("Changed", "changed"),
            ("Release after", "release_after_seconds"),
        ],
    )
}

/// Reset a stopped image to a fresh device, so that its next boot needs
/// enrolling, pairing and unlocking again. The file stays where it is, so
/// the window and `start` find it afterwards. The registry says which
/// emulator holds an image, and QEMU's own lock on the file says whether one
/// still does when the registry is between hosts.
fn wipe(path: Option<&Path>, yes: bool, output: &Output, paths: &Paths) -> Result<(), Error> {
    let local = |err: anyhow::Error| Error::io(format!("{err:#}"));
    let path = match path {
        Some(path) => disk::settle(path).map_err(local)?,
        None => {
            let settings = Settings::load(&paths.data).map_err(local)?;
            disk::select(None, settings.disk(), &paths.data).map_err(local)?
        }
    };
    let size = std::fs::metadata(&path)
        .map_err(|err| {
            Error::new(
                Code::DiskMissing,
                format!("could not read {}: {err}", path.display()),
            )
        })?
        .len();

    if let Some(instance) = discovery::booted(&listing()?, &path) {
        return Err(Error::new(
            Code::DiskBusy,
            format!("{} is booted by {}", path.display(), locator(instance)),
        )
        .hint(format!(
            "stop it first with `ark-emulator stop {}`",
            locator(instance)
        )));
    }
    let libs = bundle::resolve_qemu_libs(paths.resources.as_deref());
    if qemu::image_in_use(&path, libs.as_deref()) {
        return Err(Error::new(
            Code::DiskBusy,
            format!("{} is held by a running emulator", path.display()),
        )
        .hint("stop it first, or close its window"));
    }

    if !yes
        && !output.confirm(
            &format!("Wipe {}, {}?", path.display(), output::bytes(size)),
            &format!("wiping {} was not confirmed", path.display()),
            "--yes",
        )?
    {
        return Err(Error::new(
            Code::ConfirmationRequired,
            format!("{} was left alone", path.display()),
        ));
    }

    qemu::create_disk(&path, libs.as_deref())
        .map_err(|err| Error::io(format!("could not reset {}: {err:#}", path.display())))?;
    output.block(
        &json!({"path": path.display().to_string(), "wiped": true}),
        &[("Path", "path"), ("Wiped", "wiped")],
    )
}

/// What the emulators running on this computer look like. A registry that
/// answers and cannot be read is a failure; none at all is an empty list.
fn listing() -> Result<Vec<Instance>, Error> {
    discovery::CLIENT.list().map_err(registry_error)
}

/// Report a registry failure with its server explanation and a recovery hint.
fn registry_error(err: discovery::Failure) -> Error {
    let hint = if matches!(err, discovery::Failure::Http { status: 403, .. }) {
        "update Ark Emulator and restart all running launchers"
    } else {
        "check `ark-emulator list`; another program may be holding the registry's port"
    };
    Error::new(Code::RegistryUnreachable, err.to_string()).hint(hint)
}

/// Whether something accepts connections on a loopback port, which QEMU does
/// for as long as the emulator holding it lives.
fn answering(port: u16) -> bool {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    TcpStream::connect_timeout(&address.into(), DIAL).is_ok()
}

/// One emulator as both outputs carry it, under the names `ark devices` uses
/// for the same facts. The log file is worked out here rather than read from
/// the registry, which publishes no paths.
fn row(paths: &Paths, instance: &Instance) -> Value {
    json!({
        "locator": locator(instance),
        "port": instance.port,
        "image": instance.disk,
        "ready": instance.ready,
        "environment": instance.env,
        "name": instance.name,
        "serial": instance.serial,
        "expires": instance.expiry.and_then(iso8601),
        "log": diagnostics::log_path(&paths.data, instance.port).display().to_string(),
    })
}

/// The locator a running emulator is addressed by, in this tool and in `ark`.
fn locator(instance: &Instance) -> String {
    format!("emulator:{}", instance.port)
}

/// The result of a stop: the locators of the emulators that went.
fn stopped(ports: &[u16]) -> Value {
    let mut ports = ports.to_vec();
    ports.sort_unstable();
    let locators: Vec<String> = ports
        .iter()
        .map(|port| format!("emulator:{port}"))
        .collect();
    json!({ "stopped": locators })
}

/// The running emulators a selector names, as `ark -d` reads one: an exact
/// locator, a unique serial, a unique name or a unique image basename. The
/// bare word `emulator`, like no selector at all, means the only one running.
fn pick(running: &[Instance], selector: Option<&str>) -> Result<Vec<Instance>, Error> {
    let listed =
        |instances: &[Instance]| instances.iter().map(locator).collect::<Vec<_>>().join(", ");
    let Some(selector) = selector.filter(|selector| *selector != "emulator") else {
        return match running {
            [] => Err(Error::new(Code::NoEmulator, "no emulator is running")
                .hint("`ark-emulator start` boots one")),
            [only] => Ok(vec![only.clone()]),
            _ => Err(Error::new(
                Code::AmbiguousEmulator,
                format!("several emulators are running: {}", listed(running)),
            )
            .hint("name one by its locator")),
        };
    };
    if let Some(port) = selector.strip_prefix("emulator:") {
        let port = port.parse::<u16>().ok();
        return match running.iter().find(|instance| Some(instance.port) == port) {
            Some(instance) => Ok(vec![instance.clone()]),
            None => Err(Error::new(
                Code::NoEmulator,
                format!("no emulator is running as {selector}"),
            )
            .hint("`ark-emulator list` shows what is")),
        };
    }
    let matches: Vec<&Instance> = running
        .iter()
        .filter(|instance| {
            instance.serial.as_deref() == Some(selector)
                || instance.name.as_deref() == Some(selector)
                || instance.disk == selector
        })
        .collect();
    match matches[..] {
        [] => {
            let mut error = Error::new(
                Code::NoEmulator,
                format!("nothing running matches {selector:?}"),
            );
            if selector.parse::<u16>().is_ok() {
                error = error.hint(format!(
                    "a port is named by its locator, emulator:{selector}"
                ));
            }
            Err(error.hint("`ark-emulator list` shows what is running"))
        }
        [only] => Ok(vec![only.clone()]),
        _ => Err(Error::new(
            Code::AmbiguousEmulator,
            format!(
                "{selector:?} matches several emulators: {}",
                matches
                    .iter()
                    .map(|instance| locator(instance))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .hint("name one by its locator")),
    }
}

/// Print what a start settled on: where the emulator is, which image it holds,
/// and whether this run is what booted it.
fn report(
    output: &Output,
    paths: &Paths,
    instance: &Instance,
    image: &Path,
    created: bool,
    started: bool,
) -> Result<(), Error> {
    let mut document = row(paths, instance);
    document["path"] = json!(image.display().to_string());
    document["created"] = json!(created);
    document["started"] = json!(started);
    output.block(
        &document,
        &[
            ("Locator", "locator"),
            ("Image", "image"),
            ("Created", "created"),
            ("Started", "started"),
            ("Environment", "environment"),
            ("Ready", "ready"),
            ("Expires", "expires"),
        ],
    )
}

/// Say so when the device reports an environment other than the one asked for.
/// The binding is burnt in on an image's first boot and read back from the
/// firmware, so it can only be checked once the device is up.
fn warn_environment(output: &Output, boot: &Boot, instance: &Instance) {
    let (Some(asked), Some(bound)) = (boot.env.as_deref(), instance.env.as_deref()) else {
        return;
    };
    if asked != bound {
        output.event(
            "warning",
            format!("this image is bound to {bound}, not to {asked}"),
        );
    }
}

/// The architecture to boot, which defaults to this computer's own because it
/// is the only one that gets hardware acceleration.
fn architecture(named: Option<GuestArch>) -> Result<GuestArch, Error> {
    named.or_else(GuestArch::native).ok_or_else(|| {
        Error::new(
            Code::FirmwareMissing,
            format!(
                "no firmware exists for {} computers",
                std::env::consts::ARCH
            ),
        )
        .hint("pass `--arch` to name the architecture to boot")
    })
}

/// Start this executable again as the emulator, detached so that a Ctrl-C
/// meant for the wait does not reach the device. Its streams go nowhere; it
/// keeps its own log file.
fn spawn(boot: &Boot, arch: GuestArch, image: &Path, address: SocketAddr) -> anyhow::Result<Child> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--no-input")
        .arg("--image")
        .arg(image)
        .arg("--port")
        .arg(address.port().to_string())
        .arg("--arch")
        .arg(arch.name());
    if boot.headless {
        command.arg("--headless");
    }
    if let Some(env) = &boot.env {
        command.arg("--env").arg(env);
    }
    if let Some(memory) = boot.memory {
        command.arg("--memory").arg(memory.to_string());
    }
    if let (Some(kernel), Some(initrd)) = (&boot.kernel, &boot.initrd) {
        command
            .arg("--kernel")
            .arg(kernel)
            .arg("--initrd")
            .arg(initrd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    platform::detach(&mut command);
    Ok(command.spawn()?)
}

/// A tail of the emulator's log file, which is the same report its error
/// window would have shown.
fn tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL)..].join("\n")
}

/// The emulator's log file as it fills, relayed line by line while a start
/// waits. Nothing is relayed unless diagnostics were asked for.
struct Relay {
    /// The file to read, absent when nobody asked for the lines.
    log: Option<PathBuf>,

    /// How far into it the relay has read.
    read: u64,

    /// The bytes after the last line break, kept until the line completes.
    partial: Vec<u8>,
}

impl Relay {
    /// Follow `log`, from wherever it is now.
    fn new(log: Option<&Path>) -> Self {
        Self {
            log: log.map(Path::to_path_buf),
            read: 0,
            partial: Vec::new(),
        }
    }

    /// Relay every whole line the emulator has written since the last look.
    fn pump(&mut self, output: &Output) {
        let Some(log) = &self.log else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(log) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.read)).is_err() {
            return;
        }
        let mut fresh = Vec::new();
        let Ok(count) = file.read_to_end(&mut fresh) else {
            return;
        };
        self.read += count as u64;
        self.partial.extend(fresh);
        while let Some(end) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            output.event("log", String::from_utf8_lossy(&line).trim_end());
        }
    }
}

/// A Unix timestamp as the UTC instant every Dark Bio tool prints.
fn iso8601(seconds: u64) -> Option<String> {
    let instant = time::OffsetDateTime::from_unix_timestamp(i64::try_from(seconds).ok()?).ok()?;
    instant
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::testing::{registry as peer, response};
    use std::net::TcpListener;
    use tempfile::TempDir;

    /// Permanent stop refusals return on the first poll with the server's reason.
    #[test]
    fn test_stop_reports_a_permanent_refusal_without_retrying() {
        for status in [400, 403, 500] {
            let (client, worker) = peer(vec![(
                "POST",
                response(status, "request refused by test registry"),
            )]);
            let mut progress = Stopping {
                pending: vec![18181],
                done: Vec::new(),
                asked: None,
            };
            let err = progress.poll(client, Instant::now()).unwrap_err();
            assert!(
                matches!(&err, discovery::Failure::Http { status: actual, .. } if *actual == status)
            );
            let err = registry_error(err);
            assert_eq!(err.code, Code::RegistryUnreachable);
            assert_eq!(err.exit(), 3);
            assert!(err.message.contains("request refused by test registry"));
            assert_eq!(progress.pending, [18181]);
            assert!(progress.done.is_empty());
            worker.join().unwrap();
        }
    }

    /// A later refusal preserves shutdowns confirmed by an earlier poll.
    #[test]
    fn test_stop_keeps_confirmed_partial_progress_on_refusal() {
        // One guest exits while the second keeps its loopback port open
        let first = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let first_port = first.local_addr().unwrap().port();
        let second = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let second_port = second.local_addr().unwrap().port();
        let listing = json!({"version":1,"instances":[{"port":second_port,"disk":"second.ark","disk_id":"02","ready":true}]}).to_string();
        let (client, worker) = peer(vec![
            ("POST", response(204, "")),
            ("POST", response(204, "")),
            ("GET", response(200, &listing)),
            ("POST", response(403, "stop requests are refused")),
        ]);
        drop(first);
        let now = Instant::now();
        let mut progress = Stopping {
            pending: vec![first_port, second_port],
            done: Vec::new(),
            asked: None,
        };
        assert!(!progress.poll(client, now).unwrap());
        assert_eq!(progress.done, [first_port]);

        // The next heartbeat fails permanently without erasing the confirmed result
        assert!(progress.poll(client, now + Duration::from_secs(1)).is_err());
        assert_eq!(progress.done, [first_port]);
        assert_eq!(progress.pending, [second_port]);
        assert_eq!(
            stopped(&progress.done),
            json!({"stopped":[format!("emulator:{first_port}")]})
        );
        worker.join().unwrap();
    }

    /// Refused or malformed listing polls end a stop rather than running to timeout.
    #[test]
    fn test_stop_surfaces_listing_failures() {
        for reply in [
            response(403, "listing refused"),
            response(200, "not json"),
            response(200, r#"{"version":999,"instances":[]}"#),
        ] {
            let (client, worker) = peer(vec![("POST", response(204, "")), ("GET", reply.clone())]);
            let mut progress = Stopping {
                pending: vec![18181],
                done: Vec::new(),
                asked: None,
            };
            let err = progress.poll(client, Instant::now()).unwrap_err();
            assert!(!err.retryable(), "{reply}");
            assert!(progress.done.is_empty());
            assert_eq!(progress.pending, [18181]);
            worker.join().unwrap();
        }
    }

    /// Lost replies and a replacement host's missing entries recover on later beats.
    #[test]
    fn test_stop_recovers_from_host_handover_and_republication() {
        // Keep the guest alive while the replacement registry starts empty
        let guest = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = guest.local_addr().unwrap().port();
        let empty = response(200, r#"{"version":1,"instances":[]}"#);
        let listed = response(200, &json!({"version":1,"instances":[{"port":port,"disk":"demo.ark","disk_id":"01","ready":true}]}).to_string());
        let (client, worker) = peer(vec![
            ("POST", String::new()),
            ("GET", String::new()),
            ("POST", response(404, "no emulator on that port")),
            ("GET", empty.clone()),
            ("POST", response(204, "")),
            ("GET", listed),
            ("POST", response(404, "no emulator on that port")),
            ("GET", empty),
        ]);
        let mut progress = Stopping {
            pending: vec![port],
            done: Vec::new(),
            asked: None,
        };
        let now = Instant::now();
        for seconds in 0..3 {
            assert!(
                !progress
                    .poll(client, now + Duration::from_secs(seconds))
                    .unwrap(),
                "{seconds}"
            );
            assert!(progress.done.is_empty(), "{seconds}");
        }

        // A missing entry counts as stopped only once the guest port closes too
        drop(guest);
        assert!(progress.poll(client, now + Duration::from_secs(3)).unwrap());
        assert_eq!(progress.done, [port]);
        worker.join().unwrap();
    }

    #[test]
    fn test_a_timestamp_reads_as_a_utc_instant() {
        assert_eq!(iso8601(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            iso8601(1_788_000_000).as_deref(),
            Some("2026-08-29T10:40:00Z")
        );
    }

    /// A running emulator, as the registry would list it.
    fn running(port: u16, image: &str, name: Option<&str>, serial: Option<&str>) -> Instance {
        Instance {
            port,
            control: None,
            disk: image.into(),
            disk_id: format!("{port:016x}"),
            ready: true,
            env: None,
            name: name.map(str::to_owned),
            serial: serial.map(str::to_owned),
            expiry: None,
        }
    }

    /// A selector reads as `ark -d` reads it, and the only emulator running
    /// needs none.
    #[test]
    fn test_a_selector_names_one_running_emulator() {
        let both = [
            running(18181, "a.ark", Some("demo"), None),
            running(18182, "b.ark", None, Some("abc123")),
        ];
        let ports = |picked: Vec<Instance>| picked.iter().map(|i| i.port).collect::<Vec<_>>();
        assert_eq!(ports(pick(&both, Some("emulator:18182")).unwrap()), [18182]);
        assert_eq!(ports(pick(&both, Some("demo")).unwrap()), [18181]);
        assert_eq!(ports(pick(&both, Some("b.ark")).unwrap()), [18182]);
        assert_eq!(ports(pick(&both, Some("abc123")).unwrap()), [18182]);
        assert_eq!(ports(pick(&both[..1], None).unwrap()), [18181]);
        assert_eq!(ports(pick(&both[..1], Some("emulator")).unwrap()), [18181]);

        assert_eq!(pick(&both, None).unwrap_err().code, Code::AmbiguousEmulator);
        assert_eq!(pick(&[], None).unwrap_err().code, Code::NoEmulator);
        assert_eq!(
            pick(&both, Some("emulator:1")).unwrap_err().code,
            Code::NoEmulator
        );
        let bare = pick(&both, Some("18181")).unwrap_err();
        assert_eq!(bare.code, Code::NoEmulator);
        assert!(bare.hints[0].contains("emulator:18181"), "{:?}", bare.hints);

        let twins = [
            running(18181, "same.ark", None, None),
            running(18182, "same.ark", None, None),
        ];
        assert_eq!(
            pick(&twins, Some("same.ark")).unwrap_err().code,
            Code::AmbiguousEmulator
        );
    }

    /// The tail is the end of the log, not its beginning, and a log that is
    /// not there reads as nothing rather than failing.
    #[test]
    fn test_a_log_tail_carries_the_last_lines() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("18181.log");
        let lines: Vec<String> = (0..25).map(|index| format!("line {index}")).collect();
        std::fs::write(&log, lines.join("\n")).unwrap();

        let last = tail(&log);
        assert!(last.starts_with("line 5\n"), "{last}");
        assert!(last.ends_with("line 24"), "{last}");
        assert!(tail(&tmp.path().join("missing.log")).is_empty());
    }
}
