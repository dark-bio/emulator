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

use crate::args::{Boot, ButtonAction, Command, Global};
use crate::bundle::{self, Paths};
use crate::diagnostics::{self, Sink};
use crate::discovery;
use crate::disk;
use crate::error::{Code, Error};
use crate::output::{self, Output};
use crate::platform;
use crate::qemu::{self, GuestArch, HostPort};
use crate::registry::Instance;
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
        Some(Command::Start { .. } | Command::List | Command::Stop { .. } | Command::Wipe { .. })
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
        Some(Command::Help { name, all }) => return crate::help::run(name.as_slice(), all, true),
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
        Some(Command::Doctor) => crate::doctor::doctor(output, &paths, global.timeout),
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
        &mut crate::help::parser(),
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
/// locators that went are the result, and on a timeout they are the partial
/// result before the failure.
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

    let mut pending: Vec<u16> = targets.iter().map(|instance| instance.port).collect();
    let mut done: Vec<u16> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(global.timeout);
    let mut asked: Option<Instant> = None;
    while !pending.is_empty() {
        // The registry moves to another launcher when the one hosting it goes,
        // and the new host's registry starts out empty, so a request left in
        // the old one has to be left again. A round that cannot be delivered
        // at all is one the next round covers.
        if asked.is_none_or(|at| at.elapsed() >= discovery::HEARTBEAT) {
            for port in &pending {
                let _ = discovery::request_stop(*port);
            }
            asked = Some(Instant::now());
        }

        // An emulator withdraws from the registry as it goes, and QEMU lets go
        // of its port with it; the two together say it has gone. Either alone
        // misleads: the listing empties for a moment when the registry changes
        // hosts, and on Windows a closed port times out the way a busy one
        // does. A registry that cannot be read this round settles nothing.
        let listed: Option<Vec<u16>> = listing()
            .ok()
            .map(|instances| instances.iter().map(|instance| instance.port).collect());
        let (gone, held): (Vec<u16>, Vec<u16>) = pending.iter().partition(|port| {
            listed.as_ref().is_some_and(|listed| !listed.contains(port)) && !answering(**port)
        });
        done.extend(gone);
        pending = held;
        if pending.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            output.block(&stopped(&done), &[("Stopped", "stopped")])?;
            return Err(Error::new(
                Code::Timeout,
                format!(
                    "emulator:{} was still up after {} s",
                    pending[0], global.timeout
                ),
            )
            .hint("close its window, or interrupt its foreground headless process"));
        }
        std::thread::sleep(POLL);
    }
    output.block(&stopped(&done), &[("Stopped", "stopped")])
}

/// Set this emulator's CLI hold and report the state after hardware delivery.
fn button(action: ButtonAction, global: &Global, output: &Output) -> Result<(), Error> {
    let (pressed, selector) = match action {
        ButtonAction::Press { emulator } => (true, emulator),
        ButtonAction::Release { emulator } => (false, emulator),
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
    let outcome = crate::control::button(endpoint, pressed, Duration::from_secs(global.timeout))?;
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
    if !pressed && outcome.pressed {
        output.event("note", "the window still holds the button");
    }
    output.block(
        &json!({
            "locator": locator(instance), "pressed": outcome.pressed,
            "cli_pressed": outcome.cli_pressed, "changed": outcome.changed,
        }),
        &[
            ("Locator", "locator"),
            ("Pressed", "pressed"),
            ("CLI held", "cli_pressed"),
            ("Changed", "changed"),
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
    discovery::list().map_err(|err| {
        Error::new(Code::RegistryUnreachable, format!("{err:#}"))
            .hint("another program may be holding the emulator registry's port")
    })
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
    use tempfile::TempDir;

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
