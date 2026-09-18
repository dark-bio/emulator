// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! What the command line does with an emulator: boot one, find them, stop one,
//! wipe an image, and say what this build is made of.
//!
//! None of it opens a window or touches the display. A command finds emulators
//! through the registry, asks one to stop through the same registry, and boots
//! a new one by starting this executable again with the image and the port it
//! settled on. The process that owns QEMU stays the one that owns its window.
//!
//! Nothing here speaks to the firmware. Everything about an emulated Ark's
//! identity, pairing and data belongs to `ark`.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::PackageInfo;

use crate::bundle::{self, Paths};
use crate::diagnostics::{self, Sink};
use crate::discovery::{self, disk_id};
use crate::disk::{name_of, DEFAULT_DISK};
use crate::output::{self, Error, Output};
use crate::qemu::{self, GuestArch, HostPort};
use crate::registry::{Instance, REGISTRY_PORT};
use crate::settings::Settings;
use crate::{Boot, Global};

/// How often a wait asks the registry again. Short enough that a boot which
/// takes ten seconds is reported as soon as it happens.
const POLL: Duration = Duration::from_millis(250);

/// How much of a failed launcher's log an error carries, in lines. Enough to
/// hold what QEMU said about why it would not start.
const TAIL: usize = 20;

/// How long a loopback port is given to answer. Nothing on this computer is
/// far enough away to need more.
const DIAL: Duration = Duration::from_millis(500);

/// What the command line can be asked to do.
#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Boot an emulator in the background and print its locator once ready
    Start {
        /// The image, environment, memory and firmware the emulator boots on.
        #[command(flatten)]
        boot: Boot,
    },

    /// Show the emulators running on this computer
    List,

    /// Shut an emulator down, like closing its window
    Stop {
        /// Port of the emulator to stop, from `list` or `ark devices`
        #[arg(
            value_name = "PORT",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        port: Option<u16>,

        /// Stop every emulator on this computer
        #[arg(long)]
        all: bool,
    },

    /// Delete a stopped disk image so the next boot is a fresh device
    Wipe {
        /// Image to delete
        #[arg(value_name = "PATH")]
        path: PathBuf,

        /// Delete without being asked to confirm
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Check this computer and this build; suggest fixes
    Doctor,

    /// Help for a command or a topic: agents, output, disks, registry
    Help {
        /// Command or topic to explain
        #[arg(value_name = "COMMAND_OR_TOPIC")]
        name: Option<String>,

        // The root, every command and every topic as one document.
        #[arg(long, hide = true, conflicts_with = "name")]
        all: bool,
    },
}

/// Run one command, print what it answers, and hand back the exit code. No
/// command at all is the root's own answer, which is what `--version` asks
/// for, since a bare run opens a window instead of coming through here.
pub(crate) fn run(
    command: Option<Command>,
    global: &Global,
    identifier: &str,
    package: &PackageInfo,
) -> i32 {
    let output = Output::new(global);
    diagnostics::log_sink(match global.log {
        Some(_) => Sink::Events(output.clone()),
        None => Sink::Quiet,
    });
    match dispatch(command, global, &output, identifier, package) {
        Ok(()) => 0,
        Err(error) => {
            output.error(&error);
            error.exit
        }
    }
}

/// Send one command to the code that serves it, with the paths every command
/// needs already worked out.
fn dispatch(
    command: Option<Command>,
    global: &Global,
    output: &Output,
    identifier: &str,
    package: &PackageInfo,
) -> Result<(), Error> {
    // Answered before anything is resolved, since a reader asking what a
    // command does may be on a computer where nothing else would work.
    if let Some(Command::Help { name, all }) = &command {
        return crate::help::run(name.as_slice(), *all, true);
    }
    let paths = Paths::resolve(identifier, package)
        .map_err(|err| Error::new(1, "io", format!("{err:#}")))?;
    match command {
        Some(Command::Start { boot }) => start(&boot, global, output, &paths),
        Some(Command::List) => list(output, &paths),
        Some(Command::Stop { port, all }) => stop(port, all, global, output),
        Some(Command::Wipe { path, yes }) => wipe(&path, yes, output),
        Some(Command::Doctor) => doctor(output, &paths),
        Some(Command::Help { .. }) => unreachable!("answered before the paths"),
        None => version(output, &paths),
    }
}

/// What this build is made of, which is what a bug report needs naming.
fn version(output: &Output, paths: &Paths) -> Result<(), Error> {
    let arch = architecture(None)?;
    let qemu = qemu::resolve_qemu(arch);
    output.block(
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "firmware": bundle::firmware_version(paths.resources.as_deref(), arch),
            "qemu": qemu_version(&qemu.binary),
        }),
        &[
            ("Version", "version"),
            ("Firmware", "firmware"),
            ("QEMU", "qemu"),
        ],
    );
    Ok(())
}

/// Boot an emulator on the image this run settles on, and answer once the
/// firmware accepts clients.
///
/// The emulator runs in a process of its own, which is the process that owns
/// its window and its QEMU. This command hands it the image and the port so
/// that both sides name the same device, then watches the registry for it.
fn start(boot: &Boot, global: &Global, output: &Output, paths: &Paths) -> Result<(), Error> {
    let arch = architecture(boot.arch)?;
    let settings = Settings::load(&paths.data).map_err(local)?;
    let running = listing()?;

    let address = match boot.host_addr {
        Some(address) => address,
        None => HostPort::reserve()
            .map_err(|err| {
                Error::new(1, "port-exhausted", format!("{err:#}"))
                    .hint("pass `--host-addr` to choose the port yourself")
            })?
            .addr(),
    };
    output.event("step", format!("holding port {}", address.port()));

    let image = choose(
        boot.disk.as_deref(),
        &settings,
        paths,
        address.port(),
        &running,
    )?;
    let id = disk_id(&image);
    if let Some(instance) = running.get(&id) {
        output.event("note", format!("{} is already booted", name_of(&image)));
        return report(output, paths, instance, &image, false, false);
    }

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
    let mut child = spawn(boot, arch, &image, address).map_err(local)?;
    let mut relay = Relay::new(global.log.map(|_| log.as_path()));

    let deadline = Instant::now() + Duration::from_secs(global.timeout);
    loop {
        relay.pump(output);
        if let Some(instance) = listing()?.get(&id) {
            if instance.ready {
                warn_environment(output, boot, instance);
                return report(output, paths, instance, &image, created, true);
            }
        } else if let Ok(Some(status)) = child.try_wait() {
            relay.pump(output);
            return Err(Error::new(
                1,
                "stopped-unexpectedly",
                format!("the emulator exited with {status}\n{}", tail(&log)),
            )
            .hint(format!("its log is at {}", log.display())));
        }
        if Instant::now() >= deadline {
            let unready = Instance {
                port: address.port(),
                disk: name_of(&image),
                disk_id: id,
                ready: false,
                env: None,
                name: None,
                serial: None,
                expiry: None,
            };
            report(output, paths, &unready, &image, created, true)?;
            return Err(Error::new(
                7,
                "timeout",
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

/// Show every emulator this computer is running.
fn list(output: &Output, paths: &Paths) -> Result<(), Error> {
    let running = listing()?;
    let mut instances: Vec<&Instance> = running.values().collect();
    instances.sort_by_key(|instance| instance.port);
    let rows: Vec<Value> = instances
        .iter()
        .map(|instance| row(paths, instance))
        .collect();
    output.table(
        &json!({ "emulators": rows }),
        &rows,
        &[
            ("PORT", "port"),
            ("DISK", "disk"),
            ("READY", "ready"),
            ("ENV", "env"),
            ("NAME", "name"),
            ("SERIAL", "serial"),
            ("EXPIRES", "expiry"),
        ],
    );
    Ok(())
}

/// Shut one emulator down, or every one of them.
fn stop(port: Option<u16>, all: bool, global: &Global, output: &Output) -> Result<(), Error> {
    let running = listing()?;
    let mut ports: Vec<u16> = match port {
        Some(port) => {
            if !running.values().any(|instance| instance.port == port) {
                return Err(Error::new(
                    3,
                    "no-emulator",
                    format!("no emulator is running on port {port}"),
                )
                .hint("`ark-emulator list` shows the ports in use"));
            }
            vec![port]
        }
        None => running.values().map(|instance| instance.port).collect(),
    };
    ports.sort_unstable();

    if ports.is_empty() {
        output.event("note", "no emulators are running");
        output.block(&json!({ "stopped": [] }), &[("Stopped", "stopped")]);
        return Ok(());
    }
    for port in &ports {
        output.event("step", format!("asking the emulator on {port} to stop"));
    }

    let deadline = Instant::now() + Duration::from_secs(global.timeout);
    let mut asked: Option<Instant> = None;
    loop {
        // The registry moves to another launcher when the one hosting it goes,
        // and the new host's registry starts out empty, so a request left in
        // the old one has to be left again. A round that cannot be delivered
        // at all is one the next round covers.
        if asked.is_none_or(|at| at.elapsed() >= discovery::HEARTBEAT) {
            for port in &ports {
                let _ = discovery::request_stop(*port);
            }
            asked = Some(Instant::now());
        }

        // An emulator holds its port for as long as it is up, so a port that
        // refuses a connection is a device that has gone. That is a firmer
        // answer than the listing, which a change of registry can empty for a
        // moment.
        let left: Vec<u16> = ports
            .iter()
            .copied()
            .filter(|port| listening(*port))
            .collect();
        if left.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::new(
                7,
                "timeout",
                format!(
                    "the emulator on port {} was still up after {} s",
                    left[0], global.timeout
                ),
            )
            .hint("close its window to shut it down"));
        }
        std::thread::sleep(POLL);
    }

    if all {
        output.block(&json!({ "stopped": ports }), &[("Stopped", "stopped")]);
    } else {
        output.block(
            &json!({ "port": ports.first(), "stopped": true }),
            &[("Port", "port"), ("Stopped", "stopped")],
        );
    }
    Ok(())
}

/// Delete a stopped image, so that the next boot on it is a fresh device.
fn wipe(path: &Path, yes: bool, output: &Output) -> Result<(), Error> {
    let path = settle(path)?;
    let size = std::fs::metadata(&path)
        .map_err(|err| {
            Error::new(
                1,
                "disk-missing",
                format!("could not read {}: {err}", path.display()),
            )
        })?
        .len();

    if let Some(instance) = listing()?.get(&disk_id(&path)) {
        return Err(Error::new(
            3,
            "disk-busy",
            format!(
                "{} is booted by the emulator on port {}",
                path.display(),
                instance.port
            ),
        )
        .hint(format!(
            "stop it first with `ark-emulator stop {}`",
            instance.port
        )));
    }

    if !yes
        && !output.confirm(
            &format!("Delete {}, {}?", path.display(), output::bytes(size)),
            &format!("deleting {} was not confirmed", path.display()),
            "--yes",
        )?
    {
        return Err(Error::new(
            1,
            "confirmation-required",
            format!("{} was left alone", path.display()),
        ));
    }

    std::fs::remove_file(&path).map_err(|err| {
        Error::new(
            1,
            "io",
            format!("could not delete {}: {err}", path.display()),
        )
    })?;
    output.block(
        &json!({"path": path, "deleted": true, "freed_bytes": size}),
        &[
            ("Path", "path"),
            ("Deleted", "deleted"),
            ("Freed", "freed_bytes"),
        ],
    );
    Ok(())
}

/// Check this computer and this build, and say what to fix. Every check runs
/// and the whole list prints; the first failure then sets the exit, so a
/// caller sees everything that is wrong at once.
fn doctor(output: &Output, paths: &Paths) -> Result<(), Error> {
    let mut checks = Checks::new(output);
    let arch = architecture(None)?;

    let qemu = qemu::resolve_qemu(arch);
    let qemu_version = qemu_version(&qemu.binary);
    let origin = if qemu.bundled { "bundled" } else { "on PATH" };
    match &qemu_version {
        Some(version) => checks.ok(
            "qemu",
            &format!("{version} {origin} at {}", qemu.binary.display()),
        ),
        None => checks.fail(
            "qemu",
            Error::new(
                1,
                "qemu-missing",
                format!("{} could not be run", qemu.binary.display()),
            )
            .hint(if qemu.bundled {
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
                1,
                "firmware-missing",
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
                1,
                "no-acceleration",
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
            Error::new(
                1,
                "io",
                format!("{} is not writable: {err}", paths.data.display()),
            )
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
                Error::new(1, "io", format!("{err:#}"))
                    .hint("move the settings file aside to start over"),
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
                    1,
                    "disk-missing",
                    format!("{} is not there", image.display()),
                )
                .hint("open or create one in the window, or start with --disk"),
            ),
        },
    }

    let registry = discovery::list();
    match &registry {
        Ok(instances) if instances.is_empty() => checks.ok("registry", "no emulator running"),
        Ok(instances) => checks.ok("registry", &format!("{} running", instances.len())),
        Err(err) => checks.fail(
            "registry",
            Error::new(3, "registry-unreachable", format!("{err:#}"))
                .hint("another program may be holding the emulator registry's port"),
        ),
    }

    match HostPort::reserve() {
        Ok(port) => checks.ok("ports", &format!("{} free", port.port())),
        Err(err) => checks.fail(
            "ports",
            Error::new(1, "port-exhausted", format!("{err:#}"))
                .hint("stop an emulator, or pass --host-addr to choose a port"),
        ),
    }

    let document = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "firmware": firmware.as_ref().map(|firmware| json!({
            "version": firmware_version,
            "kernel": digest(&firmware.kernel),
            "initrd": digest(&firmware.initrd),
        })),
        "qemu": {
            "binary": qemu.binary,
            "version": qemu_version,
            "bundled": qemu.bundled,
        },
        "accel": accel,
        "arch": arch.name(),
        "data_dir": paths.data,
        "settings": settings.as_ref().map(|settings| settings.path().to_path_buf()),
        "disk": image,
        "logs_dir": diagnostics::logs_dir(&paths.data),
        "registry": {
            "reachable": listening(REGISTRY_PORT),
            "instances": registry.as_ref().map_or(0, Vec::len),
        },
        "checks": checks.rows,
    });
    output.checklist(&document, &checks.rows);
    match checks.failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The diagnostics of one doctor run, in the order they ran, and the first
/// failure among them, which is what the command exits with.
struct Checks<'a> {
    /// Where a step is narrated as each check lands.
    output: &'a Output,

    /// Every check as the document carries it.
    rows: Vec<Value>,

    /// The earliest failure, kept while the later checks still run.
    failure: Option<Error>,
}

impl<'a> Checks<'a> {
    /// An empty list, narrating to `output`.
    fn new(output: &'a Output) -> Self {
        Self {
            output,
            rows: Vec::new(),
            failure: None,
        }
    }

    /// Record a check that passed, with what it saw.
    fn ok(&mut self, name: &str, detail: &str) {
        self.add(name, "ok", detail, None);
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

    /// Append one check and narrate it as a step.
    fn add(&mut self, name: &str, result: &str, detail: &str, hint: Option<&str>) {
        self.output
            .event("step", format!("{name}: {result}, {detail}"));
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

/// What the emulators running on this computer look like, by the image each
/// one holds. The image is what tells two emulators apart before either has
/// been given a name.
fn listing() -> Result<HashMap<String, Instance>, Error> {
    let instances = discovery::list().map_err(|err| {
        Error::new(3, "registry-unreachable", format!("{err:#}"))
            .hint("another program may be holding the emulator registry's port")
    })?;
    Ok(instances
        .into_iter()
        .map(|instance| (instance.disk_id.clone(), instance))
        .collect())
}

/// Whether anything is accepting connections on a loopback port. For the
/// registry's port it says whether one is being served; for an emulator's it
/// says whether the device is still up, since QEMU holds that port for as long
/// as the emulator lives.
fn listening(port: u16) -> bool {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    TcpStream::connect_timeout(&address.into(), DIAL).is_ok()
}

/// One emulator as both outputs carry it. The log file is worked out here
/// rather than read from the registry, which publishes no paths.
fn row(paths: &Paths, instance: &Instance) -> Value {
    json!({
        "locator": format!("emulator:{}", instance.port),
        "port": instance.port,
        "disk": instance.disk,
        "disk_id": instance.disk_id,
        "ready": instance.ready,
        "env": instance.env,
        "name": instance.name,
        "serial": instance.serial,
        "expiry": instance.expiry.map(iso8601),
        "log": diagnostics::log_path(&paths.data, instance.port),
    })
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
    document["path"] = json!(image);
    document["created"] = json!(created);
    document["started"] = json!(started);
    output.block(
        &document,
        &[
            ("Locator", "locator"),
            ("Disk", "disk"),
            ("Created", "created"),
            ("Started", "started"),
            ("Env", "env"),
            ("Ready", "ready"),
            ("Expires", "expiry"),
        ],
    );
    Ok(())
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
    if let Some(arch) = named {
        return Ok(arch);
    }
    match std::env::consts::ARCH {
        "aarch64" => Ok(GuestArch::Arm64),
        "x86_64" => Ok(GuestArch::Amd64),
        other => Err(Error::new(
            1,
            "firmware-missing",
            format!("no firmware exists for {other} computers"),
        )
        .hint("pass `--arch` to name the architecture to boot")),
    }
}

/// Which image to boot. An image named on the command line wins, then the
/// remembered one, which is the device the owner opens by double-click, and
/// then the image the launcher allocates for itself. An emulator already
/// holding the chosen image is reported rather than refused, since a start
/// states a goal.
fn choose(
    named: Option<&Path>,
    settings: &Settings,
    paths: &Paths,
    port: u16,
    running: &HashMap<String, Instance>,
) -> Result<PathBuf, Error> {
    if let Some(image) = named {
        return settle(image);
    }
    if let Some(image) = settings.disk() {
        let image = settle(image)?;
        if running.contains_key(&disk_id(&image)) || image.is_file() {
            return Ok(image);
        }
    }
    let default = settle(&paths.data.join(DEFAULT_DISK))?;
    if !running.contains_key(&disk_id(&default)) {
        return Ok(default);
    }
    settle(&paths.data.join(format!("emulator-{port}.ark")))
}

/// An image's path with the directories above it resolved, so that this
/// command and the emulator it starts agree on which image is which. The file
/// itself need not exist yet, and a symbolic link in the path would otherwise
/// give the two of them different answers.
fn settle(image: &Path) -> Result<PathBuf, Error> {
    let image = std::path::absolute(image).map_err(|err| {
        Error::new(
            1,
            "io",
            format!("could not resolve {}: {err}", image.display()),
        )
    })?;
    let (Some(parent), Some(name)) = (image.parent(), image.file_name()) else {
        return Ok(image);
    };
    match parent.canonicalize() {
        Ok(parent) => Ok(parent.join(name)),
        Err(_) => Ok(image),
    }
}

/// Start this executable again as the emulator, in a process group of its own
/// so that a Ctrl-C meant for the wait does not reach the device. Its streams
/// go nowhere: it keeps its own log file.
fn spawn(boot: &Boot, arch: GuestArch, image: &Path, address: SocketAddr) -> anyhow::Result<Child> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("--no-input")
        .arg("--disk")
        .arg(image)
        .arg("--host-addr")
        .arg(address.to_string())
        .arg("--arch")
        .arg(arch.name());
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
    detach(&mut command);
    Ok(command.spawn()?)
}

/// Put the emulator in a group of its own, so that a Ctrl-C at the terminal
/// ends the wait and leaves the device booting.
#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

/// The same, plus the flag that keeps Windows from giving a console-subsystem
/// child a console window of its own, and this process's own streams made
/// private first. The child gets null streams of its own, but Windows would
/// still hand it an inheritable copy of these, and a file that `start`'s
/// output was redirected to would then stay open for as long as the emulator
/// runs.
#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: a standard handle this process owns, or null, or the invalid
        // value, is read and then only handed back to Win32 to clear one flag
        // on it; nothing is dereferenced.
        unsafe {
            let handle = GetStdHandle(stream);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
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
}

impl Relay {
    /// Follow `log`, from wherever it is now.
    fn new(log: Option<&Path>) -> Self {
        Self {
            log: log.map(Path::to_path_buf),
            read: 0,
        }
    }

    /// Relay whatever the emulator has written since the last look.
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
        let mut fresh = String::new();
        let Ok(count) = file.read_to_string(&mut fresh) else {
            return;
        };
        self.read += count as u64;
        for line in fresh.lines() {
            output.event("log", line);
        }
    }
}

/// The SHA-256 of a file, as the digest a bug report quotes.
fn digest(path: &Path) -> Option<String> {
    use sha2::{Digest as _, Sha256};
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// The version QEMU reports, which is the third word of its first line.
fn qemu_version(binary: &Path) -> Option<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?;
    line.split_whitespace().nth(3).map(str::to_owned)
}

/// A Unix timestamp as the UTC instant every Dark Bio tool prints, by the
/// civil-from-days conversion.
fn iso8601(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let clock = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        (clock / 60) % 60,
        clock % 60
    )
}

/// A failure in this computer's own files, which is the exit class a caller
/// fixes locally.
fn local(error: anyhow::Error) -> Error {
    Error::new(1, "io", format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The port a start would hold, which only ever shows up in the name of an
    /// image allocated for a second unattended emulator.
    const PORT: u16 = 18182;

    fn instance(port: u16, image: &Path) -> Instance {
        Instance {
            port,
            disk: name_of(image),
            disk_id: disk_id(image),
            ready: true,
            env: None,
            name: None,
            serial: None,
            expiry: None,
        }
    }

    fn running(instances: &[Instance]) -> HashMap<String, Instance> {
        instances
            .iter()
            .map(|instance| (instance.disk_id.clone(), instance.clone()))
            .collect()
    }

    #[test]
    fn test_a_named_image_wins_over_the_remembered_one() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths {
            data: tmp.path().to_path_buf(),
            resources: None,
        };
        let remembered = tmp.path().join("remembered.ark");
        std::fs::write(&remembered, b"").unwrap();
        let mut settings = Settings::load(tmp.path()).unwrap();
        settings
            .apply(Some(&remembered), true, 2048, "release")
            .unwrap();

        let named = tmp.path().join("named.ark");
        let chosen = choose(Some(&named), &settings, &paths, PORT, &running(&[])).unwrap();
        assert_eq!(chosen, settle(&named).unwrap());
    }

    #[test]
    fn test_a_remembered_image_is_chosen_even_while_it_is_booted() {
        // A start states a goal, so an image that is already up is reported
        // rather than passed over for a second one.
        let tmp = TempDir::new().unwrap();
        let paths = Paths {
            data: tmp.path().to_path_buf(),
            resources: None,
        };
        let remembered = settle(&tmp.path().join("remembered.ark")).unwrap();
        std::fs::write(&remembered, b"").unwrap();
        let mut settings = Settings::load(tmp.path()).unwrap();
        settings
            .apply(Some(&remembered), false, 2048, "release")
            .unwrap();

        let booted = running(&[instance(18181, &remembered)]);
        let chosen = choose(None, &settings, &paths, PORT, &booted).unwrap();
        assert_eq!(chosen, remembered);
    }

    #[test]
    fn test_a_remembered_image_that_is_gone_falls_back_to_the_default() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths {
            data: tmp.path().to_path_buf(),
            resources: None,
        };
        let mut settings = Settings::load(tmp.path()).unwrap();
        settings
            .apply(Some(&tmp.path().join("deleted.ark")), true, 2048, "release")
            .unwrap();

        let chosen = choose(None, &settings, &paths, PORT, &running(&[])).unwrap();
        assert_eq!(chosen, settle(&tmp.path().join(DEFAULT_DISK)).unwrap());
    }

    #[test]
    fn test_a_default_image_in_use_gives_way_to_one_named_by_port() {
        let tmp = TempDir::new().unwrap();
        let paths = Paths {
            data: tmp.path().to_path_buf(),
            resources: None,
        };
        let settings = Settings::load(tmp.path()).unwrap();
        let default = settle(&tmp.path().join(DEFAULT_DISK)).unwrap();

        let booted = running(&[instance(18181, &default)]);
        let chosen = choose(None, &settings, &paths, PORT, &booted).unwrap();
        assert_eq!(
            chosen,
            settle(&tmp.path().join("emulator-18182.ark")).unwrap()
        );
    }

    #[test]
    fn test_an_image_is_settled_to_the_same_path_from_either_side() {
        // The emulator resolves the path it is handed all over again, so a
        // link anywhere above the image has to be gone by then.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("images");
        std::fs::create_dir(&real).unwrap();
        let image = real.join("device.ark");
        std::fs::write(&image, b"").unwrap();

        let settled = settle(&image).unwrap();
        assert_eq!(disk_id(&settled), disk_id(&image.canonicalize().unwrap()));
        assert_eq!(settle(&settled).unwrap(), settled);
    }

    #[test]
    fn test_a_timestamp_reads_as_a_utc_instant() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso8601(1_788_000_000), "2026-08-29T10:40:00Z");
    }

    #[test]
    fn test_a_log_tail_carries_the_last_lines_it_has() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("18181.log");
        let lines: Vec<String> = (0..TAIL + 5).map(|index| format!("line {index}")).collect();
        std::fs::write(&log, lines.join("\n")).unwrap();

        let last = tail(&log);
        assert_eq!(last.lines().count(), TAIL);
        assert!(last.starts_with("line 5"));
        assert!(last.ends_with(&format!("line {}", TAIL + 4)));
        assert!(tail(&tmp.path().join("missing.log")).is_empty());
    }
}
