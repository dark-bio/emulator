// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounded socket I/O and HTTP exchanges, with user-owned native listeners.
//!
//! Native listeners use filesystem Unix-domain sockets. Their directories
//! admit only the current user, through Unix permissions or Windows DACLs.
//! Browser discovery uses the same bounded streams over loopback TCP, with its
//! own route table. Each HTTP connection carries one request and one response.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use polling::{Event, Events, Poller};
use sha2::{Digest as _, Sha256};
use socket2::{Domain, SockAddr, Socket, Type};
use tiny_http::{Header, Method, Response};
use ureq_proto::http::Version;

#[cfg(windows)]
#[path = "local_windows.rs"]
mod windows;
#[cfg(windows)]
use windows::{create_directory, prepare, verify_directory};

/// Registration key for the sole socket owned by each readiness poller.
const SOCKET_EVENT: usize = 0;
/// Maximum attempts to allocate a fresh private channel directory.
const DIRECTORY_ATTEMPTS: usize = 16;
/// Delay after a listener fails to accept a connection.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Maximum time to receive a request or deliver a response.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum request header and body sizes, each in bytes.
const MAX_REQUEST: usize = 8192;
/// Process-local component distinguishing simultaneous listener names.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Endpoints retained for cleanup when process exit skips their destructors.
static SOCKETS: Mutex<Option<Vec<SocketFiles>>> = Mutex::new(None);

/// Excludes process spawning while a test releases and reacquires a file lock.
/// Forked children retain inherited locks until exec closes their descriptors.
#[cfg(all(test, unix))]
pub(crate) static PROCESS_TEST: Mutex<()> = Mutex::new(());

/// Files owned by a bound local endpoint.
struct SocketFiles {
    /// Socket removed on listener drop or process exit.
    path: PathBuf,
    /// Whether the socket's parent is a private directory owned by this endpoint.
    remove_directory: bool,
    /// Whether a fixture created the lock while exclusively reserving its name.
    #[cfg(test)]
    remove_lock: bool,
}

impl SocketFiles {
    /// Remove the socket and any lock owned exclusively by a fixture.
    fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
        if self.remove_directory {
            let _ = std::fs::remove_dir(self.path.parent().unwrap());
        }
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

/// Resolve the shared IPC root within this user's local application data.
fn directory() -> io::Result<PathBuf> {
    dirs::data_local_dir()
        .map(|path| path.join("bio.dark.emulator").join("ipc"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "local application data directory is unavailable",
            )
        })
}

/// Resolve a protocol name to the same short filename on every host.
pub(crate) fn address(name: &str) -> io::Result<PathBuf> {
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
    let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
    let path = directory()?.join(&digest[..32]);
    SockAddr::unix(&path)?;
    Ok(path)
}

/// Create the private IPC root or verify its existing ownership and permissions.
fn ensure_directory(path: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    match create_directory(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    verify_directory(path)
}

/// Private directory retained until QEMU's endpoint has been removed.
pub(crate) struct PrivateDirectory {
    /// Directory removed after its socket is closed.
    path: PathBuf,
}

impl PrivateDirectory {
    /// Locate a socket within this access-controlled directory.
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Create a private directory before QEMU can bind a socket inside it.
pub(crate) fn private_directory() -> io::Result<PrivateDirectory> {
    let root = directory()?;
    ensure_directory(&root)?;

    // A collision never adopts or removes a directory belonging to another endpoint
    for _ in 0..DIRECTORY_ATTEMPTS {
        let path = root.join(format!("h-{}", &identity()[..16]));
        match create_directory(&path) {
            Ok(()) => {
                let directory = PrivateDirectory { path };
                verify_directory(directory.path())?;
                return Ok(directory);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a private IPC directory",
    ))
}

/// Nonblocking socket registered for readiness and explicit wakeups on every host.
struct ReadySocket {
    /// Poller released before its socket, after unregistering it in `Drop`.
    poller: Poller,
    /// Socket kept alive for its entire readiness registration.
    socket: Socket,
}

impl ReadySocket {
    /// Register an owned socket without requesting events until a wait begins.
    fn new(socket: Socket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let poller = Poller::new()?;
        // SAFETY: this wrapper owns the socket and unregisters it before closing it
        unsafe { poller.add(&socket, Event::none(SOCKET_EVENT))? };
        Ok(Self { poller, socket })
    }

    /// Wait for readiness, a shutdown notification or the fixed deadline.
    fn wait(&self, interest: Event, deadline: Instant) -> io::Result<()> {
        // One-shot interest is rearmed after every WouldBlock, including timed-out waits
        self.poller.modify(&self.socket, interest)?;
        let mut events = Events::with_capacity(NonZeroUsize::new(1).unwrap());
        self.poller.wait_deadline(&mut events, deadline)?;
        Ok(())
    }
}

impl Drop for ReadySocket {
    fn drop(&mut self) {
        let _ = self.poller.delete(&self.socket);
    }
}

/// A local connection with a single deadline for each read or write phase.
pub(crate) struct Stream {
    /// Nonblocking connection and its platform-independent readiness wait.
    io: ReadySocket,
    /// Deadline shared by partial reads in the current phase.
    read_deadline: Cell<Instant>,
    /// Deadline shared by partial writes in the current phase.
    write_deadline: Cell<Instant>,
}

impl Stream {
    /// Connect to a native endpoint without consulting proxies or DNS.
    pub(crate) fn connect(name: &str, timeout: Duration) -> io::Result<Self> {
        let path = address(name)?;
        Self::connect_path(&path, timeout)
    }

    /// Connect to a private endpoint supplied directly by the launcher.
    pub(crate) fn connect_path(path: &std::path::Path, timeout: Duration) -> io::Result<Self> {
        verify_directory(path.parent().unwrap())?;
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        socket.connect_timeout(&SockAddr::unix(path)?, timeout)?;
        Self::new(socket, timeout)
    }

    /// Wrap a connection with readiness waits and bounded I/O.
    pub(super) fn new(socket: Socket, timeout: Duration) -> io::Result<Self> {
        let io = ReadySocket::new(socket)?;
        let deadline = Instant::now() + timeout;
        Ok(Self {
            io,
            read_deadline: Cell::new(deadline),
            write_deadline: Cell::new(deadline),
        })
    }

    /// Set the budget for the next response read phase.
    pub(crate) fn set_read_timeout(&self, timeout: Duration) {
        self.read_deadline.set(Instant::now() + timeout);
    }

    /// Set the budget for the next request write phase.
    pub(crate) fn set_write_timeout(&self, timeout: Duration) {
        self.write_deadline.set(Instant::now() + timeout);
    }

    /// Retry readiness under a fixed deadline, never replaying an application request.
    fn bounded<T>(
        &self,
        deadline: Instant,
        interest: Event,
        mut operation: impl FnMut(&Socket) -> io::Result<T>,
    ) -> io::Result<T> {
        loop {
            // A delayed worker must try queued I/O before deciding it needs more time
            match operation(&self.io.socket) {
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "local IPC deadline expired",
                        ));
                    }
                    if err.kind() == io::ErrorKind::WouldBlock
                        && let Err(err) = self.io.wait(interest, deadline)
                        && err.kind() != io::ErrorKind::Interrupted
                    {
                        return Err(err);
                    }
                }
                result => return result,
            }
        }
    }
}

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.bounded(
            self.read_deadline.get(),
            Event::readable(SOCKET_EVENT),
            |mut socket| socket.read(bytes),
        )
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bounded(
            self.write_deadline.get(),
            Event::writable(SOCKET_EVENT),
            |mut socket| socket.write(bytes),
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.io.socket).flush()
    }
}

/// One user-owned listener with exclusive name ownership and bounded accepts.
pub(crate) struct Server {
    /// Listener and readiness notifications, inaccessible to browser networking APIs.
    listener: ReadySocket,
    /// Endpoint name, never an arbitrary path supplied by discovery.
    #[cfg(test)]
    name: String,
    /// Interrupts an accept loop during launcher shutdown.
    stopped: AtomicBool,
    /// Removes the endpoint after the listener closes and before releasing its lock.
    _binding: Binding,
}

/// Socket path and ownership lock retained through listener teardown.
struct Binding {
    /// Socket removed after its listener has closed.
    path: PathBuf,
    /// Held across stale socket removal, binding and final socket cleanup.
    _lock: Option<File>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        remove_socket(&self.path);
    }
}

impl Server {
    /// Bind a private endpoint, recovering a stale socket only under its lock.
    pub(crate) fn bind(name: &str) -> io::Result<Self> {
        let path = address(name)?;
        ensure_directory(path.parent().unwrap())?;
        let lock = if name.starts_with("c-") || name.starts_with("t-") {
            None
        } else {
            Some(prepare(&path)?)
        };
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        listener.bind(&SockAddr::unix(&path)?)?;
        let binding = Binding {
            path: path.clone(),
            _lock: lock,
        };
        listener.listen(128)?;
        let listener = ReadySocket::new(listener)?;
        track_socket(path.clone(), false)?;
        let server = Self {
            listener,
            #[cfg(test)]
            name: name.to_owned(),
            stopped: AtomicBool::new(false),
            _binding: binding,
        };

        // macOS cannot set a socket's mode before bind. The private directory
        // protects it until chmod, and Server cleans up if chmod fails.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(server)
    }

    /// Bind a fixture and remove its newly created lock during endpoint cleanup.
    /// The caller must reserve the name until the listener has been released.
    #[cfg(test)]
    pub(crate) fn bind_test(name: &str) -> io::Result<Self> {
        let path = address(name)?;
        let remove_lock = !path.with_extension("lock").try_exists()?;
        let result = Self::bind(name);
        if remove_lock {
            match &result {
                Ok(server) if server._binding._lock.is_some() => {
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
        self.accept_timeout(timeout)?
            .map(|stream| Request::read(stream, MAX_REQUEST))
            .transpose()
    }

    /// Accept a bounded native stream without imposing HTTP framing.
    pub(crate) fn accept_timeout(&self, timeout: Duration) -> io::Result<Option<Stream>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Ok(None);
            }
            match self.listener.socket.accept() {
                Ok((socket, _)) => {
                    return Stream::new(socket, IO_TIMEOUT).map(Some);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(err) = self.listener.wait(Event::readable(SOCKET_EVENT), deadline)
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

    /// Wake an idle receiver during shutdown.
    pub(crate) fn unblock(&self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.listener.poller.notify();
    }
}

/// Register a bound socket for cleanup on normal process exit.
pub(crate) fn track_socket(path: PathBuf, remove_directory: bool) -> io::Result<()> {
    let mut sockets = SOCKETS.lock().unwrap();
    if sockets.is_none() {
        // Unix also runs this backstop when main returns without explicit cleanup
        #[cfg(unix)]
        // SAFETY: cleanup_sockets has the required ABI and remains valid until process exit
        if unsafe { libc::atexit(cleanup_sockets) } != 0 {
            return Err(io::Error::other("could not register local IPC cleanup"));
        }
        *sockets = Some(Vec::new());
    }
    sockets.as_mut().unwrap().push(SocketFiles {
        path,
        remove_directory,
        #[cfg(test)]
        remove_lock: false,
    });
    Ok(())
}

/// Remove owned endpoints best-effort before exiting without Rust destructors.
pub(crate) fn exit(code: i32) -> ! {
    // Windows ExitProcess bypasses C atexit callbacks
    cleanup_sockets();
    std::process::exit(code);
}

/// Remove an owned socket and release its process-exit cleanup record.
pub(crate) fn remove_socket(path: &std::path::Path) {
    let mut sockets = SOCKETS.lock().unwrap();
    if let Some(sockets) = sockets.as_mut()
        && let Some(index) = sockets.iter().position(|socket| socket.path == path)
    {
        sockets.swap_remove(index).remove();
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove owned endpoints without waiting for another thread during process exit.
pub(crate) extern "C" fn cleanup_sockets() {
    if let Ok(mut sockets) = SOCKETS.try_lock()
        && let Some(sockets) = sockets.as_mut()
    {
        for socket in sockets.drain(..) {
            socket.remove();
        }
    }
}

/// Create a new user-only directory without accepting an existing path.
#[cfg(unix)]
fn create_directory(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Lock a persistent endpoint before reclaiming an owned stale socket.
#[cfg(unix)]
fn prepare(path: &std::path::Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
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
    Ok(lock)
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

/// One bounded HTTP request whose reply closes its socket connection.
pub(crate) struct Request {
    /// HTTP version used for the reply's status line.
    version: Version,
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
    /// Parse one HTTP request under the stream's deadline and a body size limit.
    pub(super) fn read(mut stream: Stream, limit: usize) -> io::Result<Self> {
        let request = match super::http::read_request(&mut stream, limit) {
            Ok(request) => request,
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                let status = if err
                    .get_ref()
                    .is_some_and(|err| err.is::<super::http::TooLarge>())
                {
                    413
                } else {
                    400
                };
                return Self::reject(stream, status);
            }
            Err(err) => return Err(err),
        };
        let method = request
            .method()
            .as_str()
            .parse::<Method>()
            .map_err(|()| io::Error::from(io::ErrorKind::InvalidData))?;
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str())
            .to_owned();
        let mut headers = Vec::new();
        for (name, value) in request.headers() {
            headers.push(
                Header::from_bytes(name.as_str(), value.as_bytes())
                    .map_err(|()| io::Error::from(io::ErrorKind::InvalidData))?,
            );
        }
        Ok(Self {
            version: request.version(),
            method,
            path,
            headers,
            body: request.into_body(),
            stream,
        })
    }

    /// Reject framing before dispatching anything to an application handler.
    fn reject(mut stream: Stream, status: u16) -> io::Result<Self> {
        stream.set_write_timeout(IO_TIMEOUT);
        let _ = write!(
            stream,
            "HTTP/1.1 {status} Rejected\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
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
    /// Send a response with its byte length and close the socket connection.
    pub(crate) fn respond<R: Read>(mut self, response: Response<R>) -> io::Result<()> {
        let status = response.status_code();
        let mut raw = format!(
            "{:?} {} {}\r\nConnection: close\r\n",
            self.version,
            status.0,
            status.default_reason_phrase()
        )
        .into_bytes();
        for header in response.headers() {
            if !header.field.equiv("Content-Length")
                && !header.field.equiv("Transfer-Encoding")
                && !header.field.equiv("Connection")
            {
                write!(raw, "{header}\r\n")?;
            }
        }
        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body)?;
        // These statuses have no response body, so no length is needed
        if matches!(status.0, 100..=199 | 204 | 304) {
            raw.extend_from_slice(b"\r\n");
        } else {
            write!(raw, "Content-Length: {}\r\n\r\n", body.len())?;
            if self.method != Method::Head {
                raw.extend_from_slice(&body);
            }
        }
        self.stream.set_write_timeout(IO_TIMEOUT);
        self.stream.write_all(&raw)
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

    /// Protocol names map to the app-data paths native clients expect on every host.
    #[test]
    fn test_endpoint_paths_match_the_shared_namespace() {
        let root = dirs::data_local_dir()
            .unwrap()
            .join("bio.dark.emulator")
            .join("ipc");
        for (name, file) in [
            ("registry-18180", "675ee216b25e35275ab9abd52629e31f"),
            ("c-0123456789abcdef", "498ca4e5b7df41bc7d678993c7bb742e"),
        ] {
            assert_eq!(address(name).unwrap(), root.join(file), "{name}");
        }
    }

    /// Private channel directories share the IPC root and retain exclusive ownership.
    #[test]
    fn test_private_directories_are_distinct_and_removed_on_drop() {
        let first = private_directory().unwrap();
        let second = private_directory().unwrap();
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        assert_eq!(first_path.parent().unwrap(), directory().unwrap());
        assert_eq!(first_path.parent(), second_path.parent());
        assert_ne!(first_path, second_path);
        verify_directory(&first_path).unwrap();
        verify_directory(&second_path).unwrap();

        // Exclusive creation preserves an existing directory and its contents
        let retained = second_path.join("retained");
        std::fs::write(&retained, b"owned by the other channel").unwrap();
        assert_eq!(
            create_directory(&second_path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(first);
        assert!(!first_path.exists());
        assert_eq!(
            std::fs::read(&retained).unwrap(),
            b"owned by the other channel"
        );
        std::fs::remove_file(retained).unwrap();
        drop(second);
        assert!(!second_path.exists());
    }

    /// Existing broad permissions and redirected IPC directories are refused unchanged.
    #[cfg(unix)]
    #[test]
    fn test_directory_access_controls() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let directory = private_directory().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            ensure_directory(directory.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            Stream::connect_path(&directory.path().join("hw.sock"), IO_TIMEOUT)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        // Following a private target through a link must not bypass directory checks
        let target = private_directory().unwrap();
        let link = directory.path().join("link");
        symlink(target.path(), &link).unwrap();
        assert_eq!(
            ensure_directory(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::remove_file(link).unwrap();
    }

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
            .write_all(b"GET /test HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert!(reply.ends_with(b"\r\n\r\naccepted"), "{reply:?}");
        assert!(reply.windows(17).any(|bytes| bytes == b"Content-Length: 8"));
        worker.join().unwrap();
    }

    /// Unique control and test endpoints leave neither sockets nor lock files.
    #[test]
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
    fn test_fixture_lock_cleanup_preserves_existing_locks() {
        #[cfg(unix)]
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
    fn test_process_exit_cleans_sockets() {
        #[cfg(unix)]
        let _exclusive = PROCESS_TEST.lock().unwrap();
        if let Ok(id) = std::env::var("ARK_IPC_EXIT_TEST") {
            let _control = Server::bind(&format!("c-{id}")).unwrap();
            let _test = Server::bind(&format!("t-{id}")).unwrap();
            let _registry = Server::bind(&format!("registry-{}", &id[..32])).unwrap();
            let _fixture = Server::bind_test(&format!("registry-fixture-{}", &id[..32])).unwrap();
            exit(0);
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
    fn test_process_exit_skips_busy_cleanup() {
        #[cfg(unix)]
        let _exclusive = PROCESS_TEST.lock().unwrap();
        if let Ok(name) = std::env::var("ARK_IPC_BUSY_EXIT_TEST") {
            let _server = Server::bind(&name).unwrap();
            let _sockets = SOCKETS.lock().unwrap();
            exit(0);
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
    fn test_permissions_stale_recovery_and_file_preservation() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        #[cfg(unix)]
        let _exclusive = PROCESS_TEST.lock().unwrap();
        let name = format!("registry-{}", &identity()[..32]);
        let server = Server::bind(&name).unwrap();
        let path = address(&name).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
                0o700
            );
        }
        verify_directory(path.parent().unwrap()).unwrap();
        drop(server);
        assert!(!path.exists());
        assert!(path.with_extension("lock").exists());

        // A crashed owner's socket remains after its kernel listener disappears
        let stale = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        stale.bind(&SockAddr::unix(&path).unwrap()).unwrap();
        drop(stale);
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
        next.write_all(b"GET /next HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        assert_eq!(server.recv().unwrap().url(), "/next");
    }

    /// A queued stop survives a delayed reader, while an idle read still expires.
    #[test]
    fn test_buffered_stop_survives_an_expired_read_deadline() {
        // Queue the whole request before the server gets time to read it
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        let stream = server.accept_timeout(IO_TIMEOUT).unwrap().unwrap();
        client
            .write_all(b"POST /v1/stop HTTP/1.0\r\nContent-Length: 0\r\n\r\n")
            .unwrap();

        // An expired timestamp models descheduling without a timing-based wait
        stream.read_deadline.set(Instant::now() - IO_TIMEOUT);
        let request = Request::read(stream, MAX_REQUEST).unwrap();
        assert_eq!(request.method(), &Method::Post);
        assert_eq!(request.url(), "/v1/stop");
        assert!(request.body().is_empty());

        // Once queued bytes are consumed, the same deadline permits no more waiting
        let mut reply = request.into_writer();
        assert_eq!(
            reply.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        reply
            .write_all(b"HTTP/1.0 202 Accepted\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        drop(reply);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert_eq!(
            response,
            "HTTP/1.0 202 Accepted\r\nContent-Length: 0\r\n\r\n"
        );
    }

    /// An idle reply times out, then fragmented data and peer closure remain readable.
    #[test]
    fn test_idle_reply_resumes_and_preserves_fragments() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        client
            .write_all(b"GET /test HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut reply = server.recv().unwrap().into_writer();

        // An open socket with no response bytes remains idle
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

        // Fragments and EOF arriving during a readiness wait must each wake the reader
        let writer = thread::spawn(move || {
            for fragment in [b"last".as_slice(), b" reply"] {
                thread::sleep(Duration::from_millis(20));
                reply.write_all(fragment).unwrap();
            }
            thread::sleep(Duration::from_millis(20));
        });
        client.set_read_timeout(IO_TIMEOUT);
        let mut last = String::new();
        client.read_to_string(&mut last).unwrap();
        writer.join().unwrap();
        assert_eq!(last, "last reply");
    }

    /// A stalled peer bounds writes, which resume when it starts consuming bytes.
    #[test]
    fn test_full_send_buffer_times_out_and_resumes() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
        let mut peer = server.accept_timeout(IO_TIMEOUT).unwrap().unwrap();
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

        // Reusing the connection rearms writable interest after the expired deadline
        let reader = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            peer.set_read_timeout(IO_TIMEOUT);
            let mut received = Vec::new();
            peer.read_to_end(&mut received).unwrap();
            received
        });
        client.set_write_timeout(IO_TIMEOUT);
        client.write_all(b"resumed").unwrap();
        drop(client);
        let received = reader.join().unwrap();
        let buffered = received.strip_suffix(b"resumed").unwrap();
        assert!(!buffered.is_empty());
        assert!(buffered.iter().all(|byte| *byte == 0));
    }

    /// Listeners queue more than one client before accepting either connection.
    #[test]
    fn test_connections_can_queue_before_accept() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        let _first = Stream::connect(&name, IO_TIMEOUT).unwrap();
        let _second = Stream::connect(&name, IO_TIMEOUT).unwrap();
        assert!(server.accept_timeout(IO_TIMEOUT).unwrap().is_some());
        assert!(server.accept_timeout(IO_TIMEOUT).unwrap().is_some());
    }

    /// Accept readiness wakes again after an idle timeout and successive connections.
    #[test]
    fn test_accept_readiness_rearms_after_timeouts_and_connections() {
        let name = format!("t-{}", identity());
        let server = Server::bind(&name).unwrap();
        assert!(
            server
                .accept_timeout(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );

        // Each client arrives after the previous connection has been handled
        let (waiting, ready) = std::sync::mpsc::channel();
        let receiver = thread::spawn(move || {
            for expected in [1, 2, 3] {
                waiting.send(()).unwrap();
                let mut stream = server.accept_timeout(IO_TIMEOUT).unwrap().unwrap();
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                assert_eq!(byte, [expected]);
                stream.write_all(&byte).unwrap();
            }
        });
        for value in [1, 2, 3] {
            ready.recv_timeout(IO_TIMEOUT).unwrap();
            thread::sleep(Duration::from_millis(20));
            let mut client = Stream::connect(&name, IO_TIMEOUT).unwrap();
            client.write_all(&[value]).unwrap();
            let mut echoed = [0];
            client.read_exact(&mut echoed).unwrap();
            assert_eq!(echoed, [value]);
        }
        receiver.join().unwrap();
    }
}
