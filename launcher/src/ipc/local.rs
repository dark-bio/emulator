// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounded HTTP exchanges over user-owned sockets and Windows named pipes.
//!
//! Only native processes can reach these listeners. Unix sockets live in a
//! private directory, and Windows pipes admit only the current user's SID.
//! Each connection carries one bounded request and one response. HTTP framing
//! preserves the control protocol across platforms.

use std::cell::Cell;
#[cfg(unix)]
use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::{
    io::{AsFd as _, AsRawFd as _},
    net::UnixStream,
};
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use interprocess::ConnectWaitMode;
#[cfg(unix)]
use interprocess::local_socket::ConnectOptions;
#[cfg(unix)]
use interprocess::local_socket::Stream as Socket;
use interprocess::local_socket::{
    GenericFilePath, Listener, ListenerNonblockingMode, ListenerOptions, prelude::*,
};
use sha2::{Digest as _, Sha256};
use tiny_http::{HTTPVersion, Header, Method, Response};

#[cfg(windows)]
#[path = "local_windows.rs"]
mod windows;
#[cfg(windows)]
use windows::{Stream as Socket, identity as windows_identity};

/// Poll interval for nonblocking pipes, which have no portable I/O timeout.
const POLL: Duration = Duration::from_millis(5);
/// Idle interval between nonblocking Windows accepts.
#[cfg(windows)]
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// Delay after a listener fails to accept a connection.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Maximum time to receive a request or deliver a response.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum request header and body sizes, each in bytes.
const MAX_REQUEST: usize = 8192;
/// Process-local component distinguishing simultaneous listener names.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Endpoints retained for cleanup when process exit skips their destructors.
#[cfg(unix)]
static SOCKETS: Mutex<Option<Vec<SocketFiles>>> = Mutex::new(None);

/// Excludes process spawning while a test releases and reacquires a file lock.
/// Forked children retain inherited locks until exec closes their descriptors.
#[cfg(all(test, unix))]
pub(crate) static PROCESS_TEST: Mutex<()> = Mutex::new(());

/// Files owned by a bound Unix endpoint.
#[cfg(unix)]
struct SocketFiles {
    /// Socket removed on listener drop or process exit.
    path: PathBuf,
    /// Whether a fixture created the lock while exclusively reserving its name.
    #[cfg(test)]
    remove_lock: bool,
}

#[cfg(unix)]
impl SocketFiles {
    /// Remove the socket and any lock owned exclusively by a fixture.
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
        #[cfg(test)]
        if self.remove_lock {
            let _ = std::fs::remove_file(self.path.with_extension("lock"));
        }
    }
}

/// Generate an opaque launch identity without claiming it is a credential.
pub(crate) fn identity() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:x}",
        Sha256::digest(format!(
            "{}:{stamp:?}:{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    )
}

/// Resolve a protocol name within the current user's local IPC namespace.
fn address(name: &str) -> io::Result<PathBuf> {
    if name.is_empty()
        || name.len() > 70
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid local endpoint name",
        ));
    }
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not retain pointers
        let uid = unsafe { libc::geteuid() };
        Ok(PathBuf::from(format!("/tmp/ark-emulator-{uid}")).join(name))
    }
    #[cfg(windows)]
    {
        Ok(PathBuf::from(format!(
            r"\\.\pipe\ark-emulator-{}-{name}",
            windows_identity()?.0
        )))
    }
}

/// A local connection with a single deadline for each read or write phase.
pub(crate) struct Stream {
    /// Nonblocking transport, including on Windows where timeouts are unavailable.
    socket: Socket,
    /// Deadline shared by partial reads in the current phase.
    read_deadline: Cell<Instant>,
    /// Deadline shared by partial writes in the current phase.
    write_deadline: Cell<Instant>,
}

impl Stream {
    /// Connect to a native endpoint without consulting proxies or DNS.
    pub(crate) fn connect(name: &str, timeout: Duration) -> io::Result<Self> {
        let path = address(name)?;
        #[cfg(unix)]
        verify_directory(path.parent().unwrap())?;
        #[cfg(unix)]
        let socket = ConnectOptions::new()
            .name(path.to_fs_name::<GenericFilePath>()?)
            .wait_mode(ConnectWaitMode::Timeout(timeout))
            .nonblocking_stream(true)
            .connect_sync()?;
        #[cfg(windows)]
        let socket = windows::connect(&path, timeout)?;
        Ok(Self::new(socket, timeout))
    }

    /// Wrap an accepted nonblocking connection with bounded I/O.
    fn new(socket: Socket, timeout: Duration) -> Self {
        let deadline = Instant::now() + timeout;
        Self {
            socket,
            read_deadline: Cell::new(deadline),
            write_deadline: Cell::new(deadline),
        }
    }

    /// Set the budget for the next response read phase.
    pub(crate) fn set_read_timeout(&self, timeout: Duration) {
        self.read_deadline.set(Instant::now() + timeout);
    }

    /// Set the budget for the next request write phase.
    pub(crate) fn set_write_timeout(&self, timeout: Duration) {
        self.write_deadline.set(Instant::now() + timeout);
    }
}

/// Retry readiness under a fixed deadline, never replaying an application request.
fn bounded<T>(deadline: Instant, mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "local IPC deadline expired"))?;
        match operation() {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL.min(remaining))
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        bounded(self.read_deadline.get(), || {
            #[cfg(unix)]
            {
                self.socket.read(bytes)
            }
            #[cfg(windows)]
            {
                windows::read(&mut self.socket, bytes)
            }
        })
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        bounded(self.write_deadline.get(), || {
            let result = self.socket.write(bytes);
            // A full nonblocking Windows pipe may accept no bytes without an error
            #[cfg(windows)]
            if matches!(result, Ok(0)) && !bytes.is_empty() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            result
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        // Writes are unbuffered; named-pipe flush would wait for peer consumption
        Ok(())
    }
}

/// One user-owned listener with exclusive name ownership and bounded accepts.
pub(crate) struct Server {
    /// Platform listener, inaccessible to browser networking APIs.
    listener: Listener,
    /// Endpoint name, never an arbitrary path supplied by discovery.
    #[cfg(any(unix, test))]
    name: String,
    /// Interrupts an accept loop during launcher shutdown.
    stopped: AtomicBool,
    /// Socket pair that interrupts the listener's readiness wait.
    #[cfg(unix)]
    wake: (UnixStream, UnixStream),
    /// Notification that interrupts the idle pipe wait.
    #[cfg(windows)]
    wake: (Mutex<()>, Condvar),
    /// Held across stale socket removal, binding and final socket cleanup.
    #[cfg(unix)]
    _lock: Option<File>,
}

impl Server {
    /// Bind a private endpoint, recovering a stale socket only under its lock.
    pub(crate) fn bind(name: &str) -> io::Result<Self> {
        let path = address(name)?;
        #[cfg(unix)]
        let lock = prepare(&path)?;
        #[cfg(unix)]
        let wake = {
            let pair = UnixStream::pair()?;
            pair.1.set_nonblocking(true)?;
            pair
        };
        #[cfg(windows)]
        let wake = (Mutex::new(()), Condvar::new());
        let options = ListenerOptions::new()
            .name(path.as_path().to_fs_name::<GenericFilePath>()?)
            .nonblocking(ListenerNonblockingMode::Both)
            .reclaim_name(false);
        #[cfg(windows)]
        let options = {
            use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
            use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt as _;
            options.security_descriptor(windows_identity()?.1.to_owned_sd()?)
        };
        let listener = options.create_sync()?;
        let server = Self {
            listener,
            #[cfg(any(unix, test))]
            name: name.to_owned(),
            stopped: AtomicBool::new(false),
            wake,
            #[cfg(unix)]
            _lock: lock,
        };

        // macOS cannot set a socket's mode before bind. The private directory
        // protects it until chmod, and Server cleans up if chmod fails.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            track_socket(path.clone())?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(server)
    }

    /// Bind a fixture and remove its newly created lock during endpoint cleanup.
    /// The caller must reserve the name until the listener has been released.
    #[cfg(test)]
    pub(crate) fn bind_test(name: &str) -> io::Result<Self> {
        #[cfg(unix)]
        let path = address(name)?;
        #[cfg(unix)]
        let remove_lock = !path.with_extension("lock").try_exists()?;
        let result = Self::bind(name);
        #[cfg(unix)]
        if remove_lock {
            match &result {
                Ok(server) if server._lock.is_some() => {
                    let mut sockets = SOCKETS.lock().unwrap();
                    let socket = sockets
                        .as_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|socket| socket.path == path)
                        .unwrap();
                    socket.remove_lock = true;
                }
                Err(_) => {
                    let _ = std::fs::remove_file(path.with_extension("lock"));
                }
                _ => {}
            }
        }
        result
    }

    /// Return the protocol name a native client uses to connect.
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Wait for a complete request, checking for shutdown between accepts.
    pub(crate) fn recv(&self) -> io::Result<Request> {
        loop {
            if let Some(request) = self.recv_timeout(IO_TIMEOUT)? {
                return Ok(request);
            }
            if self.stopped.load(Ordering::Acquire) {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
        }
    }

    /// Bound the idle accept wait and the subsequent request read independently.
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<Request>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Ok(None);
            }
            match self.listener.accept() {
                Ok(socket) => {
                    #[cfg(windows)]
                    let socket = Socket::Server(socket);
                    return Request::read(Stream::new(socket, IO_TIMEOUT)).map(Some);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(err) = self.wait(deadline)
                        && err.kind() != io::ErrorKind::Interrupted
                    {
                        thread::sleep(ACCEPT_BACKOFF);
                        return Err(err);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    thread::sleep(ACCEPT_BACKOFF);
                    return Err(err);
                }
            }
        }
    }

    /// Block until a connection, shutdown notification or idle deadline arrives.
    #[cfg(unix)]
    fn wait(&self, deadline: Instant) -> io::Result<()> {
        let Listener::UdSocket(listener) = &self.listener;
        let mut fds = [
            libc::pollfd {
                fd: listener.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = deadline.saturating_duration_since(Instant::now());
        let millis = timeout.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        // SAFETY: fds contains two initialized entries whose descriptors remain alive
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        Ok(())
    }

    /// Wait for the next pipe poll or a shutdown notification.
    #[cfg(windows)]
    fn wait(&self, deadline: Instant) -> io::Result<()> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        let _guard = self
            .wake
            .1
            .wait_timeout_while(
                self.wake.0.lock().unwrap(),
                ACCEPT_POLL.min(timeout),
                |_| !self.stopped.load(Ordering::Acquire),
            )
            .unwrap();
        Ok(())
    }

    /// Wake an idle receiver during shutdown.
    pub(crate) fn unblock(&self) {
        #[cfg(windows)]
        let _guard = self.wake.0.lock().unwrap();
        self.stopped.store(true, Ordering::Release);
        #[cfg(unix)]
        let _ = (&self.wake.1).write(&[1]);
        #[cfg(windows)]
        self.wake.1.notify_all();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(path) = address(&self.name) {
            let mut sockets = SOCKETS.lock().unwrap();
            if let Some(sockets) = sockets.as_mut()
                && let Some(index) = sockets.iter().position(|socket| socket.path == path)
            {
                sockets.swap_remove(index).remove();
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Register a bound socket for cleanup on normal process exit.
#[cfg(unix)]
fn track_socket(path: PathBuf) -> io::Result<()> {
    let mut sockets = SOCKETS.lock().unwrap();
    if sockets.is_none() {
        // process::exit skips destructors, including those in registry worker threads
        // SAFETY: cleanup_sockets has the required ABI and remains valid until process exit
        if unsafe { libc::atexit(cleanup_sockets) } != 0 {
            return Err(io::Error::other("could not register local IPC cleanup"));
        }
        *sockets = Some(Vec::new());
    }
    sockets.as_mut().unwrap().push(SocketFiles {
        path,
        #[cfg(test)]
        remove_lock: false,
    });
    Ok(())
}

/// Remove owned endpoints without waiting for another thread during process exit.
#[cfg(unix)]
extern "C" fn cleanup_sockets() {
    if let Ok(mut sockets) = SOCKETS.try_lock()
        && let Some(sockets) = sockets.as_mut()
    {
        for socket in sockets.drain(..) {
            socket.remove();
        }
    }
}

/// Create a private directory and lock a name before reclaiming its socket.
#[cfg(unix)]
fn prepare(path: &std::path::Path) -> io::Result<Option<File>> {
    use std::fs::{DirBuilder, OpenOptions};
    use std::os::unix::fs::{
        DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
    };
    let directory = path.parent().unwrap();
    match DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    verify_directory(directory)?;
    let name = path.file_name().unwrap().to_str().unwrap();
    if name.starts_with("c-") || name.starts_with("t-") {
        return Ok(None);
    }
    // SAFETY: geteuid has no preconditions and does not retain pointers
    let uid = unsafe { libc::geteuid() };
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.with_extension("lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC lock is not private",
        ));
    }
    lock.try_lock().map_err(|err| match err {
        std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::AddrInUse),
        std::fs::TryLockError::Error(err) => err,
    })?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
            std::fs::remove_file(path)?
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local endpoint is not an owned socket",
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    Ok(Some(lock))
}

/// Reject redirected or accessible IPC directories on both sides of a connection.
#[cfg(unix)]
fn verify_directory(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions and does not retain pointers
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC directory is not private",
        ));
    }
    Ok(())
}

/// One bounded HTTP request whose reply closes the native connection.
pub(crate) struct Request {
    /// Parsed HTTP method.
    method: Method,
    /// Exact request target, validated by the endpoint handler.
    path: String,
    /// Headers including repeated fields for application validation.
    headers: Vec<Header>,
    /// Complete request body, bounded before allocation.
    body: Vec<u8>,
    /// Connection retained until the response is written.
    stream: Stream,
}

impl Request {
    /// Parse one HTTP/1.0 request under fixed size and time limits.
    fn read(stream: Stream) -> io::Result<Self> {
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            if head.len() + count > MAX_REQUEST {
                return Self::reject(reader.into_inner(), 413);
            }
            head.extend_from_slice(&available[..count]);
            reader.consume(count);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let mut fields = [httparse::EMPTY_HEADER; 32];
        let mut parsed = httparse::Request::new(&mut fields);
        if !matches!(parsed.parse(&head), Ok(httparse::Status::Complete(_)))
            || parsed.version != Some(0)
        {
            return Self::reject(reader.into_inner(), 400);
        }
        let method = parsed
            .method
            .unwrap()
            .parse::<Method>()
            .map_err(|()| io::Error::from(io::ErrorKind::InvalidData))?;
        let path = parsed.path.unwrap().to_owned();
        let mut headers = Vec::new();
        let mut length = None;
        for field in parsed.headers {
            if field.name.eq_ignore_ascii_case("Transfer-Encoding") {
                return Self::reject(reader.into_inner(), 413);
            }
            if field.name.eq_ignore_ascii_case("Content-Length") {
                if length.is_some() {
                    return Self::reject(reader.into_inner(), 400);
                }
                length = std::str::from_utf8(field.value)
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok());
                if length.is_none() {
                    return Self::reject(reader.into_inner(), 400);
                }
                if length.unwrap() > MAX_REQUEST {
                    return Self::reject(reader.into_inner(), 413);
                }
            }
            headers.push(
                Header::from_bytes(field.name, field.value)
                    .map_err(|()| io::Error::from(io::ErrorKind::InvalidData))?,
            );
        }
        let mut body = vec![0; length.unwrap_or(0)];
        reader.read_exact(&mut body)?;
        Ok(Self {
            method,
            path,
            headers,
            body,
            stream: reader.into_inner(),
        })
    }

    /// Reject framing before dispatching anything to an application handler.
    fn reject(mut stream: Stream, status: u16) -> io::Result<Self> {
        let _ = write!(
            stream,
            "HTTP/1.0 {status} Rejected\r\nContent-Length: 0\r\n\r\n"
        );
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local request framing",
        ))
    }

    /// Return the request method.
    pub(crate) fn method(&self) -> &Method {
        &self.method
    }
    /// Return the exact request target.
    pub(crate) fn url(&self) -> &str {
        &self.path
    }
    /// Return all headers, preserving duplicate fields.
    pub(crate) fn headers(&self) -> &[Header] {
        &self.headers
    }
    /// Borrow the complete request body, bounded before allocation.
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }
    /// Send an HTTP response and release the native connection.
    pub(crate) fn respond<R: Read>(self, response: Response<R>) -> io::Result<()> {
        self.stream.set_write_timeout(IO_TIMEOUT);
        response.raw_print(self.stream, HTTPVersion(1, 0), &self.headers, false, None)
    }
    /// Expose the response stream to scripted peers testing partial replies.
    #[cfg(test)]
    pub(crate) fn into_writer(self) -> Stream {
        self.stream
    }
}

/// Local transport permissions, ownership and deadline regressions.
#[cfg(test)]
mod tests {
    use super::*;

    /// Private listeners preserve replies and refuse a second live owner.
    #[test]
    fn test_local_roundtrip_and_exclusive_owner() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        assert!(Server::bind(&name).is_err());
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.url(), "/test");
            request.respond(Response::from_string("accepted")).unwrap();
        });
        let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        client
            .write_all(b"GET /test HTTP/1.0\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        assert!(reply.ends_with("\r\n\r\naccepted"), "{reply:?}");
        worker.join().unwrap();
    }

    /// Unique control and test endpoints leave neither sockets nor lock files.
    #[test]
    #[cfg(unix)]
    fn test_unique_endpoints_leave_no_files_on_drop() {
        for prefix in ["c", "t"] {
            let name = format!("{prefix}-{}", identity());
            let server = Server::bind(&name).unwrap();
            let path = address(&name).unwrap();
            assert!(path.exists());
            assert!(!path.with_extension("lock").exists());
            drop(server);
            assert!(!path.exists());
            assert!(!path.with_extension("lock").exists());
        }
    }

    /// Fixture cleanup removes newly created locks and preserves pre-existing ones.
    #[test]
    #[cfg(unix)]
    fn test_fixture_lock_cleanup_preserves_existing_locks() {
        let _exclusive = PROCESS_TEST.lock().unwrap();
        let name = format!("registry-{}", &identity()[..32]);
        let path = address(&name).unwrap();
        drop(Server::bind_test(&name).unwrap());
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());

        drop(Server::bind(&name).unwrap());
        drop(Server::bind_test(&name).unwrap());
        assert!(!path.exists());
        assert!(path.with_extension("lock").exists());
        std::fs::remove_file(path.with_extension("lock")).unwrap();
    }

    /// Normal process exit removes sockets even when listener destructors are skipped.
    #[test]
    #[cfg(unix)]
    fn test_process_exit_cleans_sockets() {
        let _exclusive = PROCESS_TEST.lock().unwrap();
        if let Ok(id) = std::env::var("ARK_IPC_EXIT_TEST") {
            let _control = Server::bind(&format!("c-{id}")).unwrap();
            let _test = Server::bind(&format!("t-{id}")).unwrap();
            let _registry = Server::bind(&format!("registry-{}", &id[..32])).unwrap();
            let _fixture = Server::bind_test(&format!("registry-fixture-{}", &id[..32])).unwrap();
            std::process::exit(0);
        }
        let id = identity();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("test_process_exit_cleans_sockets")
            .env("ARK_IPC_EXIT_TEST", &id)
            .status()
            .unwrap();
        assert!(status.success());
        for name in [format!("c-{id}"), format!("t-{id}")] {
            let path = address(&name).unwrap();
            assert!(!path.exists());
            assert!(!path.with_extension("lock").exists());
        }
        let path = address(&format!("registry-{}", &id[..32])).unwrap();
        assert!(!path.exists());
        assert!(path.with_extension("lock").exists());
        std::fs::remove_file(path.with_extension("lock")).unwrap();
        let fixture = address(&format!("registry-fixture-{}", &id[..32])).unwrap();
        assert!(!fixture.exists());
        assert!(!fixture.with_extension("lock").exists());
    }

    /// Exit skips socket cleanup when its bookkeeping mutex is held.
    #[test]
    #[cfg(unix)]
    fn test_process_exit_skips_busy_cleanup() {
        let _exclusive = PROCESS_TEST.lock().unwrap();
        if let Ok(name) = std::env::var("ARK_IPC_BUSY_EXIT_TEST") {
            let _server = Server::bind(&name).unwrap();
            let _sockets = SOCKETS.lock().unwrap();
            std::process::exit(0);
        }
        let name = format!("t-{}", identity());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("test_process_exit_skips_busy_cleanup")
            .env("ARK_IPC_BUSY_EXIT_TEST", &name)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        std::fs::remove_file(address(&name).unwrap()).unwrap();
        assert!(
            status
                .expect("socket cleanup blocked process exit")
                .success()
        );
    }

    /// Idle accepts honor their timeout and wake promptly during shutdown.
    #[test]
    fn test_idle_timeout_and_shutdown_wakeup() {
        let name = format!("t-{}", identity());
        let server = std::sync::Arc::new(Server::bind(&name).unwrap());
        let started = Instant::now();
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(1));

        let (finished, result) = std::sync::mpsc::channel();
        let receiver = server.clone();
        let worker = thread::spawn(move || {
            let request = receiver.recv_timeout(Duration::from_secs(30));
            finished
                .send(request.map(|request| request.is_none()))
                .unwrap();
        });
        thread::sleep(Duration::from_millis(50));
        server.unblock();
        assert!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
        );
        worker.join().unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_secs(30))
                .unwrap()
                .is_none()
        );
    }

    /// Exhausted file descriptors delay failed accepts without busy looping.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_accept_errors_back_off() {
        let _exclusive = PROCESS_TEST.lock().unwrap();
        if std::env::var_os("ARK_IPC_ACCEPT_ERROR_TEST").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("test_accept_errors_back_off")
                .env("ARK_IPC_ACCEPT_ERROR_TEST", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let _client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: limit is writable and the resource selector is valid
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let exhausted = libc::rlimit {
            rlim_cur: 0,
            ..limit
        };
        // SAFETY: only this subprocess's soft limit changes, and exhausted is initialized
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &exhausted) },
            0
        );
        let started = Instant::now();
        let result = server.recv_timeout(Duration::from_secs(1));
        let elapsed = started.elapsed();
        // SAFETY: limit contains the original resource limits of this subprocess
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::EMFILE));
        assert!(elapsed >= Duration::from_millis(50));
        assert!(elapsed < Duration::from_secs(1));
    }

    /// Socket permissions restrict access and stale recovery preserves files.
    #[test]
    #[cfg(unix)]
    fn test_permissions_stale_recovery_and_file_preservation() {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::net::UnixListener;
        let _exclusive = PROCESS_TEST.lock().unwrap();
        let name = format!("registry-{}", &identity()[..32]);
        let server = Server::bind(&name).unwrap();
        let path = address(&name).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        drop(server);
        assert!(!path.exists());
        assert!(path.with_extension("lock").exists());

        // A crashed owner's socket remains after its kernel listener disappears
        drop(UnixListener::bind(&path).unwrap());
        let server = Server::bind(&name).unwrap();
        assert!(Stream::connect(&name, IO_TIMEOUT).is_ok());
        drop(server);
        std::fs::write(&path, b"preserve this file").unwrap();
        assert!(
            matches!(Server::bind(&name), Err(err) if err.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve this file");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(path.with_extension("lock")).unwrap();
    }

    /// Incomplete requests expire without preventing the next request.
    #[test]
    fn test_partial_request_is_bounded() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let mut stalled = Stream::connect(&name, IO_TIMEOUT).unwrap();
        stalled.write_all(b"GET /").unwrap();
        let started = Instant::now();
        let err = server.recv().err().expect("partial request was accepted");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < Duration::from_secs(4));
        drop(stalled);
        let mut next = Stream::connect(&name, IO_TIMEOUT).unwrap();
        next.write_all(b"GET /next HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(server.recv().unwrap().url(), "/next");
    }

    /// An idle reply times out, then fragmented data and peer closure remain readable.
    #[test]
    fn test_idle_reply_resumes_and_preserves_fragments() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        client.write_all(b"GET /test HTTP/1.0\r\n\r\n").unwrap();
        let mut reply = server.recv().unwrap().into_writer();

        // An open pipe with no response bytes is idle, not at EOF
        client.set_read_timeout(Duration::from_millis(40));
        let err = client.read(&mut [0]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");

        // The first fragment survives an idle interval before the second arrives
        reply.write_all(b"first").unwrap();
        client.set_read_timeout(IO_TIMEOUT);
        let mut first = [0; 5];
        client.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"first");
        client.set_read_timeout(Duration::from_millis(40));
        let err = client.read(&mut [0]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");

        // Closing after the final fragment supplies real EOF
        reply.write_all(b"last").unwrap();
        drop(reply);
        client.set_read_timeout(IO_TIMEOUT);
        let mut last = String::new();
        client.read_to_string(&mut last).unwrap();
        assert_eq!(last, "last");
    }

    /// A peer that stops consuming bytes cannot leave a writer blocked indefinitely.
    #[test]
    fn test_full_send_buffer_obeys_the_write_deadline() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        let _peer = server.listener.accept().unwrap();
        client.set_write_timeout(Duration::from_millis(40));

        // Bound the attempted data while filling the OS buffer without a reader
        let mut failure = None;
        for _ in 0..2048 {
            if let Err(err) = client.write_all(&[0; 8192]) {
                failure = Some(err);
                break;
            }
        }
        let err = failure.expect("the send buffer accepted 16 MiB without a reader");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    }

    /// A busy Windows pipe respects the native client's connection timeout.
    #[test]
    #[cfg(windows)]
    fn test_busy_pipe_connection_is_bounded() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let _first = Stream::connect(&name, IO_TIMEOUT).unwrap();
        let (finished, result) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            finished
                .send(Stream::connect(&name, Duration::from_millis(40)).map(|_| ()))
                .unwrap();
        });

        // Accept only after the deadline check, releasing a regressed waiting client
        let answer = result.recv_timeout(Duration::from_secs(1));
        let _peer = server.listener.accept().unwrap();
        worker.join().unwrap();
        let err = answer
            .expect("connection exceeded its deadline")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    }
}
