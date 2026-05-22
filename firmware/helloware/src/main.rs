// helloware: a stand-in for arkos-core. Runs as PID 1 in a minimal Alpine
// initramfs booted by QEMU. Hosts two WebSocket endpoints:
//
//   /ws  -- the "dashboard" channel; in production this is what the host
//           dashboard talks to. Here it echoes text, cased according to the
//           current hardware state.
//   /hw  -- the "hardware" channel; in production this carries LED state out
//           and button events in. Here it has a single toggle: case=upper or
//           case=lower, controlling how /ws echoes text.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};

const BANNER: &str = "helloware v0.1 \u{00b7} ready";
const LISTEN: &str = "0.0.0.0:8080";

#[derive(Clone)]
struct HwState {
    // true = uppercase (default), false = lowercase. The dashboard handler
    // reads this every time it cases a reply; the hardware handler writes it.
    case_upper: Arc<AtomicBool>,
}

#[tokio::main]
async fn main() {
    println!("[helloware] {BANNER}");

    let state = HwState {
        case_upper: Arc::new(AtomicBool::new(true)),
    };

    let app = Router::new()
        .route("/ws", get(dashboard_ws))
        .route("/hw", get(hardware_ws))
        .with_state(state);

    let addr: SocketAddr = LISTEN.parse().expect("static literal parses");

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            println!("[helloware] WS server listening on {addr}");
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("[helloware] serve error: {e}");
            }
        }
        Err(e) => {
            eprintln!("[helloware] bind {addr} failed: {e}");
        }
    }

    eprintln!("[helloware] idling (server exited); kill the guest to stop");
    park().await;
}

async fn dashboard_ws(
    ws: WebSocketUpgrade,
    State(state): State<HwState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_dashboard(socket, state))
}

async fn handle_dashboard(socket: WebSocket, state: HwState) {
    let (mut sender, mut receiver) = socket.split();

    if sender.send(Message::Text(BANNER.to_string())).await.is_err() {
        return;
    }

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                let upper = state.case_upper.load(Ordering::Relaxed);
                let cased = if upper {
                    t.trim().to_uppercase()
                } else {
                    t.trim().to_lowercase()
                };
                let response = format!("HELLO from helloware: {cased}");
                println!(
                    "[helloware] rx[{}]: {t:?} -> {response:?}",
                    if upper { "UPPER" } else { "lower" }
                );
                if sender.send(Message::Text(response)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn hardware_ws(
    ws: WebSocketUpgrade,
    State(state): State<HwState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_hardware(socket, state))
}

async fn handle_hardware(socket: WebSocket, state: HwState) {
    let (mut sender, mut receiver) = socket.split();

    // Sync the UI's switch to the firmware's current state on connect.
    let init = case_label(state.case_upper.load(Ordering::Relaxed));
    if sender.send(Message::Text(init.to_string())).await.is_err() {
        return;
    }

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => match t.trim() {
                "upper" => {
                    state.case_upper.store(true, Ordering::Relaxed);
                    println!("[helloware] hw: case=upper");
                }
                "lower" => {
                    state.case_upper.store(false, Ordering::Relaxed);
                    println!("[helloware] hw: case=lower");
                }
                other => {
                    println!("[helloware] hw: ignoring unknown {other:?}");
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
}

fn case_label(upper: bool) -> &'static str {
    if upper { "upper" } else { "lower" }
}

async fn park() -> ! {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
