// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The launcher's side of the registry: reading it, keeping this emulator's
//! entry in it up to date, and bounded HTTP framing over private local IPC.
//!
//! A refused connection means no registry is running. Connection loss can
//! recover through host takeover; HTTP refusals, timeouts and malformed replies
//! reach the caller. Publication failures reach the launcher's error handler,
//! while withdrawal stays best-effort so shutdown never depends on discovery.
//!
//! Every launcher tries to host the registry, one wins the port, and the rest
//! publish themselves to whoever did (see [`super::registry`]). Takeover rides
//! on the heartbeat: one that cannot be delivered means the host is gone, so
//! the launcher tries to become the host and republishes itself either way.

use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use sha2::{Digest as _, Sha256};

use super::local;
use super::registry::{self, Instance, REGISTRY_PORT, SCHEMA_VERSION};
use crate::diagnostics::{log, trace};
use crate::runtime::hardware::Controller;

/// How often this emulator re-registers itself.
/// This stays well below the registry's expiry to keep the entry alive.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(1);

/// Limit for each registry I/O operation. Windows gets time to finish its
/// loopback connection-refusal retries before a timeout becomes a failure.
const TIMEOUT: Duration = Duration::from_secs(if cfg!(windows) { 5 } else { 2 });

/// The most a registry answer may be. A hundred entries are a few kilobytes;
/// anything near this is not a registry.
const MAX_RESPONSE: u64 = 1 << 20;

/// This emulator's entry, shared by the heartbeat and shutdown.
static ENTRY: OnceLock<Mutex<Instance>> = OnceLock::new();

/// Serializes publication with withdrawal and prevents a stopped guest returning.
static PUBLISHING: Mutex<bool> = Mutex::new(true);

/// Client for the registry shared by all launchers on this computer.
pub(crate) const CLIENT: Client = Client {
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, REGISTRY_PORT),
};

/// Registry operations with transport loss distinguished from permanent failures.
#[derive(Clone, Copy)]
pub(crate) struct Client {
    /// Loopback listener serving this registry.
    pub(crate) address: SocketAddrV4,
}

impl Client {
    /// List running emulators, returning an empty list only on connection refusal.
    pub(crate) fn list(self) -> Result<Vec<Instance>, Failure> {
        let body = match request(self.address, "GET", "/v1/instances", None) {
            Ok(body) => body,
            Err(Failure::Transport(err))
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(err) => return Err(err),
        };
        parse_listing(&body).map_err(Failure::Invalid)
    }

    /// Publish an entry, attempting host takeover only after a lost connection.
    fn publish(self, instance: &Instance) -> Result<(), Failure> {
        let body = serde_json::to_vec(instance)
            .context("could not encode this emulator's entry")
            .map_err(Failure::Invalid)?;
        let answer = match request(self.address, "POST", "/v1/instances", Some(&body)) {
            Err(err) if err.retryable() => {
                // A disappearing host frees its port for one of the other launchers
                if registry::host(self.address)? {
                    log!("[discovery] the registry had no host, taking it over");
                }
                request(self.address, "POST", "/v1/instances", Some(&body))?
            }
            answer => answer?,
        };
        if !answer.is_empty() {
            return Err(Failure::Invalid(anyhow::anyhow!(
                "the registry's publication reply must be empty"
            )));
        }
        Ok(())
    }
}

/// The instances in a listing. Each entry is read on its own, so one this
/// build cannot make sense of is logged and skipped rather than hiding the
/// rest, and a listing of another version is refused whole.
fn parse_listing(body: &[u8]) -> Result<Vec<Instance>> {
    let listing: serde_json::Value =
        serde_json::from_slice(body).context("could not read the registry's answer")?;
    let version = listing["version"].as_u64();
    if version != Some(u64::from(SCHEMA_VERSION)) {
        bail!(
            "the registry speaks version {}, and this build knows version {SCHEMA_VERSION}",
            version.map_or("none".to_owned(), |version| version.to_string())
        );
    }
    let entries = listing["instances"]
        .as_array()
        .context("the registry's answer holds no instances")?;
    Ok(entries
        .iter()
        .filter_map(
            |entry| match serde_json::from_value::<Instance>(entry.clone()) {
                Ok(instance) => Some(instance),
                Err(err) => {
                    log!("[discovery] skipping an entry the registry holds: {err}");
                    None
                }
            },
        )
        .collect())
}

/// The emulator among `instances` that holds `image`, if one does. The image
/// is what tells two emulators apart before either has been given a name.
pub(crate) fn booted<'a>(instances: &'a [Instance], image: &Path) -> Option<&'a Instance> {
    let id = disk_id(image);
    instances.iter().find(|instance| instance.disk_id == id)
}

/// Whether something accepts connections on a loopback port.
pub(crate) fn answering(port: u16) -> bool {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    TcpStream::connect_timeout(&address.into(), TIMEOUT).is_ok()
}

/// Publish this emulator before the runtime starts its periodic heartbeats.
pub(crate) fn register(
    port: u16,
    disk: &Path,
    hardware: Controller,
    control: super::control::Endpoint,
) -> Result<()> {
    let instance = Instance {
        port,
        control: Some(control),
        disk: disk
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        disk_id: disk_id(disk),
        ready: false,
        env: None,
        name: None,
        serial: None,
        expiry: None,
    };
    if ENTRY.set(Mutex::new(instance)).is_err() {
        bail!("this launcher already registered an emulator");
    }

    publish(&hardware)
}

/// Refresh the current entry, returning permanent failures to the runtime.
pub(crate) fn publish(hardware: &Controller) -> Result<()> {
    let publishing = PUBLISHING.lock().unwrap();
    if !*publishing || crate::runtime::stopping() {
        return Ok(());
    }
    let Some(entry) = ENTRY.get() else {
        return Ok(());
    };
    let answer = {
        let state = hardware.snapshot();
        let mut entry = entry.lock().unwrap();
        entry.ready = state.connected && state.nameplate.known;
        entry.env = state.nameplate.env;
        entry.name = state.nameplate.name;
        entry.serial = state.nameplate.serial;
        entry.expiry = state.nameplate.expiry;
        CLIENT.publish(&entry)
    };
    drop(publishing);
    match answer {
        Ok(()) => Ok(()),
        Err(err) if err.retryable() => {
            log!("[discovery] waiting for a registry host: {err}");
            Ok(())
        }
        Err(err) => Err(err).context("could not publish this emulator to the registry"),
    }
}

/// Withdraw this emulator from the registry. Best effort and quick: it runs
/// while the window is closing, and the entry would expire on its own anyway.
pub(crate) fn deregister() {
    let mut publishing = PUBLISHING.lock().unwrap();
    if !*publishing {
        return;
    }
    *publishing = false;
    let Some(entry) = ENTRY.get() else {
        return;
    };
    let port = entry.lock().unwrap().port;
    if let Err(e) = request(
        CLIENT.address,
        "DELETE",
        &format!("/v1/instances/{port}"),
        None,
    ) {
        log!("[discovery] could not deregister: {e}");
    }
}

/// Opaque, stable digest of a disk image's location, so two launchers can
/// agree on whether they are looking at the same image without the registry
/// publishing anybody's paths.
///
/// SHA-256 over the canonicalized path, truncated to 64 bits. The id travels
/// between emulators, which can be different builds, so it has to stay stable
/// across Rust versions. That rules out both [`std::hash::DefaultHasher`] and
/// `OsStr::as_encoded_bytes`, whose encoding std documents as comparable only
/// within one Rust version.
pub(crate) fn disk_id(disk: &Path) -> String {
    // Canonicalize so two spellings of one file agree. It needs the file to
    // exist, which one being allocated for the first time does not.
    let path = std::fs::canonicalize(disk).unwrap_or_else(|_| disk.to_path_buf());

    // Hashed as raw bytes: a Unix path is arbitrary bytes, and a lossy string
    // would map every path differing only in ill-formed encoding onto one id.
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        for unit in path.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    let digest = hasher.finalize();

    digest[..8].iter().fold(String::new(), |mut id, byte| {
        let _ = write!(id, "{byte:02x}");
        id
    })
}

/// A registry failure retaining whether host handover can recover it.
#[derive(Debug)]
pub(crate) enum Failure {
    /// An operating system error opening or using the local connection.
    Transport(io::Error),
    /// An HTTP refusal whose status and explanation came from the registry.
    Http {
        /// HTTP status code returned by the server.
        status: u16,
        /// Plain-text explanation, bounded by the response size limit.
        reason: String,
    },
    /// An invalid response, unsupported listing version or encoding failure.
    Invalid(anyhow::Error),
}

impl Failure {
    /// Whether losing a registry host can account for this connection failure.
    pub(crate) fn retryable(&self) -> bool {
        matches!(self, Self::Transport(err) if matches!(err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof | io::ErrorKind::Interrupted))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "registry I/O failed: {err}"),
            Self::Http { status, reason } if reason.is_empty() => {
                write!(f, "the registry answered HTTP {status}")
            }
            Self::Http { status, reason } => {
                write!(f, "the registry answered HTTP {status}: {reason}")
            }
            Self::Invalid(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for Failure {}

impl From<io::Error> for Failure {
    fn from(err: io::Error) -> Self {
        Self::Transport(err)
    }
}

/// Exchange one native registry request under a fixed I/O deadline.
fn request(
    addr: SocketAddrV4,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<Vec<u8>, Failure> {
    let deadline = Instant::now() + TIMEOUT;
    let mut stream = connect(addr, deadline)?;
    let mut exchange = || -> Result<Vec<u8>, Failure> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        stream.set_read_timeout(remaining);
        stream.set_write_timeout(remaining);

        let mut head = format!(
            "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\
             Content-Length: {}\r\n",
            body.map_or(0, <[u8]>::len)
        );
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
        }
        if matches!(method, "POST" | "DELETE") {
            head.push_str(&format!(
                "{}: {}\r\n",
                registry::WRITE_HEADER,
                registry::WRITE_HEADER_VALUE
            ));
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes())?;
        if let Some(body) = body {
            stream.write_all(body)?;
        }
        stream.flush()?;

        let mut raw = Vec::new();
        (&mut stream).take(MAX_RESPONSE + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_RESPONSE {
            return Err(Failure::Invalid(anyhow::anyhow!(
                "the registry's answer was too large"
            )));
        }
        let answer = split_response(&raw);
        trace!(
            "[discovery] {method} {path}: {}",
            match &answer {
                Ok(body) => format!("{} bytes", body.len()),
                Err(err) => err.to_string(),
            }
        );
        answer
    };
    exchange()
}

/// Wait for a winning launcher's local bind without falling back to HTTP writes.
fn connect(addr: SocketAddrV4, deadline: Instant) -> Result<local::Stream, Failure> {
    loop {
        let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
            Failure::Invalid(anyhow::anyhow!(
                "the browser registry has no local endpoint for this user; close emulators owned by other users, then update and restart all emulators"
            ))
        })?;
        match local::Stream::connect(&registry::local_name(addr.port()), remaining) {
            Ok(stream) => return Ok(stream),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                match TcpStream::connect_timeout(&addr.into(), remaining) {
                    Ok(_) => std::thread::sleep(Duration::from_millis(10).min(remaining)),
                    Err(cause) if cause.kind() == io::ErrorKind::ConnectionRefused => {
                        return Err(err.into());
                    }
                    Err(cause) => return Err(cause.into()),
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Pull the body out of a response, failing on any status the registry uses to
/// say no. The only server on the other end is [`super::registry`], which
/// answers with a status line, a few headers and an unencoded body.
fn split_response(raw: &[u8]) -> Result<Vec<u8>, Failure> {
    // A host disappearing before its reply can be retried after takeover
    if raw.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the registry closed before replying",
        )
        .into());
    }
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("the registry's answer had no header block")
        .map_err(Failure::Invalid)?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|status| (100..=599).contains(status))
        .context("the registry's answer had no valid status")
        .map_err(Failure::Invalid)?;
    if !(200..300).contains(&status) {
        return Err(Failure::Http {
            status,
            reason: String::from_utf8_lossy(&raw[split + 4..]).trim().to_owned(),
        });
    }
    Ok(raw[split + 4..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::testing::{registry as peer, response};
    use std::thread;

    /// A publish refusal reaches the launcher with the status and server explanation.
    #[test]
    fn test_registration_refusal_is_permanent_and_preserves_the_reason() {
        let (client, worker) = peer(vec![(
            "POST",
            response(403, "registry writes require X-Ark-Registry: 1"),
        )]);
        let instance: Instance = serde_json::from_str(
            r#"{"port":18181,"disk":"demo.ark","disk_id":"0123abcd","ready":false}"#,
        )
        .unwrap();
        let err = client.publish(&instance).unwrap_err();
        assert!(!err.retryable());
        assert!(
            matches!(&err, Failure::Http { status: 403, reason } if reason == "registry writes require X-Ark-Registry: 1")
        );
        assert!(err.to_string().contains("403"));
        assert!(err.to_string().contains("X-Ark-Registry: 1"));
        worker.join().unwrap();
    }

    /// Unexpected publication bodies fail without being interpreted as commands.
    #[test]
    fn test_registration_rejects_nonempty_publication_replies() {
        let instance: Instance = serde_json::from_str(
            r#"{"port":18181,"disk":"demo.ark","disk_id":"0123abcd","ready":false}"#,
        )
        .unwrap();
        for body in ["unexpected reply", r#"{"stop":true}"#] {
            let (client, worker) = peer(vec![("POST", response(200, body))]);
            let err = client.publish(&instance).unwrap_err();
            assert!(matches!(err, Failure::Invalid(_)), "{body}");
            assert!(!err.retryable(), "{body}");
            worker.join().unwrap();
        }
    }

    /// Publication takes over a departed host and restores the discoverable entry.
    #[test]
    fn test_publication_takes_over_after_the_registry_host_exits() {
        // Reserve an isolated registry address, then let its previous host go
        let old_host = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = Client {
            address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, old_host.local_addr().unwrap().port()),
        };
        drop(old_host);
        assert!(client.list().unwrap().is_empty());

        // The next publication creates a registry and registers through native IPC
        let instance: Instance = serde_json::from_str(
            r#"{"port":18181,"disk":"demo.ark","disk_id":"0123abcd","ready":false}"#,
        )
        .unwrap();
        client.publish(&instance).unwrap();
        let listing = client.list().unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].disk, "demo.ark");

        // Browser discovery reflects the same entry written through native IPC
        let mut browser = TcpStream::connect(client.address).unwrap();
        browser.set_read_timeout(Some(TIMEOUT)).unwrap();
        write!(
            browser,
            "GET /v1/instances HTTP/1.0\r\nHost: {}\r\n\r\n",
            client.address
        )
        .unwrap();
        let mut reply = Vec::new();
        browser.read_to_end(&mut reply).unwrap();
        let public = parse_listing(&split_response(&reply).unwrap()).unwrap();
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].disk, listing[0].disk);
    }

    /// A browser listener alone cannot receive a fallback registry request.
    #[test]
    fn test_missing_native_endpoint_does_not_fall_back_to_http() {
        let browser = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, browser.local_addr().unwrap().port());
        browser.set_nonblocking(true).unwrap();
        let err = connect(addr, Instant::now() + Duration::from_millis(100))
            .err()
            .unwrap();
        assert!(!err.retryable());
        assert!(err.to_string().contains("update and restart"), "{err}");
        while let Ok((mut probe, _)) = browser.accept() {
            probe.set_read_timeout(Some(TIMEOUT)).unwrap();
            assert_eq!(probe.read(&mut [0]).unwrap(), 0);
        }
    }

    /// Discovery keeps HTTP failures, invalid JSON and unsupported versions visible.
    #[test]
    fn test_discovery_failures_are_not_empty_listings() {
        for reply in [
            response(503, "registry unavailable"),
            response(200, "not json"),
            response(200, r#"{"version":999,"instances":[]}"#),
        ] {
            let (client, worker) = peer(vec![("GET", reply.clone())]);
            let err = client.list().unwrap_err();
            assert!(!err.retryable(), "{reply}");
            worker.join().unwrap();
        }
    }

    /// A registry that accepts a connection but never replies is a discovery failure.
    #[test]
    fn test_a_stalled_registry_is_not_an_empty_listing_or_a_handover() {
        let (client, server, _reservation) = crate::ipc::testing::server();
        let (release, held) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            held.recv().unwrap();
            drop(request);
        });

        // Hold the reply until the client reports its own I/O timeout
        let err = client.list().unwrap_err();
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(matches!(&err, Failure::Transport(cause)
            if matches!(cause.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)));
        assert!(!err.retryable());
    }

    /// Native clients send the protocol marker for every write and leave reads open.
    #[test]
    fn test_registry_requests_carry_the_write_guard() {
        // Inspect native requests without a running emulator
        let (client, server, _reservation) = crate::ipc::testing::server();
        let address = client.address;
        let worker = thread::spawn(move || {
            for method in ["GET", "POST", "DELETE"] {
                let incoming = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert_eq!(incoming.method().as_str(), method);
                let guards: Vec<_> = incoming
                    .headers()
                    .iter()
                    .filter(|header| header.field.equiv("X-Ark-Registry"))
                    .map(|header| header.value.as_str())
                    .collect();
                if method == "GET" {
                    assert!(guards.is_empty());
                } else {
                    assert_eq!(guards, ["1"]);
                }
                assert!(
                    !incoming
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("Origin"))
                );
                incoming.respond(tiny_http::Response::empty(204)).unwrap();
            }
        });

        // Cover listing, publication and withdrawal through the shared client
        for (method, path, body) in [
            ("GET", "/v1/instances", None),
            (
                "POST",
                "/v1/instances",
                Some(
                    br#"{"port":18181,"disk":"demo.ark","disk_id":"0123abcd","ready":true}"#
                        .as_slice(),
                ),
            ),
            ("DELETE", "/v1/instances/18181", None),
        ] {
            request(address, method, path, body)
                .unwrap_or_else(|err| panic!("{method} {path}: {err}"));
        }
        worker.join().unwrap();
    }

    #[test]
    fn test_a_body_is_split_off_the_headers() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        assert_eq!(split_response(raw).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn test_an_empty_body_is_not_an_error() {
        let raw = b"HTTP/1.1 204 No Content\r\n\r\n";
        assert!(split_response(raw).unwrap().is_empty());
    }

    #[test]
    fn test_a_refusal_is_an_error() {
        let raw = b"HTTP/1.1 400 Bad Request\r\n\r\nmalformed body";
        let err = split_response(raw).unwrap_err().to_string();
        assert!(err.contains("400"), "{err}");
    }

    #[test]
    fn test_a_truncated_answer_is_an_error() {
        assert!(split_response(b"HTTP/1.1 200 OK\r\nContent-Type: x").is_err());
    }

    /// A listing of another version is refused whole, since nothing in it can
    /// be trusted to mean what this build thinks.
    #[test]
    fn test_a_listing_of_another_version_is_refused() {
        let err = parse_listing(br#"{"version": 999, "instances": []}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("999"), "{err}");
        assert!(parse_listing(br#"{"instances": []}"#).is_err());
    }

    /// One entry this build cannot read hides no other.
    #[test]
    fn test_a_malformed_entry_does_not_hide_the_rest() {
        let body = br#"{"version": 1, "instances": [
            {"port": "not a port"},
            {"port": 18182, "disk": "b.ark", "disk_id": "02", "ready": true}
        ]}"#;
        let instances = parse_listing(body).unwrap();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].port, 18182);
    }

    #[test]
    fn test_disk_id_distinguishes_images() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_ne!(
            disk_id(&tmp.path().join("a.ark")),
            disk_id(&tmp.path().join("b.ark"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_disk_id_distinguishes_paths_that_are_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let a = Path::new(OsStr::from_bytes(b"/tmp/\xff.ark"));
        let b = Path::new(OsStr::from_bytes(b"/tmp/\xfe.ark"));
        assert_ne!(disk_id(a), disk_id(b));
    }

    #[test]
    fn test_disk_id_agrees_across_spellings_of_one_image() {
        // Canonicalization needs the file to exist.
        let tmp = tempfile::TempDir::new().unwrap();
        let direct = tmp.path().join("ark.ark");
        std::fs::write(&direct, b"").unwrap();
        let indirect = tmp.path().join("sub").join("..").join("ark.ark");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        assert_eq!(disk_id(&direct), disk_id(&indirect));
    }

    #[test]
    fn test_the_emulator_holding_an_image_is_found_by_its_identity() {
        let tmp = tempfile::TempDir::new().unwrap();
        let image = tmp.path().join("a.ark");
        let instances = [Instance {
            port: 18181,
            control: None,
            disk: "a.ark".into(),
            disk_id: disk_id(&image),
            ready: true,
            env: None,
            name: None,
            serial: None,
            expiry: None,
        }];
        assert_eq!(booted(&instances, &image).map(|i| i.port), Some(18181));
        assert!(booted(&instances, &tmp.path().join("b.ark")).is_none());
    }
}
