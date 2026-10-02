// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Isolated registry peers for HTTP failures and lifecycle scenarios.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::discovery::Client;
use super::{local, registry as registry_api};

/// Reserve an isolated registry namespace without using the shared discovery port.
pub(crate) fn server() -> (Client, local::Server, TcpListener) {
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = SocketAddrV4::new(
        Ipv4Addr::LOCALHOST,
        reservation.local_addr().unwrap().port(),
    );
    let server = local::Server::bind_test(&registry_api::local_name(address.port())).unwrap();
    (Client { address }, server, reservation)
}

/// Serve scripted replies, closing without a response when a reply is empty.
pub(crate) fn registry(replies: Vec<(&'static str, String)>) -> (Client, JoinHandle<()>) {
    let (client, server, reservation) = server();
    let worker = thread::spawn(move || {
        for (method, reply) in replies {
            let incoming = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(incoming.method().as_str(), method);
            let mut writer = incoming.into_writer();
            writer.write_all(reply.as_bytes()).unwrap();
        }
        drop(server);
        drop(reservation);
    });
    (client, worker)
}

/// Encode an HTTP reply with an explicit body length for the registry peer.
pub(crate) fn response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.0 {status} Test\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
