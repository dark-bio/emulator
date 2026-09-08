//! The registry of emulators running on this machine, served over HTTP on a
//! fixed loopback port so that anything wanting to talk to an emulator can find
//! one without guessing ports.
//!
//! There is no separate daemon. The registry is hosted by whichever launcher
//! currently holds the port, and every launcher tries to take it:
//!
//!   launcher A ── binds :18180 ──▶ [registry thread]
//!   launcher B ── bind fails ────▶ POST /v1/instances ──▶ ─┤
//!   launcher C ── bind fails ────▶ POST /v1/instances ──▶ ─┤
//!                                                          │
//!   consumers  ───────────────── GET  /v1/instances ──▶ ───┘
//!
//! That is enough to give the registry a lifetime independent of any *one*
//! emulator, which is all it ever needed. A process of its own would also have
//! needed spawning detached on three platforms, a reason to shut down, and a
//! place in the packaging.
//!
//! Ownership is settled by whoever binds first, with no locking and nothing
//! left behind by a launcher killed mid-startup. When the host exits, the port
//! frees, the next heartbeat from a surviving launcher fails, and that one
//! takes over (see [`crate::discovery`]). The registry it starts with is empty
//! and refills itself, because every launcher publishes only its own entry and
//! keeps republishing it.
//!
//! Entries live only as long as they are refreshed. Registration doubles as a
//! heartbeat, so an emulator that is killed outright disappears on its own with
//! no liveness probing, of which there is no portable kind.
//!
//! What an entry carries is deliberately narrow. Any page in any browser can
//! read a loopback port, and the allowed origin here has to be `*`, so the
//! registry publishes a disk image's file name but never its path.

use std::collections::HashMap;
use std::io::{Cursor, Read as _};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::diagnostics::log;

/// Port the registry is served on. One below the first port an emulator takes,
/// so the whole emulator range reads as one contiguous block.
pub(crate) const REGISTRY_PORT: u16 = 18180;

/// Schema version of the registry's responses, so a consumer meeting an
/// registry older than itself can tell rather than guess. Bumped only for a
/// breaking change; new fields are additive and do not need one.
const SCHEMA_VERSION: u32 = 1;

/// How long an entry survives without being refreshed. Three times the
/// heartbeat interval in [`crate::discovery`], so a single missed beat, or a
/// launcher briefly too busy to send one, does not evict a live emulator.
const ENTRY_TTL: Duration = Duration::from_secs(15);

/// How often the serve loop wakes up with no request to handle, which is what
/// bounds how late an entry's expiry can be.
const TICK: Duration = Duration::from_millis(500);

/// Largest request body the registry will read. Registrations are a few hundred
/// bytes; anything beyond this is not one of ours.
const MAX_BODY: usize = 8 * 1024;

/// A running emulator, as published to whoever asks. Also the body a launcher
/// registers itself with, so the two never drift apart.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Instance {
    /// Host port SLIRP forwards into this emulator's guest. Both the entry's
    /// identity in the registry and what a consumer connects to.
    pub(crate) port: u16,

    /// File name of the backing disk image, which is what tells two emulators
    /// apart while neither has been named or onboarded. Never the full path:
    /// see the module docs.
    pub(crate) disk: String,

    /// Opaque digest of the disk image's full path, which is what lets a
    /// launcher check whether an image is already booted without the registry
    /// having to publish where anybody's images live.
    pub(crate) disk_id: String,

    /// Whether the firmware has booted far enough to accept a client. False
    /// while the guest is still coming up.
    pub(crate) ready: bool,

    /// Cloud environment the device is bound to, once it has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) env: Option<String>,

    /// Name the device has been given, if it has been given one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,

    /// Serial the device reports, absent until it has been onboarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) serial: Option<String>,

    /// When the device's identity stops being valid, as a Unix timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expiry: Option<u64>,
}

/// The registry's answer to a listing request.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Listing {
    /// Schema version of this response; see [`SCHEMA_VERSION`].
    pub(crate) version: u32,

    /// Every emulator that has been heard from recently enough.
    pub(crate) instances: Vec<Instance>,
}

/// An entry together with when it was last refreshed, which is the only thing
/// keeping it alive.
struct Entry {
    instance: Instance,
    seen: Instant,
}

/// The registry itself: every emulator that has been heard from, keyed by the
/// port it holds.
struct Registry {
    entries: HashMap<u16, Entry>,
}

impl Registry {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Add or refresh an entry. A re-registration replaces the whole record
    /// rather than merging into it, so an emulator that clears a claim (a
    /// device being renamed to nothing, say) is not stuck advertising the old
    /// value.
    fn upsert(&mut self, instance: Instance) {
        self.entries.insert(
            instance.port,
            Entry {
                instance,
                seen: Instant::now(),
            },
        );
    }

    /// Drop an entry, if it is there. Idempotent: a launcher that both
    /// deregisters and then stops heartbeating is the common case.
    fn remove(&mut self, port: u16) {
        self.entries.remove(&port);
    }

    /// Forget entries that have not been refreshed within [`ENTRY_TTL`].
    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.seen) < ENTRY_TTL);
    }

    /// Every live entry, in port order so a consumer's list does not reshuffle
    /// itself between polls.
    fn listing(&self) -> Listing {
        let mut instances: Vec<Instance> = self
            .entries
            .values()
            .map(|entry| entry.instance.clone())
            .collect();
        instances.sort_by_key(|instance| instance.port);
        Listing {
            version: SCHEMA_VERSION,
            instances,
        }
    }
}

/// Serve the registry from this process, if nobody else already is. Returns
/// whether this process is now the host.
///
/// Losing the race for the port is the ordinary outcome and not a failure: it
/// means another launcher is serving, which is what the caller wanted. Failing
/// for any other reason is reported and treated the same way, since discovery
/// is never worth refusing to boot over.
///
/// Loopback only. The registry describes what is running on this machine and
/// has no business being reachable from off it.
pub(crate) fn host() -> bool {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, REGISTRY_PORT);
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return false,
        Err(e) => {
            log!("[registry] could not bind {addr}: {e}");
            return false;
        }
    };

    // tiny_http takes an already-bound listener, so the bind above stays the
    // thing that decides the race rather than anything inside the server.
    let server = match Server::from_listener(listener, None::<tiny_http::SslConfig>) {
        Ok(server) => server,
        Err(e) => {
            log!("[registry] could not serve on {addr}: {e}");
            return false;
        }
    };
    log!("[registry] hosting the registry on {addr}");

    // Runs for the life of the process. There is no shutdown condition to
    // reach: the registry exists for exactly as long as a launcher is holding
    // the port, and this launcher exiting is what hands it to the next one.
    thread::spawn(move || serve(&server, &Arc::new(Mutex::new(Registry::new()))));
    true
}

/// The serve loop, waking on [`TICK`] even with nothing to answer so that
/// entries expire on time rather than only when somebody asks.
fn serve(server: &Server, registry: &Arc<Mutex<Registry>>) {
    loop {
        match server.recv_timeout(TICK) {
            Ok(Some(request)) => handle(request, registry),
            Ok(None) => {}
            // A failed accept says nothing about the other clients, so keep
            // serving rather than taking every emulator's discovery down with
            // one bad connection.
            Err(e) => log!("[registry] could not accept a request: {e}"),
        }
        with_registry(registry, |reg| reg.expire(Instant::now()));
    }
}

/// Route one request. Every answer carries the CORS headers, including the
/// error ones, so a browser can read the reason rather than an opaque failure.
fn handle(mut request: Request, registry: &Arc<Mutex<Registry>>) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();

    let response = match (&method, path.as_str()) {
        // Preflight. Answered for any path so that a consumer probing a route
        // this build does not have still gets a CORS answer to read the 404
        // through.
        (Method::Options, _) => empty(StatusCode(204)),

        (Method::Get, "/v1/instances") => {
            let listing = with_registry(registry, |reg| reg.listing());
            match serde_json::to_vec(&listing) {
                Ok(body) => json(body),
                Err(e) => text(
                    StatusCode(500),
                    &format!("could not encode the registry: {e}"),
                ),
            }
        }

        (Method::Post, "/v1/instances") => match read_body(&mut request) {
            Ok(body) => match serde_json::from_slice::<Instance>(&body) {
                Ok(instance) => {
                    with_registry(registry, |reg| reg.upsert(instance));
                    empty(StatusCode(204))
                }
                Err(e) => text(StatusCode(400), &format!("malformed body: {e}")),
            },
            Err(response) => response,
        },

        (Method::Delete, _) => match path.strip_prefix("/v1/instances/") {
            Some(port) => match port.parse::<u16>() {
                Ok(port) => {
                    with_registry(registry, |reg| reg.remove(port));
                    empty(StatusCode(204))
                }
                Err(_) => text(StatusCode(400), "not a port number"),
            },
            None => text(StatusCode(404), "no such route"),
        },

        _ => text(StatusCode(404), "no such route"),
    };

    respond(request, response);
}

/// Run `f` against the registry, recovering a lock poisoned by a panic in
/// another request's handler. A poisoned registry is still a usable one, and
/// dropping every emulator's discovery over one bad request would be worse.
fn with_registry<T>(registry: &Arc<Mutex<Registry>>, f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = match registry.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Read a request's body, capped at [`MAX_BODY`]. Borrows rather than consumes
/// so that a body which fails to read is still something we can answer.
fn read_body(request: &mut Request) -> Result<Vec<u8>, Response<Cursor<Vec<u8>>>> {
    if request.body_length().is_some_and(|len| len > MAX_BODY) {
        return Err(text(StatusCode(413), "body too large"));
    }
    let mut body = Vec::new();
    match request
        .as_reader()
        .take(MAX_BODY as u64)
        .read_to_end(&mut body)
    {
        Ok(_) => Ok(body),
        Err(e) => Err(text(
            StatusCode(400),
            &format!("could not read the body: {e}"),
        )),
    }
}

/// Headers every response carries.
///
/// The allowed origin is `*` because the consumers are pages served from
/// wherever they happen to be served from, and an allowlist would mean baking
/// somebody's hostnames in here. What keeps that acceptable is the shape of an
/// [`Instance`]: ports and file names of things already running on this
/// machine, and no paths.
///
/// `Access-Control-Allow-Private-Network` is for Chromium's private network
/// access rules, under which a page on a public origin reaching a loopback
/// address must preflight and be told, explicitly, that the loopback service
/// meant to be reachable.
fn cors() -> Vec<Header> {
    [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS"),
        ("Access-Control-Allow-Headers", "Content-Type"),
        ("Access-Control-Allow-Private-Network", "true"),
        ("Access-Control-Max-Age", "600"),
    ]
    .iter()
    .filter_map(|(name, value)| Header::from_bytes(name.as_bytes(), value.as_bytes()).ok())
    .collect()
}

/// A JSON response.
fn json(body: Vec<u8>) -> Response<Cursor<Vec<u8>>> {
    let mut response = Response::from_data(body).with_status_code(StatusCode(200));
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
        response.add_header(header);
    }
    response
}

/// A plain-text response, for the cases a consumer can only log.
fn text(status: StatusCode, message: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(message.as_bytes().to_vec()).with_status_code(status)
}

/// A response with no body, for the routes whose answer is their status code.
fn empty(status: StatusCode) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(Vec::new()).with_status_code(status)
}

/// Answer `request`, attaching the CORS headers on the way out. A client that
/// has already hung up is not worth reporting: the registry is polled, and the
/// next poll is along shortly.
fn respond(request: Request, mut response: Response<Cursor<Vec<u8>>>) {
    for header in cors() {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(port: u16) -> Instance {
        Instance {
            port,
            disk: "ark-disk.img".into(),
            disk_id: "0123abcd".into(),
            ready: false,
            env: None,
            name: None,
            serial: None,
            expiry: None,
        }
    }

    #[test]
    fn test_expiry_drops_a_stale_entry() {
        let mut registry = Registry::new();
        registry.upsert(instance(18181));

        let now = Instant::now();
        registry.expire(now);
        assert_eq!(registry.listing().instances.len(), 1);

        registry.expire(now + ENTRY_TTL);
        assert!(registry.listing().instances.is_empty());
    }

    #[test]
    fn test_a_refresh_keeps_an_entry_alive() {
        let mut registry = Registry::new();
        registry.upsert(instance(18181));

        // A heartbeat lands, and the entry outlives what would have been its
        // original deadline.
        registry.upsert(instance(18181));
        registry.expire(Instant::now());
        assert_eq!(registry.listing().instances.len(), 1);
    }

    #[test]
    fn test_a_refresh_replaces_rather_than_merges() {
        let mut registry = Registry::new();
        let mut named = instance(18181);
        named.name = Some("ark".into());
        registry.upsert(named);

        registry.upsert(instance(18181));
        assert_eq!(registry.listing().instances[0].name, None);
    }

    #[test]
    fn test_removing_an_entry_is_idempotent() {
        let mut registry = Registry::new();
        registry.upsert(instance(18181));
        registry.remove(18181);
        registry.remove(18181);
        assert!(registry.listing().instances.is_empty());
    }

    #[test]
    fn test_listing_is_ordered_by_port() {
        let mut registry = Registry::new();
        for port in [18183, 18181, 18182] {
            registry.upsert(instance(port));
        }
        let ports: Vec<u16> = registry
            .listing()
            .instances
            .iter()
            .map(|instance| instance.port)
            .collect();
        assert_eq!(ports, [18181, 18182, 18183]);
    }

    #[test]
    fn test_an_instance_survives_a_json_roundtrip() {
        let mut original = instance(18182);
        original.ready = true;
        original.name = Some("test ark".into());
        original.serial = Some("abc123".into());

        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: Instance = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.port, 18182);
        assert!(decoded.ready);
        assert_eq!(decoded.name.as_deref(), Some("test ark"));
        assert_eq!(decoded.serial.as_deref(), Some("abc123"));
        // Claims the device has not made are left out rather than sent as null.
        assert!(!String::from_utf8(encoded).unwrap().contains("expiry"));
    }

    #[test]
    fn test_the_preflight_headers_cover_private_network_access() {
        let names: Vec<String> = cors()
            .iter()
            .map(|header| header.field.as_str().as_str().to_ascii_lowercase())
            .collect();
        assert!(names.contains(&"access-control-allow-origin".to_string()));
        assert!(names.contains(&"access-control-allow-private-network".to_string()));
    }

    #[test]
    fn test_hosting_twice_from_one_process_is_refused() {
        // The second call is the one a heartbeat makes after a failed publish.
        // A launcher already hosting must not start a second server over the
        // top of its own, which its own bound port is what prevents.
        if !host() {
            // The port is held by something else, which the test below covers.
            return;
        }
        assert!(!host());
    }

    #[test]
    fn test_only_one_launcher_can_host_the_registry() {
        // Whoever binds first serves; the rest report a lost race rather than
        // an error, which is what lets every launcher try unconditionally.
        let Ok(held) = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, REGISTRY_PORT))
        else {
            // Something on this machine is already holding the port, which is
            // the case being asserted anyway.
            assert!(!host());
            return;
        };
        assert!(!host());
        drop(held);
    }
}
