// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Windows pipe access control, connection deadlines and read readiness.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{
    AsHandle, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle,
};
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use interprocess::local_socket::Stream as Socket;
use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt as _, BorrowedSecurityDescriptor, SecurityDescriptor,
};
use windows_sys::Win32::{
    Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::SECURITY_IDENTIFICATION,
    System::{
        Pipes::{PIPE_NOWAIT, PeekNamedPipe, SetNamedPipeHandleState},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// Restricted client handles and accepted server connections.
pub(super) enum Stream {
    /// Synchronous handle using PIPE_NOWAIT and identification-only security.
    Client(File),
    /// Accepted connection retaining the listener's I/O and close behavior.
    Server(Socket),
}

impl AsHandle for Stream {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        match self {
            Self::Client(file) => file.as_handle(),
            Self::Server(Socket::NamedPipe(pipe)) => pipe.as_handle(),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Client(file) => file.write(bytes),
            Self::Server(socket) => socket.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Named-pipe flush would wait for peer consumption without a deadline
        Ok(())
    }
}

/// Connect within the deadline without allowing the server to act as this user.
pub(super) fn connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
    // OpenOptions adds SECURITY_SQOS_PRESENT. Importing this handle into
    // interprocess would reopen it without retaining the impersonation limit.
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .security_qos_flags(SECURITY_IDENTIFICATION);
    let file = super::bounded(Instant::now() + timeout, || {
        options.open(path).map_err(|err| {
            if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
                io::ErrorKind::WouldBlock.into()
            } else {
                err
            }
        })
    })?;

    // Synchronous reads and writes return immediately in PIPE_NOWAIT mode
    // SAFETY: file owns a valid pipe handle and the mode pointer remains live
    if unsafe {
        SetNamedPipeHandleState(file.as_raw_handle(), &PIPE_NOWAIT, ptr::null(), ptr::null())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Stream::Client(file))
}

/// Read available bytes while distinguishing an idle pipe from a closed peer.
pub(super) fn read(socket: &mut Stream, bytes: &mut [u8]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }

    // interprocess maps ERROR_NO_DATA from an empty PIPE_NOWAIT read to EOF
    let mut available = 0;
    // SAFETY: the pipe handle stays alive and available is writable. Other outputs are unused.
    if unsafe {
        PeekNamedPipe(
            socket.as_handle().as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    } == 0
    {
        let err = io::Error::last_os_error();
        return match err.raw_os_error().map(|code| code as u32) {
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) => Ok(0),
            _ => Err(err),
        };
    }
    if available == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    // This stream has one reader, so the peeked bytes cannot be consumed elsewhere
    let result = match socket {
        Stream::Client(file) => file.read(bytes),
        Stream::Server(pipe) => pipe.read(bytes),
    };
    // The peer can close between PeekNamedPipe and the actual read
    match result {
        Err(err)
            if matches!(
                err.raw_os_error().map(|code| code as u32),
                Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
            ) =>
        {
            Ok(0)
        }
        result => result,
    }
}

/// Read the current user's SID and construct a pipe DACL admitting only that user.
pub(super) fn identity() -> io::Result<&'static (String, SecurityDescriptor)> {
    /// Successful identity lookup retained for the process lifetime.
    static IDENTITY: OnceLock<(String, SecurityDescriptor)> = OnceLock::new();
    /// Serializes initialization while allowing failed lookups to be retried.
    static INITIALIZING: Mutex<()> = Mutex::new(());
    if let Some(identity) = IDENTITY.get() {
        return Ok(identity);
    }
    let _initializing = INITIALIZING.lock().unwrap();
    if let Some(identity) = IDENTITY.get() {
        return Ok(identity);
    }
    let identity = read_identity()?;
    Ok(IDENTITY.get_or_init(|| identity))
}

/// Read the process token and construct a DACL restricted to its user's SID.
fn read_identity() -> io::Result<(String, SecurityDescriptor)> {
    let mut token = ptr::null_mut();
    // SAFETY: the output handle is writable and GetCurrentProcess is a valid pseudo-handle
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned an owned handle, closed exactly once by OwnedHandle
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    // SAFETY: a null buffer with length zero requests the required allocation size
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut size,
        );
    }
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: the allocation is large enough and aligned for TOKEN_USER and its SID
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser retrieval initialized TOKEN_USER and its embedded SID
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = ptr::null_mut();
    // SAFETY: the SID remains alive in buffer and the API allocates the output string
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the API returns a null-terminated UTF-16 string owned by the local heap
    let text = unsafe {
        let mut length = 0;
        while *sid.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, length));
        LocalFree(sid.cast());
        text
    };

    // A protected DACL prevents inherited access for Everyone or anonymous callers
    let sddl: Vec<_> = format!("D:P(A;;GA;;;{text})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: sddl is terminated and both output arguments meet the API's contract
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the returned descriptor stays alive through its deep copy, then is freed once
    let owned = unsafe {
        let owned = BorrowedSecurityDescriptor::from_ptr(descriptor).to_owned_sd();
        LocalFree(descriptor);
        owned
    }?;
    Ok((text, owned))
}

/// Security properties checked against a real pipe connection.
#[cfg(test)]
mod tests {
    use super::super::{IO_TIMEOUT, Server, Stream as LocalStream, identity as endpoint_id};
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use windows_sys::Win32::{
        Security::{SecurityIdentification, TokenImpersonationLevel},
        System::Threading::{GetCurrentThread, OpenThreadToken},
    };

    /// A connected server can identify the client but cannot act with its token.
    #[test]
    fn test_pipe_server_cannot_impersonate_client() {
        let name = format!("t-{}", endpoint_id());
        let server = Server::bind(&name).unwrap();
        let mut client = LocalStream::connect(&name, IO_TIMEOUT).unwrap();
        client.write_all(b"identify").unwrap();
        let Socket::NamedPipe(mut peer) = server.listener.accept().unwrap();
        peer.read_exact(&mut [0; 8]).unwrap();

        // Inspect the token captured from the production client's actual write
        let _impersonation = peer.inner().impersonate_client().unwrap();
        let mut token = ptr::null_mut();
        // SAFETY: the thread pseudo-handle is valid and token is writable
        let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
        assert_ne!(opened, 0, "{}", io::Error::last_os_error());
        // SAFETY: OpenThreadToken returned a handle owned solely by this test
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut level = 0i32;
        let mut size = 0;
        // SAFETY: level is aligned and sized for SECURITY_IMPERSONATION_LEVEL
        let queried = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenImpersonationLevel,
                (&mut level as *mut i32).cast(),
                std::mem::size_of_val(&level) as u32,
                &mut size,
            )
        };
        assert_ne!(queried, 0, "{}", io::Error::last_os_error());
        assert_eq!(level, SecurityIdentification);
    }
}
