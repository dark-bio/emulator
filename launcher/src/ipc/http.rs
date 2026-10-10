// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounded HTTP/1.1 exchanges over local sockets, leaving retries to each caller.

use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::time::Instant;

use ureq_proto::BodyMode;
use ureq_proto::client::{Call, RecvResponseResult, SendRequestResult, state::RecvResponse};
use ureq_proto::http::{HeaderMap, Request, Version, header};
use ureq_proto::server::{RecvRequestResult, Reply};

use super::local::Stream;

/// Maximum header bytes, including informational responses, and pending body bytes.
const MAX_HEADER: usize = 8192;

/// A message exceeding the endpoint's header, body or framing budget.
#[derive(Debug)]
pub(super) struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("local HTTP message too large")
    }
}

impl std::error::Error for TooLarge {}

/// Report a malformed HTTP message without disguising a socket failure.
fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Read one bounded header block without consuming the following body.
fn read_head(reader: &mut impl io::BufRead, limit: usize) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    loop {
        let count = reader
            .take((limit + 1 - head.len()) as u64)
            .read_until(b'\n', &mut head)?;
        if head.len() > limit {
            return Err(invalid(TooLarge));
        }
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
    }
}

/// Reject ambiguous lengths and transfer codings the endpoints cannot decode.
fn validate_framing(headers: &HeaderMap, version: Version) -> io::Result<()> {
    let mut encodings = headers.get_all(header::TRANSFER_ENCODING).iter();
    if let Some(encoding) = encodings.next()
        && (version != Version::HTTP_11
            || !encoding.as_bytes().eq_ignore_ascii_case(b"chunked")
            || encodings.next().is_some()
            || headers.contains_key(header::CONTENT_LENGTH))
    {
        return Err(invalid("unsupported or ambiguous HTTP body framing"));
    }
    Ok(())
}

/// Decode a body with bounded allocation and work, preserving premature EOF.
fn read_body(
    reader: &mut impl Read,
    mode: BodyMode,
    limit: usize,
    mut decode: impl FnMut(&[u8], &mut [u8]) -> io::Result<(usize, usize, bool)>,
) -> io::Result<Vec<u8>> {
    if matches!(mode, BodyMode::LengthDelimited(length) if length > limit as u64) {
        return Err(invalid(TooLarge));
    }
    // Allow small chunks while bounding extensions, trailers and other framing work
    let wire_limit = limit.saturating_mul(8).saturating_add(MAX_HEADER);
    let mut received = 0;
    let mut pending = Vec::new();
    let mut body = Vec::new();
    let mut buffer = [0; MAX_HEADER];
    loop {
        let (used, count, ended) = decode(&pending, &mut buffer)?;
        if body.len() + count > limit {
            return Err(invalid(TooLarge));
        }
        body.extend_from_slice(&buffer[..count]);
        pending.drain(..used);
        if ended && mode != BodyMode::CloseDelimited {
            return Ok(body);
        }
        if used > 0 {
            continue;
        }
        if pending.len() == buffer.len() || received == wire_limit {
            return Err(invalid(TooLarge));
        }
        let available = (buffer.len() - pending.len()).min(wire_limit - received);
        let count = reader.read(&mut buffer[..available])?;
        if count == 0 {
            return if mode == BodyMode::CloseDelimited {
                Ok(body)
            } else {
                Err(io::Error::from(io::ErrorKind::UnexpectedEof))
            };
        }
        received += count;
        pending.extend_from_slice(&buffer[..count]);
    }
}

/// Read one request, acknowledging 100-continue only after checking its limits.
/// A zero body limit rejects transfer encoding before reading any chunk framing.
pub(super) fn read_request(stream: &mut Stream, limit: usize) -> io::Result<Request<Vec<u8>>> {
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader, MAX_HEADER)?;
    let mut reply = Reply::new().map_err(invalid)?;
    // A body on any method must reach the handler's body validation
    reply.force_recv_body();
    let (_, request) = reply.try_request(&head).map_err(invalid)?;
    let request = request.ok_or_else(|| invalid("invalid local HTTP request"))?;
    validate_framing(request.headers(), request.version())?;
    if request.version() == Version::HTTP_11 {
        let mut hosts = request.headers().get_all(header::HOST).iter();
        if hosts.next().is_none_or(|host| host.is_empty()) || hosts.next().is_some() {
            return Err(invalid("HTTP/1.1 requires one Host header"));
        }
    }
    if let Some(length) = request.headers().get(header::CONTENT_LENGTH)
        && length
            .to_str()
            .ok()
            .and_then(|s| s.split(',').next()?.trim().parse::<u64>().ok())
            .is_some_and(|n| n > limit as u64)
    {
        return Err(invalid(TooLarge));
    }
    if limit == 0 && request.headers().contains_key(header::TRANSFER_ENCODING) {
        return Err(invalid(TooLarge));
    }
    if request
        .headers()
        .get_all(header::EXPECT)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"100-continue"))
    {
        return Err(invalid("unsupported HTTP expectation"));
    }
    // Unframed requests have no body, including POST; ureq-proto assumes chunking for POST
    if !request.headers().contains_key(header::CONTENT_LENGTH)
        && !request.headers().contains_key(header::TRANSFER_ENCODING)
    {
        return Ok(request.map(|()| Vec::new()));
    }
    let mut reply = match reply.proceed().unwrap() {
        RecvRequestResult::ProvideResponse(_) => return Ok(request.map(|()| Vec::new())),
        RecvRequestResult::RecvBody(reply) => reply,
        RecvRequestResult::Send100(reply) => {
            let mut output = [0; 64];
            let (count, reply) = reply.accept(&mut output).map_err(invalid)?;
            reader.get_mut().write_all(&output[..count])?;
            reply
        }
    };
    let body = read_body(&mut reader, reply.body_mode(), limit, |input, output| {
        let (used, count) = reply.read(input, output).map_err(invalid)?;
        Ok((used, count, reply.is_ended()))
    })?;
    Ok(request.map(|()| body))
}

/// A complete response whose status is interpreted by the calling operation.
#[derive(Debug)]
pub(super) struct Response {
    /// HTTP status returned by the peer.
    pub(super) status: u16,
    /// Complete response body after transfer decoding.
    pub(super) body: Vec<u8>,
}

/// Write a request with a known body length, without automatic retries or redirects.
fn send_request(
    mut stream: impl Write,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<Call<RecvResponse>> {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("http://localhost{path}"))
        .header(header::CONNECTION, "close")
        .header(header::CONTENT_LENGTH, body.len());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if !body.is_empty() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let mut call = Call::new(request.body(()).map_err(invalid)?)
        .map_err(invalid)?
        .proceed();
    let mut output = [0; MAX_HEADER];
    while !call.can_proceed() {
        let count = call.write(&mut output).map_err(invalid)?;
        stream.write_all(&output[..count])?;
    }
    match call.proceed().map_err(invalid)?.unwrap() {
        SendRequestResult::RecvResponse(call) => Ok(call),
        SendRequestResult::SendBody(mut call) => {
            stream.write_all(body)?;
            call.consume_direct_write(body.len()).map_err(invalid)?;
            Ok(call.proceed().unwrap())
        }
        SendRequestResult::Await100(_) => Err(invalid("unexpected local HTTP expectation")),
    }
}

/// Read a final response, bounding informational headers and decoded body bytes.
fn read_response(
    stream: impl Read,
    mut call: Call<RecvResponse>,
    limit: usize,
) -> io::Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut remaining = MAX_HEADER;
    let response = loop {
        let head = read_head(&mut reader, remaining)?;
        remaining -= head.len();
        let (_, response) = call.try_response(&head, false).map_err(invalid)?;
        if let Some(response) = response {
            validate_framing(response.headers(), response.version())?;
            if !(200..600).contains(&response.status().as_u16()) {
                return Err(invalid("invalid local HTTP response status"));
            }
            break response;
        }
    };
    let body = match call.proceed().unwrap() {
        RecvResponseResult::RecvBody(mut call) => {
            read_body(&mut reader, call.body_mode(), limit, |input, output| {
                let (used, count) = call.read(input, output).map_err(invalid)?;
                Ok((used, count, call.can_proceed()))
            })?
        }
        _ => Vec::new(),
    };
    Ok(Response {
        status: response.status().as_u16(),
        body,
    })
}

/// Exchange one request within a shared deadline and response body size limit.
pub(super) fn exchange(
    mut stream: Stream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    deadline: Instant,
    max_response: u64,
) -> io::Result<Response> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    stream.set_write_timeout(remaining);
    stream.set_read_timeout(remaining);
    let call = send_request(&mut stream, method, path, headers, body.unwrap_or_default())?;
    read_response(stream, call, max_response as usize)
}

/// HTTP interoperability, message boundaries and resource limits.
#[cfg(test)]
mod tests {
    use super::super::local::{self, Server};
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Read a scripted response as the reply to a bodyless GET.
    fn response(raw: &[u8], limit: usize) -> io::Result<Response> {
        let call = send_request(io::sink(), "GET", "/v1/instances", &[], &[])?;
        read_response(raw, call, limit)
    }

    /// Standard length, chunked and close-delimited responses preserve their bytes.
    #[test]
    fn test_response_framing() {
        for wire in [
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\na\0b",
            "HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\na\0b",
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\na\0b",
            "HTTP/1.0 200 OK\r\n\r\na\0b",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;x=y\r\na\r\n2\r\n\0b\r\n0\r\nX-Test: done\r\n\r\n",
            "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\na\0b",
        ] {
            let reply = response(wire.as_bytes(), 3).unwrap();
            assert_eq!(reply.status, 200);
            assert_eq!(reply.body, b"a\0b");
            assert_eq!(
                response(wire.as_bytes(), 2).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(
            response(b"HTTP/1.1 204 No Content\r\n\r\n", 0)
                .unwrap()
                .body
                .is_empty()
        );
    }

    /// Every truncated length or chunked message remains an uncertain response.
    #[test]
    fn test_truncated_responses() {
        for wire in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\na\0b".as_slice(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\na\0b\r\n0\r\n\r\n",
        ] {
            for end in 0..wire.len() {
                assert_eq!(
                    response(&wire[..end], 3).unwrap_err().kind(),
                    io::ErrorKind::UnexpectedEof,
                    "{end}"
                );
            }
        }
    }

    /// Malformed, ambiguous and oversized framing fails before use of the body.
    #[test]
    fn test_invalid_responses() {
        for head in [
            "Content-Length: invalid\r\n".to_owned(),
            "Content-Length: 1\r\nContent-Length: 2\r\n".to_owned(),
            "Content-Length: 0\r\nTransfer-Encoding: chunked\r\n".to_owned(),
            "Transfer-Encoding: gzip\r\n".to_owned(),
            "Transfer-Encoding: chunked, gzip\r\n".to_owned(),
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n".to_owned(),
            format!("X-Padding: {}\r\n", "x".repeat(8192)),
        ] {
            let wire = format!("HTTP/1.1 200 OK\r\n{head}\r\n");
            assert_eq!(
                response(wire.as_bytes(), 32).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{head}"
            );
        }
        let wire = "HTTP/1.1 100 Continue\r\n\r\n".repeat(400);
        assert_eq!(
            response(wire.as_bytes(), 32).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let wire = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{}",
            "f".repeat(8193)
        );
        assert_eq!(
            response(wire.as_bytes(), 32).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    /// A length-delimited response completes while its socket remains open.
    #[test]
    fn test_completion_before_close() {
        let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
        let mut stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
        let call = send_request(
            &mut stream,
            "POST",
            "/v1/instances",
            &[("X-Ark-Generation", "42")],
            "{\"disk\":\"démo.ark\"}".as_bytes(),
        )
        .unwrap();
        let request = server.recv().unwrap();
        assert_eq!(request.method().as_str(), "POST");
        assert_eq!(request.body(), "{\"disk\":\"démo.ark\"}".as_bytes());
        assert!(
            request
                .headers()
                .iter()
                .any(|header| header.field.equiv("X-Ark-Generation")
                    && header.value.as_str() == "42")
        );
        let mut peer = request.into_writer();
        peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .unwrap();
        assert_eq!(read_response(stream, call, 2).unwrap().body, b"{}");
        drop(peer);
    }

    /// Native exchanges preserve JSON metadata and enforce the response body limit.
    #[test]
    fn test_native_exchange() {
        for limit in [1, 2] {
            let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
            let stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
            let worker = thread::spawn(move || {
                let request = server.recv().unwrap();
                assert_eq!(request.method().as_str(), "POST");
                assert_eq!(request.url(), "/v1/instances");
                assert_eq!(request.body(), "{\"disk\":\"démo.ark\"}".as_bytes());
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("Content-Type")
                            && header.value.as_str() == "application/json")
                );
                request
                    .respond(tiny_http::Response::from_string("{}"))
                    .unwrap();
            });
            let result = exchange(
                stream,
                "POST",
                "/v1/instances",
                &[],
                Some("{\"disk\":\"démo.ark\"}".as_bytes()),
                Instant::now() + Duration::from_secs(1),
                limit,
            );
            worker.join().unwrap();
            if limit == 2 {
                assert_eq!(result.unwrap().body, b"{}");
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            }
        }
    }

    /// A request of the largest size and a 16 KiB reply arrive whole, past the
    /// 8 KiB a macOS unix socket buffers.
    #[test]
    fn test_large_exchange_preserves_request_and_response() {
        let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
        let stream = Stream::connect(server.name(), Duration::from_secs(2)).unwrap();
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.body(), vec![b'q'; 8192]);
            request
                .respond(tiny_http::Response::from_data(vec![b'r'; 16 * 1024]))
                .unwrap();
        });

        // Exchange while the server answers, as a real peer does
        let reply = exchange(
            stream,
            "POST",
            "/test",
            &[],
            Some(&[b'q'; 8192]),
            Instant::now() + Duration::from_secs(2),
            1024 * 1024,
        );
        worker.join().unwrap();
        let reply = reply.unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, vec![b'r'; 16 * 1024]);
    }

    /// Request framing accepts curl-style requests and bodies on any method.
    #[test]
    fn test_request_framing() {
        for wire in [
            "POST /v1/instances HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
            "GET /v1/instances HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
            "POST /v1/instances HTTP/1.0\r\nContent-Length: 2\r\n\r\n{}",
            "POST /v1/instances HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n1\r\n}\r\n0\r\n\r\n",
        ] {
            let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
            let mut stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
            stream.write_all(wire.as_bytes()).unwrap();
            assert_eq!(server.recv().unwrap().body(), b"{}");
        }
    }

    /// A client can await 100-continue before sending its request body.
    #[test]
    fn test_expect_continue() {
        let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
        let mut stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.body(), b"{}");
            request.respond(tiny_http::Response::empty(204)).unwrap();
        });
        stream.write_all(b"POST /v1/instances HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n").unwrap();
        let mut reader = BufReader::new(stream);
        assert_eq!(
            read_head(&mut reader, 8192).unwrap(),
            b"HTTP/1.1 100 Continue\r\n\r\n"
        );
        reader.get_mut().write_all(b"{}").unwrap();
        assert!(
            read_head(&mut reader, 8192)
                .unwrap()
                .starts_with(b"HTTP/1.1 204 ")
        );
        worker.join().unwrap();
    }

    /// Limits and ambiguous framing reject requests before application dispatch.
    #[test]
    fn test_request_rejections() {
        for (headers, body, status) in [
            ("Content-Length: 8193\r\nExpect: 100-continue\r\n", "", 413),
            (
                "Content-Length: 8193, 8193\r\nExpect: 100-continue\r\n",
                "",
                413,
            ),
            (
                "Content-Length: 0\r\nTransfer-Encoding: chunked\r\n",
                "",
                400,
            ),
            ("Content-Length: 1\r\nContent-Length: 2\r\n", "", 400),
            ("Transfer-Encoding: gzip\r\n", "", 400),
            ("Transfer-Encoding: chunked\r\n", "wat\r\n", 400),
            ("Host: duplicate\r\n", "", 400),
        ] {
            let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
            let mut stream = Stream::connect(server.name(), Duration::from_secs(1)).unwrap();
            write!(
                stream,
                "POST /v1/instances HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n{body}"
            )
            .unwrap();
            assert_eq!(
                server.recv().err().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
            let mut reader = BufReader::new(stream);
            let head = read_head(&mut reader, 8192).unwrap();
            assert!(
                head.starts_with(format!("HTTP/1.1 {status} ").as_bytes()),
                "{headers}"
            );
        }
    }

    /// A standard curl client reads and publishes JSON over the private socket.
    #[test]
    #[ignore = "requires curl with Unix socket support"]
    fn test_curl_unix_socket() {
        let server = Server::bind(&format!("t-{}", local::identity())).unwrap();
        let path = local::address(server.name()).unwrap();
        let worker = thread::spawn(move || {
            for method in ["GET", "POST", "POST"] {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), method);
                assert_eq!(request.url(), "/v1/instances");
                if method == "POST" {
                    assert_eq!(request.body(), b"{\"disk\":\"demo.ark\"}");
                }
                request
                    .respond(
                        tiny_http::Response::from_string("{\"version\":1,\"instances\":[]}")
                            .with_header(
                                tiny_http::Header::from_bytes("Content-Type", "application/json")
                                    .unwrap(),
                            ),
                    )
                    .unwrap();
            }
        });
        for extra in [
            vec![],
            vec![
                "--data-binary",
                "{\"disk\":\"demo.ark\"}",
                "-H",
                "Content-Type: application/json",
            ],
            vec![
                "--data-binary",
                "{\"disk\":\"demo.ark\"}",
                "-H",
                "Transfer-Encoding: chunked",
                "-H",
                "Expect: 100-continue",
            ],
        ] {
            let output = std::process::Command::new("curl")
                .args([
                    "--silent",
                    "--show-error",
                    "--fail",
                    "--max-time",
                    "5",
                    "--unix-socket",
                ])
                .arg(&path)
                .args(extra)
                .arg("http://localhost/v1/instances")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"{\"version\":1,\"instances\":[]}");
        }
        worker.join().unwrap();
    }
}
