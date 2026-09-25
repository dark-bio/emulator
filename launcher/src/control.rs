// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Direct, synchronous button control for one running launcher.
//!
//! Discovery advertises a loopback port and a launch identifier. Commands
//! read the current connection generation before sending an input, and wait
//! for its hardware write. Neither launcher replacement nor guest reconnection
//! replays a pending input. The identifier distinguishes launches, not users;
//! a required custom header and refused browser origins keep web pages out.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::error::{Code, Error};
use crate::hardware::{ButtonOutcome, ButtonSource, Controller};

/// A button reply is a small JSON object, even when it carries an error.
const MAX_RESPONSE: u64 = 4096;
/// Header required on every request, including reads.
const INSTANCE_HEADER: &str = "X-Ark-Emulator";
/// Connection generation observed before a button input was requested.
const GENERATION_HEADER: &str = "X-Ark-Generation";

/// The direct endpoint published in a registry entry.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Endpoint {
    /// Loopback HTTP port, distinct from the guest's forwarded port.
    pub(crate) port: u16,
    /// Opaque identifier preventing a stale entry controlling a later launch.
    pub(crate) id: String,
}

/// Connection and button state returned before an input is submitted.
#[derive(Deserialize, Serialize)]
struct Snapshot {
    /// Decimal string preserving the full connection counter.
    generation: String,
    /// Whether the guest currently accepts hardware inputs.
    connected: bool,
    /// Physical button state, including window input.
    pressed: bool,
    /// Whether the command line holds the button.
    cli_pressed: bool,
}

/// One listener and worker, stopped with the guest that owns them.
pub(crate) struct Control {
    /// Published location and identity of this listener.
    pub(crate) endpoint: Endpoint,
    /// Wakes the worker on shutdown.
    server: Arc<Server>,
    /// Prevents another request being accepted after shutdown.
    stopping: Arc<AtomicBool>,
    /// Joined after any in-flight hardware operation has ended.
    worker: Option<JoinHandle<()>>,
}

impl Control {
    /// Bind a private loopback listener before the guest is started.
    pub(crate) fn start(hardware: Controller) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("could not bind the emulator control port")?;
        let port = listener.local_addr()?.port();
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let id = format!(
            "{:x}",
            Sha256::digest(format!("{}:{stamp}:{port}", std::process::id()))
        );
        let endpoint = Endpoint { port, id };
        let server = Arc::new(
            Server::from_listener(listener, None)
                .map_err(|err| anyhow::anyhow!("could not serve emulator control: {err}"))?,
        );
        let stopping = Arc::new(AtomicBool::new(false));
        let worker = {
            let server = server.clone();
            let stopping = stopping.clone();
            let endpoint = endpoint.clone();
            thread::Builder::new()
                .name("control".to_owned())
                .spawn(move || {
                    while !stopping.load(Ordering::SeqCst) {
                        match server.recv() {
                            Ok(request) if !stopping.load(Ordering::SeqCst) => {
                                handle(request, &endpoint, &hardware);
                            }
                            _ => break,
                        }
                    }
                })?
        };
        Ok(Self {
            endpoint,
            server,
            stopping,
            worker: Some(worker),
        })
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.server.unblock();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Answer one request without accepting bodies or cross-origin browser access.
fn handle(request: Request, endpoint: &Endpoint, hardware: &Controller) {
    // Read headers without consuming a request body
    let header = |name: &str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    };
    let (status, body) = if request.body_length().is_some_and(|length| length != 0)
        || header("Transfer-Encoding").is_some()
    {
        (
            413,
            serde_json::json!({"error": "control requests have no body"}),
        )
    } else if header("Origin").is_some() {
        (
            403,
            serde_json::json!({"error": "browser requests are not accepted"}),
        )
    } else if header(INSTANCE_HEADER) != Some(endpoint.id.as_str()) {
        (
            412,
            serde_json::json!({"error": "the launcher no longer matches discovery"}),
        )
    } else {
        // Bind every input to the connection observed by its caller
        let apply = |pressed, release_after| match header(GENERATION_HEADER)
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(generation) => {
                match hardware.button(ButtonSource::Cli, pressed, generation, release_after) {
                    Ok(outcome) => (200, serde_json::to_value(outcome).unwrap()),
                    Err(err) => (409, serde_json::json!({"error": err})),
                }
            }
            None => (
                400,
                serde_json::json!({"error": "a connection generation is required"}),
            ),
        };
        match (request.method(), request.url()) {
            (&Method::Get, "/v1/button") => {
                let state = hardware.snapshot();
                (
                    200,
                    serde_json::to_value(Snapshot {
                        generation: state.generation.to_string(),
                        connected: state.connected,
                        pressed: state.pressed,
                        cli_pressed: state.cli_pressed,
                    })
                    .unwrap(),
                )
            }
            (&Method::Post, path @ ("/v1/button/press" | "/v1/button/release")) => {
                apply(path.ends_with("/press"), None)
            }
            (&Method::Post, path) if path.starts_with("/v1/button/press/") => {
                // A separate route makes older launchers reject timed presses entirely
                match path
                    .strip_prefix("/v1/button/press/")
                    .unwrap()
                    .parse::<u32>()
                {
                    Ok(seconds) => apply(true, Some(seconds)),
                    Err(_) => (
                        400,
                        serde_json::json!({"error": "release delay must be whole seconds from 0 to 4294967295"}),
                    ),
                }
            }
            _ => (404, serde_json::json!({"error": "no such control route"})),
        }
    };

    // Do not grant cross-origin access to this control endpoint
    let response = Response::from_data(serde_json::to_vec(&body).unwrap())
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
    let _ = request.respond(response);
}

/// Apply a CLI hold to the connection observed immediately before the request.
pub(crate) fn button(
    endpoint: &Endpoint,
    pressed: bool,
    release_after: Option<u32>,
    timeout: Duration,
) -> Result<ButtonOutcome, Error> {
    // Reject an inconsistent input before contacting the launcher
    if !pressed && release_after.is_some() {
        return Err(Error::new(
            Code::Usage,
            "automatic release requires a button press",
        ));
    }

    // Read the target generation before submitting an input
    let snapshot: Snapshot =
        serde_json::from_slice(&request(endpoint, "GET", "/v1/button", None, timeout)?).map_err(
            |err| {
                Error::new(
                    Code::ControlUnreachable,
                    format!("could not read button state: {err}"),
                )
            },
        )?;
    if !snapshot.connected {
        return Err(Error::new(
            Code::ButtonUnavailable,
            "the emulator's hardware is not connected",
        )
        .hint("wait for the guest to boot, then try again"));
    }
    // Validate before copying a discovered value into an HTTP header
    let generation = snapshot.generation.parse::<u64>().map_err(|_| {
        Error::new(
            Code::ControlUnreachable,
            "invalid hardware connection generation",
        )
    })?;

    // Send timed presses on a route that older launchers cannot silently accept
    let path = if let Some(seconds) = release_after {
        format!("/v1/button/press/{seconds}")
    } else if pressed {
        "/v1/button/press".to_owned()
    } else {
        "/v1/button/release".to_owned()
    };
    let outcome: ButtonOutcome = serde_json::from_slice(&request(
        endpoint,
        "POST",
        &path,
        Some(generation),
        timeout,
    )?)
    .map_err(|err| {
        Error::new(
            Code::ControlUnreachable,
            format!("could not read button delivery: {err}"),
        )
        .hint("the button state is unknown; use `ark-emulator button release` to clear a CLI hold")
    })?;

    // A successful timed command must acknowledge the requested schedule
    if outcome.release_after_seconds != release_after {
        return Err(Error::new(
            Code::ControlUnreachable,
            "the launcher did not confirm the requested release schedule",
        )
        .hint(
            "the button state is unknown; use `ark-emulator button release` to clear a CLI hold",
        ));
    }
    Ok(outcome)
}

/// Exchange one bounded HTTP/1.0 request, without retrying an uncertain input.
fn request(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
    generation: Option<u64>,
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    if endpoint.port == 0
        || endpoint.id.len() != 64
        || !endpoint.id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::new(
            Code::ControlUnreachable,
            "invalid emulator control endpoint",
        )
        .hint("restart the emulator using the current build"));
    }
    let exchange = || -> std::io::Result<Vec<u8>> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
        let mut stream = TcpStream::connect_timeout(&address, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut head = format!(
            "{method} {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\nContent-Length: 0\r\n{INSTANCE_HEADER}: {}\r\n",
            endpoint.id
        );
        if let Some(generation) = generation {
            head.push_str(&format!("{GENERATION_HEADER}: {generation}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        let mut response = Vec::new();
        stream.take(MAX_RESPONSE + 1).read_to_end(&mut response)?;
        Ok(response)
    };
    let raw = exchange().map_err(|err| {
        let code = if matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            Code::Timeout
        } else {
            Code::ControlUnreachable
        };
        Error::new(code, format!("emulator control did not answer: {err}")).hint(
            "the button state is unknown; use `ark-emulator button release` to clear a CLI hold",
        )
    })?;
    if raw.len() as u64 > MAX_RESPONSE {
        return Err(Error::new(
            Code::ControlUnreachable,
            "emulator control returned too much data",
        ));
    }
    let split = raw
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or_else(|| {
            Error::new(
                Code::ControlUnreachable,
                "emulator control returned no HTTP headers",
            )
        })?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok());
    let body = &raw[split + 4..];
    if status == Some(200) {
        return Ok(body.to_vec());
    }
    let error = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "emulator control returned an invalid response".to_owned());
    if status == Some(404) {
        return Err(Error::new(
            Code::ControlUnsupported,
            "the launcher does not support this button request",
        )
        .hint("update Ark Emulator and restart the selected emulator"));
    }
    let code = if status == Some(409) {
        Code::ButtonUnavailable
    } else {
        Code::ControlUnreachable
    };
    Err(Error::new(code, error)
        .hint("check `ark-emulator list` and retry against the current emulator"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::net::Shutdown;
    use std::sync::mpsc;
    use std::time::Instant;
    use tungstenite::WebSocket;

    /// A real controller, control listener and loopback guest for protocol tests.
    struct Fixture {
        /// The launcher side of the hardware socket.
        hardware: Controller,
        /// Listener kept for simulated guest restarts.
        listener: TcpListener,
        /// Direct HTTP service exercised by the production CLI client.
        control: Control,
        /// Guest side, also used to verify exact wire edges.
        peer: WebSocket<TcpStream>,
    }

    impl Fixture {
        /// Connect without QEMU or any frontend.
        fn new() -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let hardware = Controller::default();
            hardware.start(listener.local_addr().unwrap());
            let peer = accept(&listener);
            wait_for(|| hardware.snapshot().connected);
            let control = Control::start(hardware.clone()).unwrap();
            Self {
                hardware,
                listener,
                control,
                peer,
            }
        }

        /// Read one expected edge within the hardware response deadline.
        fn edge(&mut self) -> String {
            let frame = self.peer.read().unwrap();
            let body: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(body["d"], "button");
            assert_eq!(body["id"], "5");
            body["payload"]["edge"].as_str().unwrap().to_owned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.hardware.stop();
        }
    }

    /// Accept a hardware connection under a deadline, including the handshake.
    fn accept(listener: &TcpListener) -> WebSocket<TcpStream> {
        let listener = listener.try_clone().unwrap();
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let _ = send.send(tungstenite::accept(stream).unwrap());
        });
        receive.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    /// Bound asynchronous worker observations in synchronous tests.
    fn wait_for(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "hardware did not reach the expected state"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// CLI holds survive UI cleanup, and release preserves a separate UI hold.
    #[test]
    fn test_button_delivery_deduplication_and_independent_holds() {
        let mut fixture = Fixture::new();
        let timeout = Duration::from_secs(1);
        let generation = fixture.hardware.snapshot().generation;
        let result = button(&fixture.control.endpoint, true, None, timeout).unwrap();
        assert!(result.pressed && result.cli_pressed && result.changed);
        assert_eq!(fixture.edge(), "falling");
        let result = button(&fixture.control.endpoint, true, None, timeout).unwrap();
        assert!(!result.changed);
        fixture.hardware.release_button();
        fixture
            .hardware
            .button(ButtonSource::Ui, true, generation, None)
            .unwrap();
        let result = button(&fixture.control.endpoint, false, None, timeout).unwrap();
        assert!(result.pressed && !result.cli_pressed && result.changed);
        fixture
            .hardware
            .button(ButtonSource::Ui, false, generation, None)
            .unwrap();
        assert_eq!(fixture.edge(), "rising");
        let result = button(&fixture.control.endpoint, false, None, timeout).unwrap();
        assert!(!result.pressed && !result.cli_pressed && !result.changed);
        button(&fixture.control.endpoint, true, None, timeout).unwrap();
        assert_eq!(fixture.edge(), "falling");
        fixture.hardware.release_button();
        let result = button(&fixture.control.endpoint, false, None, timeout).unwrap();
        assert!(!result.pressed);
        assert_eq!(fixture.edge(), "rising");
    }

    /// Zero seconds delivers ordered press and release edges before replying.
    #[test]
    fn test_zero_delay_releases_before_replying() {
        let mut fixture = Fixture::new();
        for _ in 0..2 {
            // Each request completes the release before acknowledging its state
            let outcome = button(
                &fixture.control.endpoint,
                true,
                Some(0),
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(outcome.release_after_seconds, Some(0));
            assert!(outcome.changed);
            assert!(!outcome.pressed && !outcome.cli_pressed);
            assert!(!fixture.hardware.snapshot().pressed);

            // Repeated immediate presses each produce both edges in order
            assert_eq!(fixture.edge(), "falling");
            assert_eq!(fixture.edge(), "rising");
        }
    }

    /// Zero seconds cancels a previous timer while preserving the window hold.
    #[test]
    fn test_zero_delay_cancels_the_timer_and_preserves_the_window_hold() {
        // Hold the button from both sources with a release timer pending
        let mut fixture = Fixture::new();
        let timeout = Duration::from_secs(1);
        button(&fixture.control.endpoint, true, Some(1), timeout).unwrap();
        assert_eq!(fixture.edge(), "falling");
        let generation = fixture.hardware.snapshot().generation;
        fixture
            .hardware
            .button(ButtonSource::Ui, true, generation, None)
            .unwrap();

        // Immediate release clears the CLI hold without a physical release edge
        let outcome = button(&fixture.control.endpoint, true, Some(0), timeout).unwrap();
        assert!(outcome.pressed && outcome.changed);
        assert!(!outcome.cli_pressed);
        fixture
            .hardware
            .button(ButtonSource::Ui, false, generation, None)
            .unwrap();
        assert_eq!(fixture.edge(), "rising");

        // A subsequent hold survives the timer that the immediate release cancelled
        button(&fixture.control.endpoint, true, None, timeout).unwrap();
        assert_eq!(fixture.edge(), "falling");
        thread::sleep(Duration::from_millis(1200));
        assert!(fixture.hardware.snapshot().cli_pressed);
        button(&fixture.control.endpoint, false, None, timeout).unwrap();
        assert_eq!(fixture.edge(), "rising");
    }

    /// The launcher releases a timed hold after the requesting client has left.
    #[test]
    fn test_timed_release_runs_after_the_request_finishes() {
        // Deliver one timed press and observe its edge before the timer expires
        let mut fixture = Fixture::new();
        let started = Instant::now();
        let outcome = button(
            &fixture.control.endpoint,
            true,
            Some(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(outcome.release_after_seconds, Some(1));
        assert!(outcome.pressed && outcome.cli_pressed && outcome.changed);
        assert_eq!(fixture.edge(), "falling");
        assert!(fixture.hardware.snapshot().cli_pressed);

        // No client remains connected while the worker delivers the release
        fixture
            .peer
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(fixture.edge(), "rising");
        assert!(started.elapsed() >= Duration::from_secs(1));
        wait_for(|| !fixture.hardware.snapshot().cli_pressed);
        assert!(!fixture.hardware.snapshot().pressed);
    }

    /// Repeating a timed press resets its deadline without adding another edge.
    #[test]
    fn test_a_new_timed_press_replaces_the_deadline() {
        // Leave enough time to replace the first deadline before it can expire
        let mut fixture = Fixture::new();
        button(
            &fixture.control.endpoint,
            true,
            Some(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(fixture.edge(), "falling");
        thread::sleep(Duration::from_millis(100));

        // The next edge must belong to the replacement timer, not the first one
        let started = Instant::now();
        let outcome = button(
            &fixture.control.endpoint,
            true,
            Some(2),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(outcome.changed);
        fixture
            .peer
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(fixture.edge(), "rising");
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    /// Manual release and a new untimed press both remove the previous timer.
    #[test]
    fn test_cancelled_timers_cannot_release_a_later_hold() {
        for release_first in [false, true] {
            // Set up a timer and optionally release its hold explicitly
            let mut fixture = Fixture::new();
            button(
                &fixture.control.endpoint,
                true,
                Some(1),
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(fixture.edge(), "falling");
            if release_first {
                button(
                    &fixture.control.endpoint,
                    false,
                    None,
                    Duration::from_secs(1),
                )
                .unwrap();
                assert_eq!(fixture.edge(), "rising");
            }

            // An untimed press survives the previous deadline in either case
            let outcome = button(
                &fixture.control.endpoint,
                true,
                None,
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(outcome.changed);
            assert!(outcome.release_after_seconds.is_none());
            if release_first {
                assert_eq!(fixture.edge(), "falling");
            }
            thread::sleep(Duration::from_millis(1200));
            assert!(fixture.hardware.snapshot().cli_pressed);
            button(
                &fixture.control.endpoint,
                false,
                None,
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(fixture.edge(), "rising");
        }
    }

    /// Automatic release clears only the CLI hold while a pointer remains down.
    #[test]
    fn test_timed_release_preserves_the_window_hold() {
        // Establish overlapping holds, then wait for only the CLI hold to expire
        let mut fixture = Fixture::new();
        button(
            &fixture.control.endpoint,
            true,
            Some(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(fixture.edge(), "falling");
        let generation = fixture.hardware.snapshot().generation;
        fixture
            .hardware
            .button(ButtonSource::Ui, true, generation, None)
            .unwrap();
        wait_for(|| !fixture.hardware.snapshot().cli_pressed);
        assert!(fixture.hardware.snapshot().pressed);

        // Releasing the remaining holder produces the sole rising edge
        fixture
            .hardware
            .button(ButtonSource::Ui, false, generation, None)
            .unwrap();
        assert_eq!(fixture.edge(), "rising");
    }

    /// Invalid durations cannot turn an untimed hold into a timer or release it.
    #[test]
    fn test_invalid_timed_requests_leave_the_hold_unchanged() {
        // Keep a known untimed hold while submitting malformed timed routes
        let mut fixture = Fixture::new();
        button(
            &fixture.control.endpoint,
            true,
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(fixture.edge(), "falling");
        let generation = fixture.hardware.snapshot().generation;
        for delay in ["-1", "1.5", "NaN", "4294967296", "", "/v1/button/press/1"] {
            let error = request(
                &fixture.control.endpoint,
                "POST",
                &format!("/v1/button/press/{delay}"),
                Some(generation),
                Duration::from_secs(1),
            )
            .unwrap_err();
            assert_eq!(error.code, Code::ControlUnreachable, "{delay}");
        }

        // The original hold still requires an explicit release
        assert!(fixture.hardware.snapshot().cli_pressed);
        button(
            &fixture.control.endpoint,
            false,
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(fixture.edge(), "rising");
    }

    /// Old launchers reject timed inputs without receiving an untimed fallback.
    #[test]
    fn test_an_older_launcher_never_receives_an_untimed_fallback() {
        // Serve the previous control protocol with no timed route
        let server = Server::http((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = Endpoint {
            port: server.server_addr().to_ip().unwrap().port(),
            id: "1".repeat(64),
        };
        let worker = thread::spawn(move || {
            let get = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert_eq!(get.method(), &Method::Get);
            get.respond(Response::from_string(
                json!({"generation":"1","connected":true,"pressed":false,"cli_pressed":false})
                    .to_string(),
            ))
            .unwrap();
            let post = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert_eq!(post.url(), "/v1/button/press/1");
            post.respond(
                Response::from_string(r#"{"error":"no such control route"}"#)
                    .with_status_code(StatusCode(404)),
            )
            .unwrap();
            assert!(
                server
                    .recv_timeout(Duration::from_millis(150))
                    .unwrap()
                    .is_none()
            );
        });

        // Failure reports the missing support without replaying the request
        let error = button(&endpoint, true, Some(1), Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.code, Code::ControlUnsupported);
        worker.join().unwrap();
    }

    /// A guest restart clears every hold and rejects old connection generations.
    #[test]
    fn test_restart_rejects_stale_inputs_and_clears_holds() {
        let mut fixture = Fixture::new();
        let timeout = Duration::from_secs(1);
        let generation = fixture.hardware.snapshot().generation;
        // Leave a timer pending across the guest restart
        button(&fixture.control.endpoint, true, Some(2), timeout).unwrap();
        assert_eq!(fixture.edge(), "falling");
        fixture.peer.close(None).unwrap();
        wait_for(|| !fixture.hardware.snapshot().connected);
        assert_eq!(
            button(&fixture.control.endpoint, false, None, timeout)
                .unwrap_err()
                .code,
            Code::ButtonUnavailable
        );
        fixture.peer = accept(&fixture.listener);
        wait_for(|| fixture.hardware.snapshot().connected);
        let state = fixture.hardware.snapshot();
        assert!(!state.pressed && !state.cli_pressed && !state.ui_pressed);
        assert_eq!(
            request(
                &fixture.control.endpoint,
                "POST",
                "/v1/button/press",
                Some(generation),
                timeout
            )
            .unwrap_err()
            .code,
            Code::ButtonUnavailable
        );
        button(&fixture.control.endpoint, true, None, timeout).unwrap();
        assert_eq!(fixture.edge(), "falling");

        // A timer from the old connection must not release this new hold
        thread::sleep(Duration::from_millis(2200));
        assert!(fixture.hardware.snapshot().cli_pressed);
        button(&fixture.control.endpoint, false, None, timeout).unwrap();
        assert_eq!(fixture.edge(), "rising");
    }

    /// Raw requests exercise browser rejection, bounded bodies and stale launches.
    #[test]
    fn test_control_rejects_browser_requests_bodies_and_wrong_targets() {
        let fixture = Fixture::new();
        let endpoint = &fixture.control.endpoint;
        for (headers, body, expected) in [
            (String::new(), "", 412),
            (
                format!("{INSTANCE_HEADER}: {}\r\n", "0".repeat(64)),
                "",
                412,
            ),
            (
                format!(
                    "{INSTANCE_HEADER}: {}\r\nOrigin: https://example.com\r\n",
                    endpoint.id
                ),
                "",
                403,
            ),
            (format!("{INSTANCE_HEADER}: {}\r\n", endpoint.id), "x", 413),
            (format!("{INSTANCE_HEADER}: {}\r\n", endpoint.id), "", 400),
        ] {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            write!(stream, "POST /v1/button/press HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len()).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            assert_eq!(
                response
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u16>()
                    .unwrap(),
                expected
            );
            assert!(
                !response
                    .to_ascii_lowercase()
                    .contains("access-control-allow-origin")
            );
            assert!(!fixture.hardware.snapshot().pressed);
        }
        let mut stale = endpoint.clone();
        stale.id = "0".repeat(64);
        assert_eq!(
            button(&stale, true, None, Duration::from_secs(1))
                .unwrap_err()
                .code,
            Code::ControlUnreachable
        );
    }

    /// Losing a POST reply times out with an unknown outcome and no replay.
    #[test]
    fn test_a_missing_delivery_reply_is_not_retried() {
        let server = Server::http((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = Endpoint {
            port: server.server_addr().to_ip().unwrap().port(),
            id: "1".repeat(64),
        };
        let worker = thread::spawn(move || {
            let get = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert_eq!(get.method(), &Method::Get);
            get.respond(Response::from_string(
                json!({"generation":"1","connected":true,"pressed":false,"cli_pressed":false})
                    .to_string(),
            ))
            .unwrap();
            let post = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert_eq!(post.url(), "/v1/button/press");
            assert!(
                server
                    .recv_timeout(Duration::from_millis(150))
                    .unwrap()
                    .is_none()
            );
            drop(post);
        });
        let error = button(&endpoint, true, None, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.code, Code::Timeout);
        assert!(
            error
                .hints
                .iter()
                .any(|hint| hint.contains("state is unknown"))
        );
        worker.join().unwrap();
    }
}
