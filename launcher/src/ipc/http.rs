// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounded HTTP exchanges over native IPC, leaving retries to each caller.

use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::time::Instant;

use super::local::Stream;

/// A complete response whose status is interpreted by the calling operation.
#[derive(Debug)]
pub(super) struct Response {
    /// HTTP status returned by the peer.
    pub(super) status: u16,
    /// Complete, unencoded response body.
    pub(super) body: Vec<u8>,
}

/// Exchange one request within a shared deadline and total response size limit.
pub(super) fn exchange(
    mut stream: Stream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    deadline: Instant,
    max_response: u64,
) -> io::Result<Response> {
    // Request components come from fixed routes and validated caller metadata
    let mut head = format!(
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.map_or(0, <[u8]>::len)
    );
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        write!(head, "{name}: {value}\r\n").unwrap();
    }
    head.push_str("\r\n");

    // Partial writes and reads consume one budget, including connection setup
    let remaining = deadline.saturating_duration_since(Instant::now());
    stream.set_write_timeout(remaining);
    stream.set_read_timeout(remaining);
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body)?;
    }
    let mut raw = Vec::new();
    stream.take(max_response + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > max_response {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local HTTP response exceeded its size limit",
        ));
    }
    parse_response(&raw)
}

/// Decode response framing while distinguishing interrupted and malformed replies.
pub(super) fn parse_response(raw: &[u8]) -> io::Result<Response> {
    // Distinguish a peer exiting during its reply from a non-HTTP response
    let split = match raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
        Some(split) => split + 4,
        None if raw.starts_with(b"HTTP/1.") || b"HTTP/1.".starts_with(raw) => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer closed during its HTTP response headers",
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid local HTTP response headers",
            ));
        }
    };
    // Parse the complete header block before interpreting its body framing
    let mut fields = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut fields);
    if !matches!(
        response.parse(&raw[..split]),
        Ok(httparse::Status::Complete(_))
    ) || !response.code.is_some_and(|code| (100..600).contains(&code))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local HTTP response headers",
        ));
    }

    // One unencoded body ends at EOF and must match any declared length
    let mut length = None;
    for header in response.headers.iter() {
        if header.name.eq_ignore_ascii_case("Transfer-Encoding") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded local HTTP response",
            ));
        }
        if header.name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate local HTTP response length",
                ));
            }
            length = Some(
                std::str::from_utf8(header.value)
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid local HTTP response length",
                        )
                    })?,
            );
        }
    }
    let body = &raw[split..];
    if let Some(length) = length {
        if body.len() < length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer closed during its HTTP response body",
            ));
        }
        if body.len() != length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incorrect local HTTP response length",
            ));
        }
    }
    Ok(Response {
        status: response.code.unwrap(),
        body: body.to_vec(),
    })
}

/// Response framing and exchanges over native connections.
#[cfg(test)]
mod tests {
    use super::super::local::{self, Server};
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Complete replies preserve their status and body for the calling operation.
    #[test]
    fn test_complete_responses() {
        for (raw, status, body) in [
            ("HTTP/1.0 204 No Content\r\n\r\n", 204, ""),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}",
                200,
                "{\"a\":1}",
            ),
            (
                "HTTP/1.0 400 Bad Request\r\n\r\ninvalid input",
                400,
                "invalid input",
            ),
        ] {
            let response = parse_response(raw.as_bytes()).unwrap();
            assert_eq!(response.status, status);
            assert_eq!(response.body, body.as_bytes());
        }
    }

    /// Connection loss is recognizable at every boundary of the headers and body.
    #[test]
    fn test_truncated_responses() {
        let reply = b"HTTP/1.0 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        for end in 0..reply.len() {
            let err = parse_response(&reply[..end]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{end}");
        }
    }

    /// Malformed or ambiguous framing cannot be mistaken for connection loss.
    #[test]
    fn test_invalid_responses() {
        for reply in [
            "garbage",
            "HTTP?",
            "HTTP/1.0 invalid\r\n\r\n",
            "HTTP/1.0 999 Unknown\r\n\r\n",
            "HTTP/1.0 200 OK\r\nContent-Length: invalid\r\n\r\n",
            "HTTP/1.0 200 OK\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\nextra",
            "HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ] {
            let err = parse_response(reply.as_bytes()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{reply}");
        }
    }

    /// Native exchanges preserve request metadata and enforce the total reply limit.
    #[test]
    fn test_exchange_framing_and_response_limit() {
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let body = "{\"disk\":\"démo.ark\"}".as_bytes();
        for limit in [response.len() as u64, response.len() as u64 - 1] {
            // Inspect bytes received by the peer before returning a fixed response
            let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
            let stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
            let worker = thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), "POST");
                assert_eq!(request.url(), "/v1/instances");
                assert_eq!(request.body(), body);
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("Content-Type")
                            && header.value.as_str() == "application/json")
                );
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("X-Ark-Generation")
                            && header.value.as_str() == "42")
                );
                request.into_writer().write_all(response).unwrap();
            });
            // Accept a reply exactly at the limit and reject one byte beyond it
            let reply = exchange(
                stream,
                "POST",
                "/v1/instances",
                &[("X-Ark-Generation", "42")],
                Some(body),
                Instant::now() + Duration::from_secs(2),
                limit,
            );
            worker.join().unwrap();
            if limit == response.len() as u64 {
                assert_eq!(reply.unwrap().body, b"{}");
            } else {
                assert_eq!(reply.unwrap_err().kind(), io::ErrorKind::InvalidData);
            }
        }
    }
}
