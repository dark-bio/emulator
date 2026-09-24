// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The launcher's side of the registry: reading it, keeping this emulator's
//! entry in it up to date, and the small HTTP transport both need.
//!
//! Nothing here is allowed to stop an emulator from booting. Publishing is
//! best-effort and logs rather than fails. CI launches a packaged build with no
//! flags and expects it to boot unattended, with no registry, a wedged one, or
//! a machine where binding a port is not allowed at all. Reading is strict
//! where a command needs it to be: no registry is an empty list, while one
//! that answers and cannot be read is an error.
//!
//! Every launcher tries to host the registry, one wins the port, and the rest
//! publish themselves to whoever did (see [`crate::registry`]). Takeover rides
//! on the heartbeat: one that cannot be delivered means the host is gone, so
//! the launcher tries to become the host and republishes itself either way.
//!
//! The heartbeat is also how a shutdown arrives. The registry answers a beat
//! with whatever has been left for this emulator, and a stop waiting there
//! takes it down the way closing its window does. A launcher that hosts the
//! registry beats to itself over the loopback like every other one, so it
//! reads its own mailbox on the same path.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use sha2::{Digest as _, Sha256};

use crate::diagnostics::{log, trace};
use crate::hardware::Controller;
use crate::registry::{self, Beat, Instance, REGISTRY_PORT, SCHEMA_VERSION};

/// How often this emulator re-registers itself. It is the heartbeat keeping
/// its entry alive, so it has to stay well below the registry's expiry, and it
/// is also how long a stop takes to arrive.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(1);

/// How long any single request to the registry may take, bounded so a wedged
/// one cannot hold up a boot.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The most a registry answer may be. A hundred entries are a few kilobytes;
/// anything near this is not a registry.
const MAX_RESPONSE: u64 = 1 << 20;

/// This emulator's entry, shared by the heartbeat and shutdown.
static ENTRY: OnceLock<Mutex<Instance>> = OnceLock::new();

/// Serializes publication with withdrawal and prevents a stopped guest returning.
static PUBLISHING: Mutex<bool> = Mutex::new(true);

/// Address the registry is served on.
fn registry_addr() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, REGISTRY_PORT)
}

/// Ask the registry what is running. Nothing serving one is a computer with no
/// emulators on it, so a connection that cannot be made answers with an empty
/// list. Any failure after that, an answer that cannot be read, a version
/// this build does not know, a host that stops answering, is reported, since
/// something is there and it is not a registry this build can trust.
pub(crate) fn list() -> Result<Vec<Instance>> {
    let body = match request("GET", "/v1/instances", None) {
        Ok(body) => body,
        Err(Refused) => return Ok(Vec::new()),
        Err(Failed(err)) => return Err(err),
    };
    parse_listing(&body)
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

/// Ask the emulator on `port` to shut down. The request waits in the registry
/// until that emulator's next heartbeat collects it.
pub(crate) fn request_stop(port: u16) -> Result<()> {
    request("POST", &format!("/v1/instances/{port}/stop"), None)?;
    Ok(())
}

/// Publish this emulator, and start the heartbeat that keeps it published.
/// Each heartbeat takes readiness and identity from the latest hardware state.
pub(crate) fn register(
    port: u16,
    disk: &Path,
    hardware: Controller,
    control: crate::control::Endpoint,
) {
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
        return;
    }

    publish(&hardware);
    thread::spawn(move || {
        loop {
            thread::sleep(HEARTBEAT);
            publish(&hardware);
        }
    });
}

/// Send the current entry to the registry, and act on whatever comes back.
fn publish(hardware: &Controller) {
    let publishing = PUBLISHING.lock().unwrap();
    if !*publishing || crate::runtime::stopping() {
        return;
    }
    let Some(entry) = ENTRY.get() else {
        return;
    };
    let body = {
        let state = hardware.snapshot();
        let mut entry = entry.lock().unwrap();
        entry.ready = state.connected && state.nameplate.known;
        entry.env = state.nameplate.env;
        entry.name = state.nameplate.name;
        entry.serial = state.nameplate.serial;
        entry.expiry = state.nameplate.expiry;
        serde_json::to_vec(&*entry)
    };
    let body = match body {
        Ok(body) => body,
        Err(e) => {
            log!("[discovery] could not encode this emulator's entry: {e}");
            return;
        }
    };

    if let Ok(answer) = request("POST", "/v1/instances", Some(&body)) {
        drop(publishing);
        obey(&answer);
        return;
    }

    // Undeliverable means whoever hosted the registry is gone, so try to take
    // it over. Republished either way, to whichever registry now exists.
    if registry::host() {
        log!("[discovery] the registry had no host, taking it over");
    }
    let answer = request("POST", "/v1/instances", Some(&body));
    drop(publishing);
    match answer {
        Ok(answer) => obey(&answer),
        Err(err) => log!("[discovery] could not register: {err}"),
    }
}

/// Act on what the registry answered a heartbeat with. A stop waiting there is
/// the only thing it can carry, and an answer with no body carries nothing.
fn obey(answer: &[u8]) {
    if serde_json::from_slice::<Beat>(answer).is_ok_and(|beat| beat.stop) {
        log!("[discovery] asked to shut down");
        crate::runtime::shut_down(0);
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
    if let Err(e) = request("DELETE", &format!("/v1/instances/{port}"), None) {
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

/// Why a request got no answer: nobody is serving the registry, or something
/// is and the exchange failed.
enum Unanswered {
    /// No connection could be made, which is a computer with no registry. A
    /// closed loopback port is refused on Unix and left to time out on
    /// Windows, so the kind of the error does not matter, only that nothing
    /// ever answered.
    Refused,
    /// Anything after a connection was made, which is a registry that could
    /// not be used.
    Failed(anyhow::Error),
}

use Unanswered::{Failed, Refused};

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused => write!(f, "nothing is serving the registry"),
            Failed(err) => write!(f, "{err:#}"),
        }
    }
}

impl From<Unanswered> for anyhow::Error {
    fn from(unanswered: Unanswered) -> Self {
        match unanswered {
            Refused => anyhow::anyhow!("nothing is serving the registry"),
            Failed(err) => err,
        }
    }
}

/// One request to the registry, spoken directly over TCP. Four fixed routes
/// against a loopback server is well short of what an HTTP client crate is
/// for. Spoken as HTTP/1.0, so the answer ends at end of file and carries no
/// chunked framing, and bounded, so a server that is not a registry cannot
/// feed this forever.
fn request(method: &str, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>, Unanswered> {
    let addr = registry_addr();
    let mut stream = TcpStream::connect_timeout(&addr.into(), TIMEOUT).map_err(|_| Refused)?;
    let mut exchange = || -> Result<Vec<u8>> {
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;

        let mut head = format!(
            "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\
             Content-Length: {}\r\n",
            body.map_or(0, <[u8]>::len)
        );
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes())?;
        if let Some(body) = body {
            stream.write_all(body)?;
        }
        stream.flush()?;

        let mut raw = Vec::new();
        (&mut stream).take(MAX_RESPONSE).read_to_end(&mut raw)?;
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
    exchange().map_err(Failed)
}

/// Pull the body out of a response, failing on any status the registry uses to
/// say no. The only server on the other end is [`crate::registry`], which
/// answers with a status line, a few headers and an unencoded body.
fn split_response(raw: &[u8]) -> Result<Vec<u8>> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("the registry's answer had no header block")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .context("the registry's answer had no status")?;
    if !(200..300).contains(&status) {
        bail!("the registry answered {status}");
    }
    Ok(raw[split + 4..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

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
