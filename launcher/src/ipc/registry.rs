// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Running emulators, published through native IPC and discoverable over HTTP.
//!
//! The launcher holding the browser port also owns the private registry name.
//! Both listeners share one registry; only the native listener can mutate it.
//!
//! ```text
//!   launchers ------ native IPC ------> [registry]
//!   native CLI ----- native IPC ------> [        ]
//!   browsers ------- HTTP GET --------> [        ]
//! ```
//!
//! On host exit, the next publisher claims the port and native name. Every
//! launcher republishes once a second, and entries expire after 15 s without
//! a heartbeat. See [`super::discovery`] for takeover and failure handling.
//!
//! Browser discovery allows every origin and publishes basenames, never paths.
//! Lifecycle commands use [`super::control`] without consulting the registry
//! again after selecting targets.

use std::collections::HashMap;
use std::io::{self, Cursor};
use std::net::{SocketAddrV4, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Response, Server, StatusCode};

use super::local::{self, Request};

use crate::diagnostics::log;

/// Port the registry is served on. One below the first port an emulator takes,
/// so the whole emulator range reads as one contiguous block.
pub(crate) const REGISTRY_PORT: u16 = 18180;

/// Schema version of registry responses.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// How long an entry survives without being refreshed. Comfortably more than
/// the heartbeat interval in [`super::discovery`], so a launcher that is busy
/// or beating slowly is not dropped between two of its beats.
const ENTRY_TTL: Duration = Duration::from_secs(15);

/// How often the serve loop wakes up with no request to handle, which is what
/// bounds how late an entry's expiry can be.
const TICK: Duration = Duration::from_millis(500);

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

    /// Add or refresh an entry.
    ///
    /// A re-registration replaces the whole record rather than merging into it,
    /// so a cleared claim (a device renamed to nothing, say) does not linger.
    fn upsert(&mut self, instance: Instance) {
        self.entries.insert(
            instance.port,
            Entry {
                instance,
                seen: Instant::now(),
            },
        );
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

/// Serve the registry if nobody else holds the port, reporting other failures.
/// Returns whether this process became the host.
///
/// Loopback only. The registry describes what is running on this machine and
/// has no business being reachable from off it.
pub(crate) fn host(addr: SocketAddrV4) -> io::Result<bool> {
    let listener = match TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Ok(false),
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("could not bind {addr}: {e}"),
            ));
        }
    };

    // tiny_http takes an already-bound listener, so the bind above is what
    // decides the race.
    let server = Server::from_listener(listener, None::<tiny_http::SslConfig>)
        .map_err(|e| io::Error::other(format!("could not serve on {addr}: {e}")))?;
    #[cfg(not(test))]
    let native = local::Server::bind(&local_name(addr.port()))?;
    #[cfg(test)]
    let native = local::Server::bind_test(&local_name(addr.port()))?;
    let registry = Arc::new(Mutex::new(Registry::new()));
    let public_registry = registry.clone();
    log!("[registry] hosting the registry on {addr}");

    // Runs for the life of the process. This launcher exiting is what hands
    // the port to the next one.
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let listing = {
                let mut registry = public_registry.lock().unwrap();
                registry.expire(Instant::now());
                registry.listing()
            };
            handle_public(request, &listing, addr);
        }
    });
    thread::spawn(move || serve(&native, &registry));
    Ok(true)
}

/// Name the private publication endpoint paired with a browser discovery port.
pub(super) fn local_name(port: u16) -> String {
    format!("registry-{port}")
}

/// Serve only browser discovery; no request can reach a native mutation route.
fn handle_public(request: tiny_http::Request, listing: &Listing, addr: SocketAddrV4) {
    let hosts: Vec<_> = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("Host"))
        .map(|header| header.value.as_str())
        .collect();
    let expected = addr.to_string();
    let localhost = format!("localhost:{}", addr.port());
    let mut response = if hosts.len() != 1 || (hosts[0] != expected && hosts[0] != localhost) {
        text(StatusCode(403), "unexpected discovery host")
    } else {
        match (
            request.method(),
            request.url().split('?').next().unwrap_or(""),
        ) {
            (Method::Options, "/v1/instances") => empty(StatusCode(204)),
            (Method::Get, "/v1/instances") => json(serde_json::to_vec(listing).unwrap()),
            _ => text(StatusCode(404), "no such route"),
        }
    };
    for header in cors() {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

/// Serve native requests and expire entries under the shared registry lock.
fn serve(server: &local::Server, registry: &Mutex<Registry>) {
    loop {
        match server.recv_timeout(TICK) {
            Ok(Some(request)) => handle(request, &mut registry.lock().unwrap()),
            Ok(None) => {}
            // A failed accept says nothing about the other clients, so keep
            // serving.
            Err(e) => log!("[registry] could not accept a request: {e}"),
        }
        registry.lock().unwrap().expire(Instant::now());
    }
}

/// Route a native request without exposing mutations on the browser listener.
fn handle(request: Request, registry: &mut Registry) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();

    let response = match (&method, path.as_str()) {
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

        (Method::Post, "/v1/instances") => match serde_json::from_slice::<Instance>(request.body())
        {
            Ok(instance) => {
                registry.upsert(instance);
                empty(StatusCode(204))
            }
            Err(e) => text(StatusCode(400), &format!("malformed body: {e}")),
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

    let _ = request.respond(response);
}

/// Cross-origin grants carried only by browser discovery responses.
///
/// The allowed origin is `*` because an allowlist would mean baking somebody's
/// hostnames in here. What keeps that acceptable is the shape of an
/// [`Instance`]: ports and file names, never paths.
/// This listener has no write routes, regardless of the request's headers.
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

/// A plain-text response, for the cases a consumer can only log.
fn text(status: StatusCode, message: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(message.as_bytes().to_vec()).with_status_code(status)
}

/// A response with no body, for the routes whose answer is their status code.
fn empty(status: StatusCode) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(Vec::new()).with_status_code(status)
}

/// Registry lifecycle and HTTP browser guards.
#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, TcpStream};

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

    /// Exercise the actual TCP route table, including Host validation.
    fn public_exchange(
        registry: &Registry,
        method: &str,
        path: &str,
        headers: &str,
        host: Option<&str>,
    ) -> Reply {
        let server = Server::http((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.0\r\nHost: {}\r\nContent-Length: 0\r\n{headers}\r\n",
            host.unwrap_or(&address.to_string())
        )
        .unwrap();
        let request = server.recv().unwrap();
        handle_public(
            request,
            &registry.listing(),
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, address.port()),
        );
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        parse_reply(&response)
    }

    /// Decode the status, headers and complete body returned by a test peer.
    fn parse_reply(response: &str) -> Reply {
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

    /// Send a raw HTTP request through an isolated registry listener.
    fn exchange(
        registry: &mut Registry,
        method: &str,
        path: &str,
        headers: &str,
        body: &str,
    ) -> Reply {
        // Use a unique local name so these tests cannot touch a running emulator
        let server = local::Server::bind(&format!("t-{}", local::identity())).unwrap();
        let address = "localhost";
        let timeout = Duration::from_secs(2);
        let mut stream = local::Stream::connect(server.name(), timeout).unwrap();
        stream.set_read_timeout(timeout);
        stream.set_write_timeout(timeout);
        write!(
            stream,
            "{method} {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
        .unwrap();

        // Exercise routing and header parsing before inspecting the reply
        match server.recv_timeout(timeout) {
            Ok(Some(request)) => handle(request, registry),
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {}
            result => panic!("request was not received: {}", result.err().unwrap()),
        }
        let mut response = String::new();
        if let Err(err) = stream.read_to_string(&mut response) {
            assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
            assert!(response.contains("\r\n\r\n"));
        }
        parse_reply(&response)
    }

    /// Invalid JSON cannot replace a published entry.
    #[test]
    fn test_invalid_writes_preserve_entries() {
        let mut registry = Registry::new();
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE).status,
            204
        );
        let before = exchange(&mut registry, "GET", "/v1/instances", "", "").body;
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", "", "not json").status,
            400
        );
        assert_eq!(
            exchange(&mut registry, "GET", "/v1/instances", "", "").body,
            before
        );
    }

    /// The transport rejects oversized bodies before they can update the registry.
    #[test]
    fn test_oversized_writes_leave_the_registry_unchanged() {
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

    /// Native writes publish, refresh and withdraw entries without browser guards.
    #[test]
    fn test_native_writes_keep_the_registry_lifecycle() {
        let mut registry = Registry::new();
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE).status,
            204
        );
        let updated = r#"{"port":18181,"disk":"renamed.ark","disk_id":"0123abcd","ready":false}"#;
        assert_eq!(
            exchange(
                &mut registry,
                "POST",
                "/v1/instances",
                "Origin: https://example.com\r\n",
                updated
            )
            .status,
            204
        );
        let listing = exchange(&mut registry, "GET", "/v1/instances", "", "");
        let body: serde_json::Value = serde_json::from_str(&listing.body).unwrap();
        assert_eq!(body["instances"][0]["disk"], "renamed.ark");
        assert_eq!(body["instances"][0]["ready"], false);

        // Withdrawal remains idempotent, and the same launcher can publish again
        for _ in 0..2 {
            assert_eq!(
                exchange(&mut registry, "DELETE", "/v1/instances/18181", "", "").status,
                204
            );
        }
        let listing = exchange(&mut registry, "GET", "/v1/instances", "", "");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&listing.body).unwrap(),
            serde_json::json!({"version": 1, "instances": []})
        );
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE).status,
            204
        );
        let preflight = exchange(&mut registry, "OPTIONS", "/v1/instances", "", "");
        assert_eq!(preflight.status, 404);
        assert!(
            !preflight
                .headers
                .contains_key("access-control-allow-origin")
        );
    }

    /// Registry stop requests fail without affecting entries or later heartbeats.
    #[test]
    fn test_registry_rejects_stop_requests() {
        let mut registry = Registry::new();
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE).status,
            204
        );
        let before = exchange(&mut registry, "GET", "/v1/instances", "", "").body;

        // Registered ports, absent ports and the direct route all stay unsupported here
        for path in [
            "/v1/instances/18181/stop",
            "/v1/instances/18182/stop",
            "/v1/instances/nope/stop",
            "/v1/stop",
        ] {
            assert_eq!(
                exchange(&mut registry, "POST", path, "", "").status,
                404,
                "{path}"
            );
            assert_eq!(
                exchange(&mut registry, "GET", "/v1/instances", "", "").body,
                before,
                "{path}"
            );
            let heartbeat = exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE);
            assert_eq!(heartbeat.status, 204, "{path}");
            assert!(heartbeat.body.is_empty(), "{path}");
        }
    }

    /// Browser discovery grants reads while mutation routes remain absent.
    #[test]
    fn test_browser_reads_and_preflights_never_authorize_writes() {
        // Read a populated registry with an ordinary browser origin
        let mut registry = Registry::new();
        assert_eq!(
            exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE).status,
            204
        );
        let listing = public_exchange(
            &registry,
            "GET",
            "/v1/instances",
            "Origin: https://example.com\r\n",
            None,
        );
        assert_eq!(listing.status, 200);
        assert_eq!(listing.headers["access-control-allow-origin"], "*");
        let body: serde_json::Value = serde_json::from_str(&listing.body).unwrap();
        assert_eq!(body["version"], 1);
        assert_eq!(body["instances"][0]["port"], 18181);

        // Requested methods and headers never get reflected into the grant
        for (method, path, requested_headers) in [
            ("GET", "/v1/instances", ""),
            ("POST", "/v1/instances", "content-type, x-custom"),
            ("DELETE", "/v1/instances/18181", "x-custom"),
            ("POST", "/unknown", "x-custom"),
        ] {
            let headers = format!(
                "Origin: https://example.com\r\nAccess-Control-Request-Method: {method}\r\nAccess-Control-Request-Headers: {requested_headers}\r\nAccess-Control-Request-Private-Network: true\r\n"
            );
            let reply = public_exchange(&registry, "OPTIONS", path, &headers, None);
            assert_eq!(
                reply.status,
                if path == "/v1/instances" { 204 } else { 404 },
                "{method} {path}"
            );
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
            exchange(&mut registry, "POST", "/v1/instances", "", INSTANCE).status,
            204
        );
    }

    /// TCP accepts discovery only and validates its Host header.
    #[test]
    fn test_browser_listener_has_no_mutation_routes() {
        let mut registry = Registry::new();
        registry.upsert(instance(18181));
        for (method, path) in [
            ("POST", "/v1/instances"),
            ("DELETE", "/v1/instances/18181"),
            ("POST", "/v1/stop"),
            ("GET", "/v1/status"),
            ("GET", "/v1/button"),
            ("POST", "/v1/button/press"),
        ] {
            let response = public_exchange(
                &registry,
                method,
                path,
                "Origin: https://example.com\r\n",
                None,
            );
            assert_eq!(response.status, 404, "{method} {path}");
        }
        assert_eq!(registry.listing().instances.len(), 1);
        assert_eq!(
            public_exchange(
                &registry,
                "GET",
                "/v1/instances",
                "",
                Some("attacker.example:18180")
            )
            .status,
            403
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
        if !host(super::super::discovery::CLIENT.address).unwrap() {
            // The port is held by something else, which the test below covers.
            return;
        }
        assert!(!host(super::super::discovery::CLIENT.address).unwrap());
    }

    #[test]
    fn test_only_one_launcher_can_host_the_registry() {
        // Whoever binds first serves; the rest report a lost race rather than
        // an error, which is what lets every launcher try unconditionally.
        let Ok(held) = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, REGISTRY_PORT))
        else {
            // Something on this machine is already holding the port, which is
            // the case being asserted anyway.
            assert!(!host(super::super::discovery::CLIENT.address).unwrap());
            return;
        };
        assert!(!host(super::super::discovery::CLIENT.address).unwrap());
        drop(held);
    }
}
