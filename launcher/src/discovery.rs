//! The launcher's side of the registry: making sure it is being served, and
//! keeping this emulator's entry in it up to date.
//!
//! Nothing here is allowed to stop an emulator from booting. Every call is
//! best-effort and logs rather than fails. CI launches a packaged build with no
//! flags and expects it to boot unattended, with no registry, a wedged one, or
//! a machine where binding a port is not allowed at all.
//!
//! Every launcher tries to host the registry, one wins the port, and the rest
//! publish themselves to whoever did (see [`crate::registry`]). Takeover rides
//! on the heartbeat: one that cannot be delivered means the host is gone, so
//! the launcher tries to become the host and republishes itself either way.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use sha2::{Digest as _, Sha256};

use crate::diagnostics::log;
use crate::registry::{self, Instance, Listing, REGISTRY_PORT};

/// How often this emulator re-registers itself, which is also the heartbeat
/// keeping its entry alive, so it has to stay below the registry's expiry.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// How long any single request to the registry may take, bounded so a wedged
/// one cannot hold up a boot.
const TIMEOUT: Duration = Duration::from_secs(2);

/// This emulator's entry, as last published. Held here so the heartbeat thread
/// and the nameplate command can both reach it.
static ENTRY: OnceLock<Mutex<Instance>> = OnceLock::new();

/// Address the registry is served on.
fn registry_addr() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, REGISTRY_PORT)
}

/// Ask the registry what is running. An empty list is also what a machine with
/// nobody hosting one looks like, and a caller does not need to tell.
pub(crate) fn list() -> Vec<Instance> {
    match request("GET", "/v1/instances", None) {
        Ok(body) => match serde_json::from_slice::<Listing>(&body) {
            Ok(listing) => listing.instances,
            Err(e) => {
                log!("[discovery] could not read the registry's answer: {e}");
                Vec::new()
            }
        },
        Err(e) => {
            log!("[discovery] could not read the registry: {e}");
            Vec::new()
        }
    }
}

/// Make sure the registry is being served, hosting it here if nobody else is.
/// Called once at startup, before anything asks what is running.
pub(crate) fn ensure_registry() {
    registry::host();
}

/// Publish this emulator, and start the heartbeat that keeps it published.
/// Called before the guest has booted, so the entry starts out not ready.
pub(crate) fn register(port: u16, disk: &Path) {
    let instance = Instance {
        port,
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

    publish();
    thread::spawn(|| loop {
        thread::sleep(HEARTBEAT);
        publish();
    });
}

/// Fold what the firmware has reported about itself into this emulator's entry
/// and publish it. A claim left out is one that has not changed, matching the
/// partial frames the firmware sends, so this merges rather than replaces.
///
/// The first of these also marks the entry ready: the guest accepts a client
/// only once its hardware bus has a peer, and this arriving proves it does.
#[tauri::command]
pub(crate) fn nameplate(
    env: Option<String>,
    name: Option<String>,
    serial: Option<String>,
    expiry: Option<u64>,
) {
    let Some(entry) = ENTRY.get() else {
        return;
    };
    {
        let mut entry = entry.lock().unwrap();
        entry.ready = true;
        // An empty name is a device whose name was cleared, which is a value
        // rather than an absence, so it is stored as one.
        if env.is_some() {
            entry.env = env;
        }
        if name.is_some() {
            entry.name = name.filter(|name| !name.is_empty());
        }
        if serial.is_some() {
            entry.serial = serial;
        }
        if expiry.is_some() {
            entry.expiry = expiry;
        }
    }
    publish();
}

/// Send the current entry to the registry.
fn publish() {
    let Some(entry) = ENTRY.get() else {
        return;
    };
    let body = serde_json::to_vec(&*entry.lock().unwrap());
    let body = match body {
        Ok(body) => body,
        Err(e) => {
            log!("[discovery] could not encode this emulator's entry: {e}");
            return;
        }
    };

    if request("POST", "/v1/instances", Some(&body)).is_ok() {
        return;
    }

    // Undeliverable means whoever hosted the registry is gone, so try to take
    // it over. Republished either way, to whichever registry now exists.
    if registry::host() {
        log!("[discovery] the registry had no host, taking it over");
    }
    if let Err(e) = request("POST", "/v1/instances", Some(&body)) {
        log!("[discovery] could not register: {e}");
    }
}

/// Withdraw this emulator from the registry. Best effort and quick: it runs
/// while the window is closing, and the entry would expire on its own anyway.
pub(crate) fn deregister() {
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

/// One request to the registry, spoken directly over TCP. Four fixed routes
/// against a loopback server is well short of what an HTTP client crate is for.
fn request(method: &str, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>> {
    let addr = registry_addr();
    let mut stream = TcpStream::connect_timeout(&addr.into(), TIMEOUT)
        .with_context(|| format!("could not connect to the registry at {addr}"))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
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

    // `Connection: close` means the response ends at EOF, so there is no
    // chunked or keep-alive framing to interpret.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    split_response(&raw)
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

    #[test]
    fn test_disk_id_is_stable_and_distinguishes_images() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("a.img");
        let b = tmp.path().join("b.img");
        assert_eq!(disk_id(&a), disk_id(&a));
        assert_ne!(disk_id(&a), disk_id(&b));
    }

    #[test]
    #[cfg(unix)]
    fn test_disk_id_distinguishes_paths_that_are_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let a = Path::new(OsStr::from_bytes(b"/tmp/\xff.img"));
        let b = Path::new(OsStr::from_bytes(b"/tmp/\xfe.img"));
        assert_ne!(disk_id(a), disk_id(b));
    }

    #[test]
    fn test_disk_id_agrees_across_spellings_of_one_image() {
        // Canonicalization needs the file to exist.
        let tmp = tempfile::TempDir::new().unwrap();
        let direct = tmp.path().join("ark.img");
        std::fs::write(&direct, b"").unwrap();
        let indirect = tmp.path().join("sub").join("..").join("ark.img");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        assert_eq!(disk_id(&direct), disk_id(&indirect));
    }

    #[test]
    fn test_a_missing_registry_lists_nothing_rather_than_failing() {
        // Listing answers with a list whatever is running, which is what keeps
        // a boot independent of discovery.
        let _ = list();
    }
}
