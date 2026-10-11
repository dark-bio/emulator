// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Guest hardware port lifecycle notifications over QEMU's private monitor.

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::time::Duration;

use serde_json::Value;

use super::local::Stream;

/// Maximum size of one QMP greeting, response or event.
const MAX_MESSAGE: usize = 64 * 1024;

/// A QMP connection retaining incomplete JSON lines across idle polls.
pub(crate) struct Monitor {
    /// Native transport with bounded reads and writes.
    stream: BufReader<Stream>,
    /// Partial newline-delimited QMP message.
    pending: Vec<u8>,
    /// Whether QEMU has accepted capability negotiation.
    enabled: bool,
    /// Latest state request, excluding older responses from a previous greeting.
    query: u64,
    /// Whether the latest state query confirmed an open guest hardware port.
    pub(crate) open: bool,
}

impl Monitor {
    /// Prepare a connection for capability negotiation and port state queries.
    pub(crate) fn new(stream: Stream) -> Self {
        Self {
            stream: BufReader::new(stream),
            pending: Vec::new(),
            enabled: false,
            query: 0,
            open: false,
        }
    }

    /// Return the next hardware port state, or none when the monitor is idle.
    pub(crate) fn poll(&mut self) -> io::Result<Option<bool>> {
        // QMP uses JSON lines, with its framing defined by QEMU
        self.stream
            .get_mut()
            .set_read_timeout(Duration::from_millis(1));
        loop {
            match self
                .stream
                .by_ref()
                .take((MAX_MESSAGE + 1 - self.pending.len()) as u64)
                .read_until(b'\n', &mut self.pending)
            {
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None);
                }
                Err(err) => return Err(err),
                Ok(_) => {}
            }
            if self.pending.len() > MAX_MESSAGE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "QMP message too large",
                ));
            }
            if self.pending.last() != Some(&b'\n') {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let message: Value = serde_json::from_slice(&self.pending)?;
            self.pending.clear();

            // Subscribe before querying, so a concurrent port change is not lost
            if message.get("QMP").is_some() {
                self.send(br#"{"execute":"qmp_capabilities","id":"capabilities"}"#)?;
            } else if message.get("error").is_some() {
                return Err(io::Error::other("QEMU rejected hardware port monitoring"));
            } else if message["id"] == "capabilities" {
                self.enabled = true;
                self.refresh()?;
            } else if self.query != 0 && message["id"].as_u64() == Some(self.query) {
                let open = message["return"]
                    .as_array()
                    .and_then(|ports| ports.iter().find(|port| port["label"] == "hw"))
                    .and_then(|port| port["frontend-open"].as_bool())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "QMP omitted hardware port state",
                        )
                    })?;
                self.open = open;
                return Ok(Some(open));
            } else if message["event"] == "VSERPORT_CHANGE" && message["data"]["id"] == "hw" {
                // Events can arrive after a new hardware greeting on the other
                // stream. Query current state instead of applying an old close.
                self.refresh()?;
            }
        }
    }

    /// Require a current port snapshot before acknowledging another hardware greeting.
    pub(crate) fn refresh(&mut self) -> io::Result<()> {
        self.open = false;
        if self.enabled {
            self.query += 1;
            self.send(format!(r#"{{"execute":"query-chardev","id":{}}}"#, self.query).as_bytes())?;
        }
        Ok(())
    }

    /// Write one complete QMP command under a single deadline.
    fn send(&mut self, command: &[u8]) -> io::Result<()> {
        let stream = self.stream.get_mut();
        stream.set_write_timeout(Duration::from_secs(2));
        stream.write_all(command)?;
        stream.write_all(b"\n")
    }
}

/// Native monitor transcripts without a QEMU process.
#[cfg(test)]
mod tests {
    use super::super::local::{Server, identity};
    use super::*;

    /// Fragmented notifications preserve their bytes and ignore unrelated ports.
    #[test]
    fn test_monitor_preserves_partial_notifications() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let client = Stream::connect(&name, Duration::from_secs(1)).unwrap();
        let mut peer = server
            .accept_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let mut monitor = Monitor::new(client);

        // Initial state is queried after capability negotiation
        peer.write_all(
            b"{\"return\":{},\"id\":\"capabilities\"}\n{\"return\":[{\"label\":\"hw\",\"frontend-open\":true}],\"id\":1}\r\n",
        )
        .unwrap();
        assert_eq!(monitor.poll().unwrap(), Some(true));
        peer.write_all(
            b"{\"event\":\"VSERPORT_CHANGE\",\"data\":{\"id\":\"other\",\"open\":false}}\r\n",
        )
        .unwrap();
        assert_eq!(monitor.poll().unwrap(), None);
        assert!(monitor.open);

        // An idle poll must not erase a partial lifecycle notification
        peer.write_all(b"{\"event\":\"VSERPORT_CHANGE\",\"data\":")
            .unwrap();
        assert_eq!(monitor.poll().unwrap(), None);
        peer.write_all(b"{\"id\":\"hw\",\"open\":false}}\r\n")
            .unwrap();
        assert_eq!(monitor.poll().unwrap(), None);
        peer.write_all(b"{\"return\":[{\"label\":\"hw\",\"frontend-open\":false}],\"id\":2}\r\n")
            .unwrap();
        assert_eq!(monitor.poll().unwrap(), Some(false));
        assert!(!monitor.open);

        // A delayed response cannot validate a greeting from a newer session
        monitor.refresh().unwrap();
        peer.write_all(b"{\"return\":[{\"label\":\"hw\",\"frontend-open\":true}],\"id\":2}\r\n")
            .unwrap();
        assert_eq!(monitor.poll().unwrap(), None);
        assert!(!monitor.open);

        // Observe peer closure within a bounded interval across host schedulers
        drop(peer);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "monitor did not observe the peer disconnect"
            );
            match monitor.poll() {
                Ok(None) => {}
                result => {
                    assert!(matches!(
                        result.unwrap_err().kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ));
                    break;
                }
            }
        }
    }

    /// Missing state and oversized monitor messages fail instead of asserting readiness.
    #[test]
    fn test_monitor_rejects_invalid_state_and_oversized_messages() {
        for wire in [
            b"{\"return\":{},\"id\":\"capabilities\"}\n{\"return\":[],\"id\":1}\n".to_vec(),
            vec![b'x'; 65537],
        ] {
            let name = format!("t-{}", identity());
            let server = Server::bind(&name).unwrap();
            let client = Stream::connect(&name, Duration::from_secs(1)).unwrap();
            let mut peer = server
                .accept_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
            let sender = std::thread::spawn(move || {
                peer.write_all(&wire).unwrap();
                peer
            });
            let mut monitor = Monitor::new(client);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "invalid monitor input was not rejected"
                );
                match monitor.poll() {
                    Ok(None) => {}
                    result => {
                        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
                        break;
                    }
                }
            }
            sender.join().unwrap();
        }
    }
}
