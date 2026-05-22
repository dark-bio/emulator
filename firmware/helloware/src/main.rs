// helloware: a stand-in for arkos-core. Runs as PID 1 in a minimal Alpine
// initramfs booted by QEMU. Hosts a WebSocket server directly -- in
// production the real firmware will do the same (a feature-flagged transport
// that substitutes WebSocket for WebUSB inside the emulator build).
//
// QEMU's SLIRP hostfwd maps the host's TCP port to :8080 inside the guest,
// so the launcher and clients see a normal WebSocket endpoint with no
// translation hops.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};

const BANNER: &str = "helloware v0.1 \u{00b7} ready";
const LISTEN: &str = "0.0.0.0:8080";

#[tokio::main]
async fn main() {
    println!("[helloware] {BANNER}");

    let app = Router::new().route("/ws", get(ws_handler));
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

    // PID 1 must never return -- otherwise the kernel panics. Idle forever.
    eprintln!("[helloware] idling (server exited); kill the guest to stop");
    park().await;
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_ws)
}

async fn handle_ws(socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();

    if sender.send(Message::Text(BANNER.to_string())).await.is_err() {
        return;
    }

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                let response = format!("HELLO from helloware: {}", t.trim().to_uppercase());
                println!("[helloware] rx: {t:?} -> {response:?}");
                if sender.send(Message::Text(response)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn park() -> ! {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
