// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Owns the hardware socket and the current device state independently of a UI.
//!
//! One connection lasts for the firmware's lifetime. A restart opens a new
//! connection; loading or suspending a view never touches it. State readers
//! keep only the latest snapshot, and button requests are bounded and never
//! replayed across connections.

use std::io::ErrorKind;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};

use crate::diagnostics::log;

/// Pause between failed connections, including while the guest is booting.
const RETRY: Duration = Duration::from_secs(1);
/// Bounds socket writes without limiting how long boot may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Limits button latency when the guest sends no hardware frames.
const INPUT_POLL: Duration = Duration::from_millis(10);
/// Hardware frames contain only a few LED values or identity claims.
const MAX_FRAME: usize = 64 * 1024;
/// GPIO for the active-low reset button on the emulated carrier.
const BUTTON_PIN: &str = "5";
/// GPIO selecting firmware control of the LEDs.
const LED_SWITCH_PIN: &str = "22";
/// Version and revision of the carrier presented to the guest.
const CARRIER: (u8, u8) = (1, 11);

/// Which source the device face should render.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// No guest has been launched.
    #[default]
    Idle,
    /// The guest has started but has not taken control of its LEDs.
    Booting,
    /// The firmware supplies LED colors.
    Firmware,
    /// The guest has exited.
    Stopped,
}

/// Identity claims last reported by the guest, without attestation checks.
#[derive(Clone, Default, PartialEq, Serialize)]
pub(crate) struct Nameplate {
    /// Whether any nameplate has arrived in this connection.
    pub(crate) known: bool,
    /// Cloud environment bound to the image.
    pub(crate) env: Option<String>,
    /// Device name, absent when cleared.
    pub(crate) name: Option<String>,
    /// Attested serial, when enrolled.
    pub(crate) serial: Option<String>,
    /// Attestation expiry in Unix seconds.
    pub(crate) expiry: Option<u64>,
}

impl Nameplate {
    /// Apply a partial update atomically, distinguishing omission from null.
    fn merge(&mut self, payload: Value) -> Result<()> {
        let claims = payload.as_object().context("nameplate is not an object")?;
        let mut next = self.clone();
        if let Some(value) = claims.get("env") {
            next.env = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = claims.get("name") {
            next.name = serde_json::from_value::<Option<String>>(value.clone())?
                .filter(|name| !name.is_empty());
        }
        if let Some(value) = claims.get("serial") {
            next.serial = serde_json::from_value(value.clone())?;
        }
        if let Some(value) = claims.get("expiry") {
            next.expiry = serde_json::from_value(value.clone())?;
        }
        next.known = true;
        *self = next;
        Ok(())
    }
}

/// A complete device view, also available before the guest starts.
#[derive(Clone, Default, Serialize)]
pub(crate) struct State {
    /// Monotonic revision used to reject stale IPC replies.
    pub(crate) revision: u64,
    /// Connection that user inputs belong to, changing after a guest restart.
    pub(crate) generation: u64,
    /// Whether the hardware socket is attached.
    pub(crate) connected: bool,
    /// Which LED source is active.
    pub(crate) phase: Phase,
    /// Raw RGB intensities, before presentation brightness and glow.
    pub(crate) colors: [[f64; 3]; 4],
    /// Whether a button press has been sent on this connection.
    pub(crate) pressed: bool,
    /// Whether the window currently holds the button.
    pub(crate) ui_pressed: bool,
    /// Whether the command line currently holds the button.
    pub(crate) cli_pressed: bool,
    /// Latest identity claims from this connection.
    pub(crate) nameplate: Nameplate,
}

/// Independent holders of the emulated button.
#[derive(Clone, Copy)]
pub(crate) enum ButtonSource {
    /// Pointer input and window cleanup.
    Ui,
    /// Explicit commands, persisting after the CLI exits.
    Cli,
}

/// Button state after an input has been delivered or found already applied.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ButtonOutcome {
    /// Physical button state, combining the window and CLI holds.
    pub(crate) pressed: bool,
    /// Whether a CLI hold remains active.
    pub(crate) cli_pressed: bool,
    /// Whether this request changed its source's hold or release schedule.
    pub(crate) changed: bool,
    /// Release interval accepted by this request, measured from delivery.
    #[serde(default)]
    pub(crate) release_after_seconds: Option<u32>,
}

/// A button input and the connection on which it was requested.
struct Button {
    /// Holder whose state is being changed.
    source: ButtonSource,
    /// Desired hold for this input source.
    pressed: bool,
    /// Connection generation, preventing delayed inputs reaching a new guest.
    generation: u64,
    /// Automatic release of this CLI hold, measured from successful delivery.
    release_after: Option<u32>,
    /// Reports whether the edge was written to the socket.
    reply: mpsc::Sender<Result<ButtonOutcome, String>>,
}

/// The active worker and its bounded input queue.
struct Connection {
    /// Inputs awaiting delivery to the guest.
    buttons: mpsc::SyncSender<Button>,
    /// Worker joined when QEMU exits.
    thread: JoinHandle<()>,
}

/// Shared state and worker ownership for one emulated device.
struct Inner {
    /// Latest snapshot, without a queue of old LED frames.
    state: Mutex<State>,
    /// Absent until launch, and after shutdown.
    connection: Mutex<Option<Connection>>,
    /// Prevents reconnects after QEMU exits.
    stopping: AtomicBool,
    /// A duplicate handle lets shutdown interrupt a pending handshake or I/O.
    socket: Mutex<Option<TcpStream>>,
}

/// The runtime's hardware controller, shared with optional state readers.
#[derive(Clone)]
pub(crate) struct Controller(Arc<Inner>);

impl Default for Controller {
    fn default() -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            connection: Mutex::new(None),
            stopping: AtomicBool::new(false),
            socket: Mutex::new(None),
        }))
    }
}

impl Controller {
    /// Read a complete snapshot without waiting on networking or a UI.
    pub(crate) fn snapshot(&self) -> State {
        self.0.state.lock().unwrap().clone()
    }

    /// Start the sole hardware connection after QEMU has been spawned.
    pub(crate) fn start(&self, address: SocketAddr) {
        let mut connection = self.0.connection.lock().unwrap();
        assert!(connection.is_none(), "hardware already started");
        self.update(|state| state.phase = Phase::Booting);
        let (buttons, receiver) = mpsc::sync_channel(16);
        let controller = self.clone();
        let thread = thread::spawn(move || controller.run(address, receiver));
        *connection = Some(Connection { buttons, thread });
    }

    /// Stop reconnecting when the guest exits and clear its transient state.
    pub(crate) fn stop(&self) {
        let connection = self.0.connection.lock().unwrap().take();
        self.0.stopping.store(true, Ordering::SeqCst);
        self.update(|state| {
            state.connected = false;
            state.pressed = false;
            state.ui_pressed = false;
            state.cli_pressed = false;
            state.phase = Phase::Stopped;
        });
        if let Some(socket) = self.0.socket.lock().unwrap().take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(connection) = connection {
            connection.thread.thread().unpark();
            let _ = connection.thread.join();
        }
    }

    /// Send an explicit button input, failing instead of queuing it for a reboot.
    pub(crate) fn button(
        &self,
        source: ButtonSource,
        pressed: bool,
        generation: u64,
        release_after: Option<u32>,
    ) -> Result<ButtonOutcome, String> {
        self.enqueue_button(source, pressed, generation, release_after)?
            .recv()
            .unwrap_or_else(|_| {
                Err("The device disconnected before accepting the input.".to_owned())
            })
    }

    /// Queue a release on view dismissal without making its event loop wait.
    pub(crate) fn release_button(&self) {
        let _ = self.enqueue_button(ButtonSource::Ui, false, self.snapshot().generation, None);
    }

    /// Validate and queue an input, returning its delivery acknowledgement.
    fn enqueue_button(
        &self,
        source: ButtonSource,
        pressed: bool,
        generation: u64,
        release_after: Option<u32>,
    ) -> Result<mpsc::Receiver<Result<ButtonOutcome, String>>, String> {
        // Only a CLI press can own an automatic release
        if release_after.is_some() && (!pressed || !matches!(source, ButtonSource::Cli)) {
            return Err("Automatic release requires a CLI press.".to_owned());
        }

        // Reject input before enqueueing it for a different connection
        let state = self.snapshot();
        if !state.connected || state.generation != generation {
            return Err("The device is not connected.".to_owned());
        }
        let connection = self.0.connection.lock().unwrap();
        let connection = connection.as_ref().ok_or("The device has stopped.")?;
        let (reply, receiver) = mpsc::channel();
        connection
            .buttons
            .try_send(Button {
                pressed,
                source,
                generation,
                release_after,
                reply,
            })
            .map_err(|_| "The device cannot accept another button input.".to_owned())?;
        Ok(receiver)
    }

    /// Serialize state changes without waiting for networking or a frontend.
    fn update(&self, change: impl FnOnce(&mut State)) {
        let mut state = self.0.state.lock().unwrap();
        if state.phase != Phase::Stopped {
            change(&mut state);
            state.revision += 1;
        }
    }

    /// Attach when the guest listens, preserving pending handshakes during boot.
    fn run(&self, address: SocketAddr, buttons: mpsc::Receiver<Button>) {
        while !self.0.stopping.load(Ordering::SeqCst) {
            match self.connect(address) {
                Ok(socket) => {
                    self.update(|state| {
                        state.connected = true;
                        state.generation += 1;
                    });
                    let generation = self.snapshot().generation;
                    log!("[hardware] connected to {address}");
                    if let Err(err) = self.serve(socket, &buttons, generation) {
                        log!("[hardware] connection ended: {err:#}");
                    }
                    self.update(|state| {
                        state.connected = false;
                        state.pressed = false;
                        state.ui_pressed = false;
                        state.cli_pressed = false;
                        state.phase = Phase::Booting;
                        state.colors = [[0.0; 3]; 4];
                        state.nameplate = Nameplate::default();
                    });
                }
                Err(err) => log!("[hardware] waiting for guest: {err}"),
            }
            self.0.socket.lock().unwrap().take();
            while let Ok(button) = buttons.try_recv() {
                let _ = button
                    .reply
                    .send(Err("The device disconnected.".to_owned()));
            }
            if !self.0.stopping.load(Ordering::SeqCst) {
                thread::park_timeout(RETRY);
            }
        }
    }

    /// Keep the handshake blocking until the guest responds or shutdown closes it.
    fn connect(&self, address: SocketAddr) -> Result<WebSocket<TcpStream>> {
        let stream = TcpStream::connect_timeout(&address, RETRY)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        {
            let mut socket = self.0.socket.lock().unwrap();
            if self.0.stopping.load(Ordering::SeqCst) {
                bail!("hardware has stopped");
            }
            *socket = Some(stream.try_clone()?);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        // QEMU can accept TCP before the guest boots. Abandoning that
        // handshake could consume the guest's only hardware connection.
        let (socket, _) = tungstenite::client::client_with_config(
            format!("ws://{address}/v1/hw"),
            stream,
            Some(config),
        )?;
        socket.get_ref().set_read_timeout(Some(INPUT_POLL))?;
        Ok(socket)
    }

    /// Consume hardware frames and explicit inputs on one ordered connection.
    fn serve(
        &self,
        mut socket: WebSocket<TcpStream>,
        buttons: &mpsc::Receiver<Button>,
        generation: u64,
    ) -> Result<()> {
        // This deadline dies with the connection, so it cannot release a new guest
        let mut release_at = None;
        while !self.0.stopping.load(Ordering::SeqCst) {
            // Service deadlines even while inputs or LED frames arrive continuously
            if release_at.is_some_and(|deadline| Instant::now() >= deadline) {
                self.apply_button(&mut socket, ButtonSource::Cli, false, None, &mut release_at)?;
            }

            // Every accepted CLI input replaces the previous release schedule
            if let Ok(button) = buttons.try_recv() {
                if button.generation != generation {
                    let _ = button.reply.send(Err(
                        "The device restarted before accepting the input.".to_owned(),
                    ));
                } else {
                    match self.apply_button(
                        &mut socket,
                        button.source,
                        button.pressed,
                        button.release_after,
                        &mut release_at,
                    ) {
                        Ok(outcome) => {
                            let _ = button.reply.send(Ok(outcome));
                        }
                        Err(err) => {
                            let _ = button
                                .reply
                                .send(Err("The button input could not be delivered.".to_owned()));
                            return Err(err);
                        }
                    }
                }
            }

            // Idle reads return frequently enough to deliver button edges and timers
            match socket.read() {
                Ok(Message::Text(text)) => match self.frame(text.as_str()) {
                    Ok(Some(reply)) => socket.send(reply.into())?,
                    Ok(None) => {}
                    Err(err) => log!("[hardware] ignoring malformed frame: {err}"),
                },
                Ok(Message::Close(_)) => return Ok(()),
                Ok(Message::Ping(_)) => socket.flush()?,
                Ok(_) => {}
                Err(tungstenite::Error::Io(err))
                    if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// Deliver an edge before committing a hold or starting its release timer.
    fn apply_button(
        &self,
        socket: &mut WebSocket<TcpStream>,
        source: ButtonSource,
        held: bool,
        release_after: Option<u32>,
        release_at: &mut Option<Instant>,
    ) -> Result<ButtonOutcome> {
        // Combine independent holders into the physical button state
        let mut state = self.snapshot();
        let current = match source {
            ButtonSource::Ui => &mut state.ui_pressed,
            ButtonSource::Cli => &mut state.cli_pressed,
        };
        let mut changed = *current != held;
        *current = held;
        let pressed = state.ui_pressed || state.cli_pressed;
        if state.pressed != pressed {
            let frame = json!({
                "d": "button", "id": BUTTON_PIN,
                "payload": { "edge": if pressed { "falling" } else { "rising" } }
            });
            socket.send(frame.to_string().into())?;
        }

        // A fresh deadline starts after delivery, including a repeated timed press
        if matches!(source, ButtonSource::Cli) {
            changed |= release_at.is_some() || release_after.is_some();
            *release_at = release_after
                .map(|seconds| Instant::now() + Duration::from_secs(u64::from(seconds)));
        }
        if changed {
            self.update(|current| {
                current.pressed = pressed;
                current.ui_pressed = state.ui_pressed;
                current.cli_pressed = state.cli_pressed;
            });
        }

        // Complete a zero-delay release before replying or reading another frame
        if release_after == Some(0) {
            let released = self.apply_button(socket, ButtonSource::Cli, false, None, release_at)?;
            return Ok(ButtonOutcome {
                changed: changed || released.changed,
                release_after_seconds: Some(0),
                ..released
            });
        }
        Ok(ButtonOutcome {
            pressed,
            cli_pressed: state.cli_pressed,
            changed,
            release_after_seconds: release_after,
        })
    }

    /// Decode a driver frame, returning only replies the protocol requires.
    fn frame(&self, text: &str) -> Result<Option<String>> {
        let frame: Frame = serde_json::from_str(text)?;
        match frame.d.as_str() {
            "revbits" if frame.payload.get("op").and_then(Value::as_str) == Some("read") => {
                return Ok(Some(
                    json!({
                        "d": "revbits", "id": frame.id,
                        "payload": { "version": CARRIER.0, "revision": CARRIER.1 }
                    })
                    .to_string(),
                ));
            }
            "rgbled" => {
                let colors: [[f64; 3]; 4] =
                    serde_json::from_value(frame.payload["colors"].clone())?;
                if colors
                    .iter()
                    .flatten()
                    .any(|color| !color.is_finite() || *color < 0.0)
                {
                    bail!("invalid LED intensity");
                }
                self.update(|state| state.colors = colors);
            }
            "switch" if frame.id == LED_SWITCH_PIN => {
                let phase = match frame.payload["level"].as_str() {
                    Some("high") => Phase::Firmware,
                    Some("low") => Phase::Booting,
                    _ => bail!("invalid LED switch level"),
                };
                self.update(|state| state.phase = phase);
            }
            "nameplate" => {
                let mut nameplate = self.snapshot().nameplate;
                nameplate.merge(frame.payload)?;
                self.update(|state| state.nameplate = nameplate);
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Routing envelope shared by the hardware drivers.
#[derive(Deserialize)]
struct Frame {
    /// Driver tag; unknown tags are ignored.
    d: String,
    /// Driver instance, such as a pin or carrier address.
    id: String,
    /// Driver-specific fields.
    payload: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Start the production controller against a loopback hardware peer.
    fn connect() -> (Controller, TcpListener, WebSocket<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let controller = Controller::default();
        controller.start(listener.local_addr().unwrap());
        let socket = tungstenite::accept(accept(&listener)).unwrap();
        wait_for(&controller, |state| state.connected);
        (controller, listener, socket)
    }

    /// Accept the controller with a deadline so regressions cannot hang tests.
    fn accept(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(500)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    return stream;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "hardware did not connect");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        }
    }

    /// Wait for a state reached through socket traffic under a test deadline.
    fn wait_for(controller: &Controller, predicate: impl Fn(&State) -> bool) -> State {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = controller.snapshot();
            if predicate(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "hardware state did not arrive");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Read the next hardware reply under the carrier's response deadline.
    fn read(socket: &mut WebSocket<TcpStream>) -> Value {
        let frame = socket.read().unwrap();
        serde_json::from_str(frame.to_text().unwrap()).unwrap()
    }

    /// Boot and merge state without any frontend reading notifications.
    #[test]
    fn test_hardware_boot_and_partial_nameplates_need_no_frontend() {
        let (controller, _listener, mut socket) = connect();
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .unwrap();
        assert_eq!(
            read(&mut socket),
            json!({
                "d": "revbits", "id": "i2c@0x20", "payload": {"version": 1, "revision": 11}
            })
        );
        socket.send(r#"{"d":"nameplate","id":"self","payload":{"env":"develop","name":"test device","serial":"test-serial","expiry":1800000000}}"#.into()).unwrap();
        socket
            .send(r#"{"d":"nameplate","id":"self","payload":{"name":""}}"#.into())
            .unwrap();
        let state = wait_for(&controller, |state| {
            state.nameplate.known && state.nameplate.name.is_none()
        });
        assert_eq!(state.nameplate.env.as_deref(), Some("develop"));
        assert_eq!(state.nameplate.serial.as_deref(), Some("test-serial"));
        assert_eq!(state.nameplate.expiry, Some(1_800_000_000));
        socket
            .send(r#"{"d":"nameplate","id":"self","payload":{"expiry":null}}"#.into())
            .unwrap();
        wait_for(&controller, |state| state.nameplate.expiry.is_none());

        socket.send(r#"{"d":"rgbled","id":"0","payload":{"colors":[[0.1,0.2,0.3],[0,0,0],[1,1,1],[0.4,0,0]]}}"#.into()).unwrap();
        let state = wait_for(&controller, |state| state.colors[0][0] == 0.1);
        assert!(state.phase == Phase::Booting);
        socket
            .send(r#"{"d":"switch","id":"22","payload":{"level":"high"}}"#.into())
            .unwrap();
        let state = wait_for(&controller, |state| state.phase == Phase::Firmware);
        assert_eq!(state.colors[0], [0.1, 0.2, 0.3]);
        socket
            .send(r#"{"d":"switch","id":"22","payload":{"level":"low"}}"#.into())
            .unwrap();
        wait_for(&controller, |state| state.phase == Phase::Booting);
        controller.stop();
    }

    /// Bad frames leave state intact and do not prevent required replies.
    #[test]
    fn test_invalid_and_unknown_frames_do_not_corrupt_state() {
        let (controller, _listener, mut socket) = connect();
        for frame in [
            "not json",
            r#"{"d":"unknown","id":"0","payload":{"future":true}}"#,
            r#"{"d":"nameplate","id":"self","payload":{"env":"develop","expiry":"invalid"}}"#,
            r#"{"d":"rgbled","id":"0","payload":{"colors":[[1,0,0]]}}"#,
            r#"{"d":"switch","id":"99","payload":{"level":"high"}}"#,
        ] {
            socket.send(frame.into()).unwrap();
        }
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .unwrap();
        read(&mut socket);
        let state = controller.snapshot();
        assert!(!state.nameplate.known);
        assert!(state.nameplate.env.is_none());
        assert!(state.phase == Phase::Booting);
        assert_eq!(state.colors, [[0.0; 3]; 4]);
        controller.stop();
    }

    /// Button edges are ordered, deduplicated and cleared on a guest restart.
    #[test]
    fn test_button_edges_and_reconnect_reset_transient_state() {
        let (controller, listener, mut socket) = connect();
        let generation = controller.snapshot().generation;
        controller
            .button(ButtonSource::Ui, true, generation, None)
            .unwrap();
        assert_eq!(
            read(&mut socket),
            json!({
                "d": "button", "id": "5", "payload": {"edge": "falling"}
            })
        );
        controller
            .button(ButtonSource::Ui, true, generation, None)
            .unwrap();
        controller
            .button(ButtonSource::Ui, false, generation, None)
            .unwrap();
        assert_eq!(
            read(&mut socket),
            json!({
                "d": "button", "id": "5", "payload": {"edge": "rising"}
            })
        );
        controller
            .button(ButtonSource::Ui, true, generation, None)
            .unwrap();
        read(&mut socket);
        socket
            .send(r#"{"d":"nameplate","id":"self","payload":{"name":"before restart"}}"#.into())
            .unwrap();
        wait_for(&controller, |state| state.nameplate.known);
        socket.close(None).unwrap();
        drop(socket);
        let state = wait_for(&controller, |state| !state.connected);
        assert!(!state.pressed);
        assert!(!state.nameplate.known);
        assert!(state.phase == Phase::Booting);
        assert!(
            controller
                .button(ButtonSource::Ui, true, generation, None)
                .is_err()
        );
        let mut socket = tungstenite::accept(accept(&listener)).unwrap();
        wait_for(&controller, |state| state.connected);
        assert!(
            controller
                .button(ButtonSource::Ui, true, generation, None)
                .is_err()
        );
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .unwrap();
        assert_eq!(read(&mut socket)["d"], "revbits");
        controller.stop();
        assert!(controller.snapshot().phase == Phase::Stopped);
        controller
            .frame(r#"{"d":"switch","id":"22","payload":{"level":"high"}}"#)
            .unwrap();
        assert!(controller.snapshot().phase == Phase::Stopped);
    }

    /// A pending handshake survives boot delays on QEMU's forwarded port.
    #[test]
    fn test_a_pending_boot_connection_is_not_replaced() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let controller = Controller::default();
        controller.start(listener.local_addr().unwrap());
        let stream = accept(&listener);
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        let mut socket = tungstenite::accept(stream).unwrap();
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .unwrap();
        assert_eq!(read(&mut socket)["payload"]["revision"], 11);
        controller.stop();
    }

    /// Stopping closes a blocked handshake and joins the socket worker promptly.
    #[test]
    fn test_stop_interrupts_a_pending_handshake() {
        use std::io::Read as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let controller = Controller::default();
        controller.start(listener.local_addr().unwrap());
        let mut stream = accept(&listener);
        let (done, finished) = mpsc::channel();
        let worker = controller.clone();
        thread::spawn(move || {
            worker.stop();
            done.send(()).unwrap();
        });
        finished.recv_timeout(Duration::from_millis(500)).unwrap();
        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        assert!(controller.snapshot().phase == Phase::Stopped);
    }

    /// Idle reads do not hold up input delivery or a release after focus loss.
    #[test]
    fn test_idle_socket_accepts_button_inputs() {
        let (controller, _listener, mut socket) = connect();
        thread::sleep(Duration::from_millis(50));
        let reply = controller
            .enqueue_button(
                ButtonSource::Ui,
                true,
                controller.snapshot().generation,
                None,
            )
            .unwrap();
        reply
            .recv_timeout(Duration::from_millis(500))
            .unwrap()
            .unwrap();
        assert_eq!(read(&mut socket)["payload"]["edge"], "falling");
        controller.release_button();
        assert_eq!(read(&mut socket)["payload"]["edge"], "rising");
        wait_for(&controller, |state| !state.pressed);
        controller.stop();
    }

    /// A read timeout preserves an incomplete message and still permits inputs.
    #[test]
    fn test_fragmented_frames_survive_idle_reads() {
        use tungstenite::protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        };
        let (controller, _listener, mut socket) = connect();
        socket
            .send(Message::Frame(Frame::message(
                br#"{"d":"revbits","id":"carrier","payload":"#.as_slice(),
                OpCode::Data(Data::Text),
                false,
            )))
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let reply = controller
            .enqueue_button(
                ButtonSource::Ui,
                true,
                controller.snapshot().generation,
                None,
            )
            .unwrap();
        reply
            .recv_timeout(Duration::from_millis(500))
            .unwrap()
            .unwrap();
        assert_eq!(read(&mut socket)["payload"]["edge"], "falling");
        socket
            .send(Message::Frame(Frame::message(
                br#"{"op":"read"}}"#.as_slice(),
                OpCode::Data(Data::Continue),
                true,
            )))
            .unwrap();
        assert_eq!(read(&mut socket)["payload"]["revision"], 11);
        controller.stop();
    }

    /// Oversized messages terminate the connection before any state is applied.
    #[test]
    fn test_oversized_hardware_frames_disconnect() {
        let (controller, _listener, mut socket) = connect();
        socket.send("x".repeat(65 * 1024).into()).unwrap();
        let state = wait_for(&controller, |state| !state.connected);
        assert!(!state.nameplate.known);
        controller.stop();
    }
}
