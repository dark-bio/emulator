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

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

use crate::diagnostics::log;

/// Pause between failed connections, including while the guest is booting.
const RETRY: Duration = Duration::from_secs(1);
/// Bounds socket writes without limiting how long boot may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
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
    /// Latest identity claims from this connection.
    pub(crate) nameplate: Nameplate,
}

/// A button input and the connection on which it was requested.
struct Button {
    /// Desired physical button state.
    pressed: bool,
    /// Connection generation, preventing delayed inputs reaching a new guest.
    generation: u64,
    /// Reports whether the edge was written to the socket.
    reply: oneshot::Sender<Result<(), String>>,
}

/// The active worker and its bounded input queue.
struct Connection {
    /// Inputs awaiting delivery to the guest.
    buttons: mpsc::Sender<Button>,
    /// Worker aborted when QEMU exits.
    task: JoinHandle<()>,
}

/// Shared state and worker ownership for one emulated device.
struct Inner {
    /// Latest state, with coalesced notifications for observers.
    state: watch::Sender<State>,
    /// Absent until launch, and after shutdown.
    connection: Mutex<Option<Connection>>,
}

/// The runtime's hardware controller, shared with optional state readers.
#[derive(Clone)]
pub(crate) struct Controller(Arc<Inner>);

impl Default for Controller {
    fn default() -> Self {
        Self(Arc::new(Inner {
            state: watch::channel(State::default()).0,
            connection: Mutex::new(None),
        }))
    }
}

impl Controller {
    /// Read a complete snapshot without waiting on networking or a UI.
    pub(crate) fn snapshot(&self) -> State {
        self.0.state.borrow().clone()
    }

    /// Subscribe to the latest state without accumulating old LED frames.
    pub(crate) fn subscribe(&self) -> watch::Receiver<State> {
        self.0.state.subscribe()
    }

    /// Start the sole hardware connection after QEMU has been spawned.
    pub(crate) fn start(&self, executor: &tokio::runtime::Handle, address: SocketAddr) {
        let mut connection = self.0.connection.lock().unwrap();
        assert!(connection.is_none(), "hardware already started");
        self.update(|state| state.phase = Phase::Booting);
        let (buttons, receiver) = mpsc::channel(16);
        let controller = self.clone();
        let task = executor.spawn(async move { controller.run(address, receiver).await });
        *connection = Some(Connection { buttons, task });
    }

    /// Stop reconnecting when the guest exits and clear its transient state.
    pub(crate) fn stop(&self) {
        if let Some(connection) = self.0.connection.lock().unwrap().take() {
            connection.task.abort();
        }
        self.update(|state| {
            state.connected = false;
            state.pressed = false;
            state.phase = Phase::Stopped;
        });
    }

    /// Send an explicit button input, failing instead of queuing it for a reboot.
    pub(crate) async fn button(&self, pressed: bool, generation: u64) -> Result<(), String> {
        let receiver = {
            let state = self.snapshot();
            if !state.connected || state.generation != generation {
                return Err("The device is not connected.".to_owned());
            }
            let connection = self.0.connection.lock().unwrap();
            let connection = connection.as_ref().ok_or("The device has stopped.")?;
            let (reply, receiver) = oneshot::channel();
            connection
                .buttons
                .try_send(Button {
                    pressed,
                    generation,
                    reply,
                })
                .map_err(|_| "The device cannot accept another button input.".to_owned())?;
            receiver
        };
        receiver.await.unwrap_or_else(|_| {
            Err("The device disconnected before accepting the input.".to_owned())
        })
    }

    /// Serialize state changes and notify readers without waiting for them.
    fn update(&self, change: impl FnOnce(&mut State)) {
        self.0.state.send_if_modified(|state| {
            // An aborted worker may finish its current synchronous frame
            if state.phase == Phase::Stopped {
                return false;
            }
            change(state);
            state.revision += 1;
            true
        });
    }

    /// Attach when the guest listens, preserving pending handshakes during boot.
    async fn run(&self, address: SocketAddr, mut buttons: mpsc::Receiver<Button>) {
        let url = format!("ws://{address}/v1/hw");
        loop {
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_FRAME))
                .max_frame_size(Some(MAX_FRAME));
            // QEMU can accept TCP before the guest boots. Abandoning that
            // handshake could consume the guest's only hardware connection.
            match tokio_tungstenite::connect_async_with_config(&url, Some(config), true).await {
                Ok((socket, _)) => {
                    self.update(|state| {
                        state.connected = true;
                        state.generation += 1;
                    });
                    let generation = self.snapshot().generation;
                    log!("[hardware] connected to {address}");
                    if let Err(err) = self.serve(socket, &mut buttons, generation).await {
                        log!("[hardware] connection ended: {err:#}");
                    }
                    self.update(|state| {
                        state.connected = false;
                        state.pressed = false;
                        state.phase = Phase::Booting;
                        state.colors = [[0.0; 3]; 4];
                        state.nameplate = Nameplate::default();
                    });
                }
                Err(err) => log!("[hardware] waiting for guest: {err}"),
            }
            while let Ok(button) = buttons.try_recv() {
                let _ = button
                    .reply
                    .send(Err("The device disconnected.".to_owned()));
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    /// Consume hardware frames and explicit inputs on one ordered connection.
    async fn serve<S>(
        &self,
        mut socket: tokio_tungstenite::WebSocketStream<S>,
        buttons: &mut mpsc::Receiver<Button>,
        generation: u64,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            tokio::select! {
                frame = socket.next() => {
                    match frame.transpose()? {
                        Some(Message::Text(text)) => match self.frame(text.as_str()) {
                            Ok(Some(reply)) => {
                                tokio::time::timeout(WRITE_TIMEOUT, socket.send(reply.into())).await??;
                            }
                            Ok(None) => {}
                            Err(err) => log!("[hardware] ignoring malformed frame: {err}"),
                        },
                        Some(Message::Close(_)) | None => return Ok(()),
                        Some(Message::Ping(_)) => {
                            tokio::time::timeout(WRITE_TIMEOUT, socket.flush()).await??;
                        }
                        Some(_) => {}
                    }
                }
                Some(button) = buttons.recv() => {
                    if button.generation != generation {
                        let _ = button.reply.send(Err("The device restarted before accepting the input.".to_owned()));
                        continue;
                    }
                    if self.snapshot().pressed != button.pressed {
                        let frame = json!({
                            "d": "button", "id": BUTTON_PIN,
                            "payload": { "edge": if button.pressed { "falling" } else { "rising" } }
                        });
                        let result = tokio::time::timeout(WRITE_TIMEOUT, socket.send(frame.to_string().into())).await;
                        if let Err(err) = result.context("button write timed out").and_then(|result| result.map_err(Into::into)) {
                            let _ = button.reply.send(Err("The button input could not be delivered.".to_owned()));
                            return Err(err);
                        }
                        self.update(|state| state.pressed = button.pressed);
                    }
                    let _ = button.reply.send(Ok(()));
                }
            }
        }
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
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_tungstenite::{WebSocketStream, accept_async};

    /// Start the production controller against a loopback hardware peer.
    async fn connect() -> (Controller, TcpListener, WebSocketStream<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let controller = Controller::default();
        controller.start(
            &tokio::runtime::Handle::current(),
            listener.local_addr().unwrap(),
        );
        let socket = accept(&listener).await;
        wait_for(&controller, |state| state.connected).await;
        (controller, listener, socket)
    }

    /// Accept the controller with a deadline so regressions cannot hang tests.
    async fn accept(listener: &TcpListener) -> WebSocketStream<TcpStream> {
        timeout(Duration::from_secs(5), async {
            accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap()
        })
        .await
        .unwrap()
    }

    /// Wait for a state reached through socket traffic, without polling sleeps.
    async fn wait_for(controller: &Controller, predicate: impl Fn(&State) -> bool) -> State {
        timeout(Duration::from_secs(5), async {
            let mut states = controller.subscribe();
            loop {
                let state = states.borrow_and_update().clone();
                if predicate(&state) {
                    return state;
                }
                states.changed().await.unwrap();
            }
        })
        .await
        .unwrap()
    }

    /// Read the next hardware reply under the carrier's response deadline.
    async fn read(socket: &mut WebSocketStream<TcpStream>) -> Value {
        let frame = timeout(Duration::from_millis(500), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(frame.to_text().unwrap()).unwrap()
    }

    /// Boot and merge state without any frontend reading notifications.
    #[tokio::test]
    async fn test_hardware_boot_and_partial_nameplates_need_no_frontend() {
        let (controller, _listener, mut socket) = connect().await;
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .await
            .unwrap();
        assert_eq!(
            read(&mut socket).await,
            json!({
                "d": "revbits", "id": "i2c@0x20", "payload": {"version": 1, "revision": 11}
            })
        );
        socket.send(r#"{"d":"nameplate","id":"self","payload":{"env":"develop","name":"test device","serial":"test-serial","expiry":1800000000}}"#.into()).await.unwrap();
        socket
            .send(r#"{"d":"nameplate","id":"self","payload":{"name":""}}"#.into())
            .await
            .unwrap();
        let state = wait_for(&controller, |state| {
            state.nameplate.known && state.nameplate.name.is_none()
        })
        .await;
        assert_eq!(state.nameplate.env.as_deref(), Some("develop"));
        assert_eq!(state.nameplate.serial.as_deref(), Some("test-serial"));
        assert_eq!(state.nameplate.expiry, Some(1_800_000_000));
        socket
            .send(r#"{"d":"nameplate","id":"self","payload":{"expiry":null}}"#.into())
            .await
            .unwrap();
        wait_for(&controller, |state| state.nameplate.expiry.is_none()).await;

        socket.send(r#"{"d":"rgbled","id":"0","payload":{"colors":[[0.1,0.2,0.3],[0,0,0],[1,1,1],[0.4,0,0]]}}"#.into()).await.unwrap();
        let state = wait_for(&controller, |state| state.colors[0][0] == 0.1).await;
        assert!(state.phase == Phase::Booting);
        socket
            .send(r#"{"d":"switch","id":"22","payload":{"level":"high"}}"#.into())
            .await
            .unwrap();
        let state = wait_for(&controller, |state| state.phase == Phase::Firmware).await;
        assert_eq!(state.colors[0], [0.1, 0.2, 0.3]);
        socket
            .send(r#"{"d":"switch","id":"22","payload":{"level":"low"}}"#.into())
            .await
            .unwrap();
        wait_for(&controller, |state| state.phase == Phase::Booting).await;
        controller.stop();
    }

    /// Bad frames leave state intact and do not prevent required replies.
    #[tokio::test]
    async fn test_invalid_and_unknown_frames_do_not_corrupt_state() {
        let (controller, _listener, mut socket) = connect().await;
        for frame in [
            "not json",
            r#"{"d":"unknown","id":"0","payload":{"future":true}}"#,
            r#"{"d":"nameplate","id":"self","payload":{"env":"develop","expiry":"invalid"}}"#,
            r#"{"d":"rgbled","id":"0","payload":{"colors":[[1,0,0]]}}"#,
            r#"{"d":"switch","id":"99","payload":{"level":"high"}}"#,
        ] {
            socket.send(frame.into()).await.unwrap();
        }
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .await
            .unwrap();
        read(&mut socket).await;
        let state = controller.snapshot();
        assert!(!state.nameplate.known);
        assert!(state.nameplate.env.is_none());
        assert!(state.phase == Phase::Booting);
        assert_eq!(state.colors, [[0.0; 3]; 4]);
        controller.stop();
    }

    /// Button edges are ordered, deduplicated and cleared on a guest restart.
    #[tokio::test]
    async fn test_button_edges_and_reconnect_reset_transient_state() {
        let (controller, listener, mut socket) = connect().await;
        let generation = controller.snapshot().generation;
        controller.button(true, generation).await.unwrap();
        assert_eq!(
            read(&mut socket).await,
            json!({
                "d": "button", "id": "5", "payload": {"edge": "falling"}
            })
        );
        controller.button(true, generation).await.unwrap();
        controller.button(false, generation).await.unwrap();
        assert_eq!(
            read(&mut socket).await,
            json!({
                "d": "button", "id": "5", "payload": {"edge": "rising"}
            })
        );
        controller.button(true, generation).await.unwrap();
        read(&mut socket).await;
        socket
            .send(r#"{"d":"nameplate","id":"self","payload":{"name":"before restart"}}"#.into())
            .await
            .unwrap();
        wait_for(&controller, |state| state.nameplate.known).await;
        socket.close(None).await.unwrap();
        drop(socket);
        let state = wait_for(&controller, |state| !state.connected).await;
        assert!(!state.pressed);
        assert!(!state.nameplate.known);
        assert!(state.phase == Phase::Booting);
        assert!(controller.button(true, generation).await.is_err());
        let mut socket = accept(&listener).await;
        wait_for(&controller, |state| state.connected).await;
        assert!(controller.button(true, generation).await.is_err());
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .await
            .unwrap();
        assert_eq!(read(&mut socket).await["d"], "revbits");
        controller.stop();
        assert!(controller.snapshot().phase == Phase::Stopped);
        controller
            .frame(r#"{"d":"switch","id":"22","payload":{"level":"high"}}"#)
            .unwrap();
        assert!(controller.snapshot().phase == Phase::Stopped);
    }

    /// A pending handshake survives boot delays on QEMU's forwarded port.
    #[tokio::test]
    async fn test_a_pending_boot_connection_is_not_replaced() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let controller = Controller::default();
        controller.start(
            &tokio::runtime::Handle::current(),
            listener.local_addr().unwrap(),
        );
        let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_millis(1200), listener.accept())
                .await
                .is_err()
        );
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(r#"{"d":"revbits","id":"i2c@0x20","payload":{"op":"read"}}"#.into())
            .await
            .unwrap();
        assert_eq!(read(&mut socket).await["payload"]["revision"], 11);
        controller.stop();
    }

    /// Oversized messages terminate the connection before any state is applied.
    #[tokio::test]
    async fn test_oversized_hardware_frames_disconnect() {
        let (controller, _listener, mut socket) = connect().await;
        socket.send("x".repeat(65 * 1024).into()).await.unwrap();
        let state = wait_for(&controller, |state| !state.connected).await;
        assert!(!state.nameplate.known);
        controller.stop();
    }
}
