// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

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
//! That gives the registry a lifetime independent of any one emulator, which
//! is all it needed. A process of its own would have needed spawning detached
//! on three platforms, a reason to shut down, and a place in the packaging.
//!
//! When the host exits, the port frees and the next launcher whose heartbeat
//! fails takes over (see [`super::discovery`]). Its registry starts empty and
//! refills, since every launcher keeps republishing its own entry. Entries live
//! only as long as they are refreshed, so an emulator killed outright drops out
//! on its own, with no liveness probing, of which there is no portable kind.
//!
//! Any page in any browser can read a loopback port, and the allowed origin
//! here has to be `*`, so the registry publishes a disk image's file name but
//! never its path.
//!
//! Writes require a fixed header that CORS preflights never allow, and reject
//! browser origins. This keeps web pages from changing the registry while
//! local processes can still publish, withdraw and stop emulators.
//!
//! The registry is also the mailbox a shutdown travels through. A request to
//! stop an emulator is recorded against its entry, and the next heartbeat from
//! that launcher is answered with it. Nothing here signals, reaches for a pid
//! or opens a second channel, so no launcher has to be special.

use std::collections::HashMap;
use std::io::{Cursor, Read as _};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::diagnostics::log;

/// Port the registry is served on. One below the first port an emulator takes,
/// so the whole emulator range reads as one contiguous block.
pub(crate) const REGISTRY_PORT: u16 = 18180;

/// Schema version of the registry's responses, so a consumer meeting an older
/// registry can tell rather than guess. Bumped only for a breaking change.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Header required on registry writes and excluded from CORS preflight grants.
pub(super) const WRITE_HEADER: &str = "X-Ark-Registry";

/// Fixed browser guard, with no authentication of local processes.
pub(super) const WRITE_HEADER_VALUE: &str = "1";

/// How long an entry survives without being refreshed. Comfortably more than
/// the heartbeat interval in [`super::discovery`], so a launcher that is busy
/// or beating slowly is not dropped between two of its beats.
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
    /// identity here and what a consumer connects to.
    pub(crate) port: u16,

    /// Direct launcher control, absent on builds that only support discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) control: Option<super::control::Endpoint>,

    /// File name of the backing disk image, which tells two emulators apart
    /// while neither is named or onboarded. Never the path: see the module
    /// docs.
    pub(crate) disk: String,

    /// Opaque digest of the disk image's full path, so a launcher can tell
    /// whether an image is already booted without the registry publishing
    /// where anybody's images live.
    pub(crate) disk_id: String,

    /// Whether the firmware has booted far enough to accept a client.
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

/// The registry's answer to a heartbeat, sent when it has something waiting
/// for that launcher. A heartbeat with nothing waiting is answered with no
/// body at all.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Beat {
    /// Whether this emulator has been asked to shut down.
    pub(crate) stop: bool,
}

/// An entry together with when it was last refreshed, which is the only thing
/// keeping it alive, and whatever is waiting to be handed to its launcher.
struct Entry {
    instance: Instance,
    seen: Instant,
    stop: bool,
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

    /// Add or refresh an entry, and answer with whatever is waiting for that
    /// launcher. A re-registration replaces the whole record rather than
    /// merging into it, so a cleared claim (a device renamed to nothing, say)
    /// does not linger. A stop that has been asked for is not part of the
    /// record and outlives the refresh, until the launcher acts on it.
    fn upsert(&mut self, instance: Instance) -> Beat {
        let stop = self
            .entries
            .get(&instance.port)
            .is_some_and(|entry| entry.stop);
        self.entries.insert(
            instance.port,
            Entry {
                instance,
                seen: Instant::now(),
                stop,
            },
        );
        Beat { stop }
    }

    /// Ask the emulator on `port` to shut down, which its next heartbeat picks
    /// up. Answers whether there is an emulator there to ask.
    fn request_stop(&mut self, port: u16) -> bool {
        match self.entries.get_mut(&port) {
            Some(entry) => {
                entry.stop = true;
                true
            }
            None => false,
        }
    }

    /// Drop an entry, if it is there. Idempotent: a launcher that deregisters
    /// and then stops heartbeating is the common case.
    fn remove(&mut self, port: u16) {
        self.entries.remove(&port);
    }

    /// Forget entries that have not been refreshed within [`ENTRY_TTL`].
    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.seen) < ENTRY_TTL);
    }

    /// Every live entry, in port order so a consumer's list does not reshuffle
    /// between polls.
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
/// Losing the race for the port is the ordinary outcome rather than a failure,
/// and any other failure is treated the same way: discovery is never worth
/// refusing to boot over.
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

    // tiny_http takes an already-bound listener, so the bind above is what
    // decides the race.
    let server = match Server::from_listener(listener, None::<tiny_http::SslConfig>) {
        Ok(server) => server,
        Err(e) => {
            log!("[registry] could not serve on {addr}: {e}");
            return false;
        }
    };
    log!("[registry] hosting the registry on {addr}");

    // Runs for the life of the process. This launcher exiting is what hands
    // the port to the next one.
    thread::spawn(move || serve(&server, &mut Registry::new()));
    true
}

/// The serve loop, waking on [`TICK`] even with nothing to answer so that
/// entries expire on time rather than only when somebody asks. One thread
/// owns the registry, so nothing here needs a lock.
fn serve(server: &Server, registry: &mut Registry) {
    loop {
        match server.recv_timeout(TICK) {
            Ok(Some(request)) => handle(request, registry),
            Ok(None) => {}
            // A failed accept says nothing about the other clients, so keep
            // serving.
            Err(e) => log!("[registry] could not accept a request: {e}"),
        }
        registry.expire(Instant::now());
    }
}

/// Route one request. Every answer carries the CORS headers, including the
/// error ones, so a browser can read the reason rather than an opaque failure.
fn handle(mut request: Request, registry: &mut Registry) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();

    // Refuse browser writes before parsing a body or changing registry state
    if matches!(method, Method::Post | Method::Delete) {
        let has_write_header = request
            .headers()
            .iter()
            .filter(|header| header.field.equiv(WRITE_HEADER))
            .map(|header| header.value.as_str())
            .eq([WRITE_HEADER_VALUE]);
        let refusal = if request
            .body_length()
            .is_some_and(|length| length > MAX_BODY)
        {
            Some(text(StatusCode(413), "body too large"))
        } else if request
            .headers()
            .iter()
            .any(|header| header.field.equiv("Origin"))
        {
            Some(text(StatusCode(403), "browser writes are not accepted"))
        } else if !has_write_header {
            Some(text(
                StatusCode(403),
                &format!("registry writes require {WRITE_HEADER}: {WRITE_HEADER_VALUE}"),
            ))
        } else {
            None
        };
        if let Some(response) = refusal {
            respond(request, response);
            return;
        }
    }

    let response = match (&method, path.as_str()) {
        // Preflight, answered for any path so a consumer probing a route this
        // build does not have can still read the 404.
        (Method::Options, _) => empty(StatusCode(204)),

        (Method::Get, "/v1/instances") => {
            let listing = registry.listing();
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
                Ok(instance) => beat(registry.upsert(instance)),
                Err(e) => text(StatusCode(400), &format!("malformed body: {e}")),
            },
            Err(response) => response,
        },

        (Method::Post, _) => match stop_route(&path) {
            Some(Ok(port)) => match registry.request_stop(port) {
                true => empty(StatusCode(204)),
                false => text(StatusCode(404), "no emulator on that port"),
            },
            Some(Err(())) => text(StatusCode(400), "not a port number"),
            None => text(StatusCode(404), "no such route"),
        },

        (Method::Delete, _) => match path.strip_prefix("/v1/instances/") {
            Some(port) => match port.parse::<u16>() {
                Ok(port) => {
                    registry.remove(port);
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

/// The port a stop request names, if this path is one. `Err` is a path in the
/// right shape whose port is not a number.
fn stop_route(path: &str) -> Option<Result<u16, ()>> {
    let port = path.strip_prefix("/v1/instances/")?.strip_suffix("/stop")?;
    Some(port.parse::<u16>().map_err(|_| ()))
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
/// The allowed origin is `*` because an allowlist would mean baking somebody's
/// hostnames in here. What keeps that acceptable is the shape of an
/// [`Instance`]: ports and file names, never paths.
/// Writes need [`WRITE_HEADER`], which this preflight never permits.
///
/// `Access-Control-Allow-Private-Network` is for Chromium's private network
/// access rules, under which a page on a public origin reaching a loopback
/// address must preflight and be told the service meant to be reachable.
fn cors() -> Vec<Header> {
    [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "GET, OPTIONS"),
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

/// The answer to a heartbeat, which carries a body only when the registry has
/// something for that launcher.
fn beat(beat: Beat) -> Response<Cursor<Vec<u8>>> {
    if !beat.stop {
        return empty(StatusCode(204));
    }
    match serde_json::to_vec(&beat) {
        Ok(body) => json(body),
        Err(e) => text(
            StatusCode(500),
            &format!("could not encode the answer: {e}"),
        ),
    }
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
/// has already hung up is not worth reporting.
fn respond(request: Request, mut response: Response<Cursor<Vec<u8>>>) {
    for header in cors() {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

/// Registry lifecycle and HTTP browser guards.
#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::TcpStream;

    use super::*;

    /// A registration body using the public registry contract.
    const INSTANCE: &str = r#"{"port":18181,"disk":"demo.ark","disk_id":"0123abcd","ready":true}"#;

    /// An HTTP reply received through the registry's real request handler.
    struct Reply {
        /// HTTP status returned by the handler.
        status: u16,
        /// Response headers indexed by lowercase name.
        headers: HashMap<String, String>,
        /// Unencoded response body.
        body: String,
    }

    /// Send a raw HTTP request through an isolated registry listener.
    fn exchange(
        registry: &mut Registry,
        method: &str,
        path: &str,
        headers: &str,
        body: &str,
    ) -> Reply {
        // Use a private port so these tests cannot touch a running emulator
        let server = Server::http((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let timeout = Duration::from_secs(2);
        let mut stream = TcpStream::connect_timeout(&address, timeout).unwrap();
        stream.set_read_timeout(Some(timeout)).unwrap();
        stream.set_write_timeout(Some(timeout)).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
        .unwrap();

        // Exercise routing and header parsing before inspecting the reply
        let request = server.recv_timeout(timeout).unwrap().unwrap();
        handle(request, registry);
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        let status = lines.next().unwrap().split_whitespace().nth(1).unwrap();
        Reply {
            status: status.parse().unwrap(),
            headers: lines
                .map(|line| {
                    let (name, value) = line.split_once(':').unwrap();
                    (name.to_ascii_lowercase(), value.trim().to_owned())
                })
                .collect(),
            body: body.to_owned(),
        }
    }

    /// Missing guards and browser origins cannot publish, withdraw or queue stops.
    #[test]
    fn test_rejected_writes_preserve_entries_and_pending_stops() {
        // Publish through HTTP so every rejected request starts from a real entry
        let mut registry = Registry::new();
        let allowed = "X-Ark-Registry: 1\r\n";
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", allowed, INSTANCE).status,
            204
        );
        let before = exchange(&mut registry, "GET", "/v1/instances", "", "").body;

        // Check each write, including simple POSTs that need no browser preflight
        for (case, headers) in [
            ("missing", ""),
            ("wrong", "X-Ark-Registry: 0\r\n"),
            ("empty", "X-Ark-Registry:\r\n"),
            ("repeated", "X-Ark-Registry: 1\r\nx-ark-registry: 1\r\n"),
            ("origin only", "Origin: https://example.com\r\n"),
            (
                "origin",
                "X-Ark-Registry: 1\r\noRiGiN: https://example.com\r\n",
            ),
            ("null origin", "X-Ark-Registry: 1\r\nOrigin: null\r\n"),
            ("empty origin", "X-Ark-Registry: 1\r\nOrigin:\r\n"),
        ] {
            for (method, path, body) in [
                (
                    "POST",
                    "/v1/instances",
                    r#"{"port":18181,"disk":"forged.ark","disk_id":"ffff","ready":false}"#,
                ),
                (
                    "POST",
                    "/v1/instances",
                    r#"{"port":18182,"disk":"extra.ark","disk_id":"ffff","ready":false}"#,
                ),
                ("POST", "/v1/instances", "not json"),
                ("POST", "/v1/instances/18181/stop", ""),
                ("DELETE", "/v1/instances/18181", ""),
            ] {
                let reply = exchange(&mut registry, method, path, headers, body);
                assert_eq!(reply.status, 403, "{case} {method} {path} {body}");
                assert_eq!(
                    exchange(&mut registry, "GET", "/v1/instances", "", "").body,
                    before,
                    "{case} {method} {path} {body}"
                );
                assert_eq!(
                    exchange(&mut registry, "POST", "/v1/instances", allowed, INSTANCE).status,
                    204,
                    "{case} {method} {path} {body}"
                );
            }
        }
    }

    /// Oversized write bodies fail before the header guard is checked.
    #[test]
    fn test_write_size_limit_precedes_the_browser_guard() {
        let mut registry = Registry::new();
        let reply = exchange(
            &mut registry,
            "POST",
            "/v1/instances",
            "",
            &"x".repeat(8193),
        );
        assert_eq!(reply.status, 413);
        let listing = exchange(&mut registry, "GET", "/v1/instances", "", "");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&listing.body).unwrap(),
            serde_json::json!({"version": 1, "instances": []})
        );
    }

    /// Guarded native writes still publish, refresh, deliver stops and withdraw.
    #[test]
    fn test_guarded_writes_keep_the_registry_lifecycle() {
        // Header names are case-insensitive on every mutating route
        let mut registry = Registry::new();
        let allowed = "x-aRk-ReGiStRy: 1\r\n";
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", allowed, INSTANCE).status,
            204
        );
        let updated = r#"{"port":18181,"disk":"renamed.ark","disk_id":"0123abcd","ready":false}"#;
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", allowed, updated).status,
            204
        );
        let listing = exchange(&mut registry, "GET", "/v1/instances", "", "");
        let body: serde_json::Value = serde_json::from_str(&listing.body).unwrap();
        assert_eq!(body["instances"][0]["disk"], "renamed.ark");
        assert_eq!(body["instances"][0]["ready"], false);

        // A stop is queued only for a known entry and reaches its next heartbeat
        assert_eq!(
            exchange(
                &mut registry,
                "POST",
                "/v1/instances/18182/stop",
                allowed,
                ""
            )
            .status,
            404
        );
        assert_eq!(
            exchange(
                &mut registry,
                "POST",
                "/v1/instances/18181/stop",
                allowed,
                ""
            )
            .status,
            204
        );
        let beat = exchange(&mut registry, "POST", "/v1/instances", allowed, updated);
        assert_eq!(beat.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&beat.body).unwrap(),
            serde_json::json!({"stop": true})
        );

        // Withdrawal remains idempotent and removes the pending stop with the entry
        for _ in 0..2 {
            assert_eq!(
                exchange(&mut registry, "DELETE", "/v1/instances/18181", allowed, "").status,
                204
            );
        }
        let listing = exchange(&mut registry, "GET", "/v1/instances", "", "");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&listing.body).unwrap(),
            serde_json::json!({"version": 1, "instances": []})
        );
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", allowed, INSTANCE).status,
            204
        );
    }

    /// Browser reads stay open while preflight never grants the write header.
    #[test]
    fn test_browser_reads_and_preflights_never_authorize_writes() {
        // Read a populated registry with an ordinary browser origin
        let mut registry = Registry::new();
        assert_eq!(
            exchange(
                &mut registry,
                "POST",
                "/v1/instances",
                "X-Ark-Registry: 1\r\n",
                INSTANCE
            )
            .status,
            204
        );
        let listing = exchange(
            &mut registry,
            "GET",
            "/v1/instances",
            "Origin: https://example.com\r\n",
            "",
        );
        assert_eq!(listing.status, 200);
        assert_eq!(listing.headers["access-control-allow-origin"], "*");
        let body: serde_json::Value = serde_json::from_str(&listing.body).unwrap();
        assert_eq!(body["version"], 1);
        assert_eq!(body["instances"][0]["port"], 18181);

        // Requested methods and headers never get reflected into the grant
        for (method, path, requested_headers) in [
            ("GET", "/v1/instances", ""),
            ("POST", "/v1/instances", "content-type, x-ark-registry"),
            ("POST", "/v1/instances/18181/stop", "x-ark-registry"),
            ("DELETE", "/v1/instances/18181", "X-Ark-Registry"),
            ("POST", "/unknown", "x-ark-registry"),
        ] {
            let headers = format!(
                "Origin: https://example.com\r\nAccess-Control-Request-Method: {method}\r\nAccess-Control-Request-Headers: {requested_headers}\r\nAccess-Control-Request-Private-Network: true\r\n"
            );
            let reply = exchange(&mut registry, "OPTIONS", path, &headers, "");
            assert_eq!(reply.status, 204, "{method} {path}");
            assert_eq!(
                reply.headers["access-control-allow-origin"], "*",
                "{method} {path}"
            );
            assert_eq!(
                reply.headers["access-control-allow-methods"], "GET, OPTIONS",
                "{method} {path}"
            );
            assert_eq!(
                reply.headers["access-control-allow-headers"], "Content-Type",
                "{method} {path}"
            );
            assert_eq!(
                reply.headers["access-control-allow-private-network"], "true",
                "{method} {path}"
            );
        }
        assert_eq!(
            exchange(&mut registry, "GET", "/v1/instances", "", "").body,
            listing.body
        );
        assert_eq!(
            exchange(
                &mut registry,
                "POST",
                "/v1/instances",
                "X-Ark-Registry: 1\r\n",
                INSTANCE
            )
            .status,
            204
        );
    }

    fn instance(port: u16) -> Instance {
        Instance {
            port,
            control: None,
            disk: "emulator.ark".into(),
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

        registry.upsert(instance(18181));
        registry.expire(Instant::now());
        assert_eq!(registry.listing().instances.len(), 1);
    }

    #[test]
    fn test_a_stop_reaches_the_target_on_its_next_beat() {
        let mut registry = Registry::new();
        registry.upsert(instance(18181));

        assert!(registry.request_stop(18181));
        assert!(registry.upsert(instance(18181)).stop);
    }

    #[test]
    fn test_a_beat_carries_nothing_until_a_stop_is_asked_for() {
        let mut registry = Registry::new();
        assert!(!registry.upsert(instance(18181)).stop);
        assert!(!registry.upsert(instance(18181)).stop);
    }

    #[test]
    fn test_a_stop_for_an_unlisted_port_is_refused() {
        let mut registry = Registry::new();
        registry.upsert(instance(18181));
        assert!(!registry.request_stop(18182));
    }

    #[test]
    fn test_a_stop_survives_until_the_target_reads_it() {
        // Several beats can pass before the launcher acts on one, and each of
        // them replaces the record.
        let mut registry = Registry::new();
        registry.upsert(instance(18181));
        registry.request_stop(18181);

        for _ in 0..3 {
            assert!(registry.upsert(instance(18181)).stop);
        }
    }

    #[test]
    fn test_only_a_stop_route_names_a_port() {
        assert_eq!(stop_route("/v1/instances/18181/stop"), Some(Ok(18181)));
        assert_eq!(stop_route("/v1/instances/nope/stop"), Some(Err(())));
        for path in [
            "/v1/instances",
            "/v1/instances/18181",
            "/stop",
            "/v1/x/1/stop",
        ] {
            assert_eq!(stop_route(path), None, "{path}");
        }
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
    fn test_hosting_twice_from_one_process_is_refused() {
        // The second call is the one a heartbeat makes after a failed publish.
        // A launcher already hosting must not stack a second server on its own.
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
