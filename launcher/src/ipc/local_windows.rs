// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Windows pipe access control, connection deadlines and read readiness.

use std::io::{self, Read as _};
use std::os::windows::io::{AsHandle as _, AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use interprocess::ConnectWaitMode;
use interprocess::local_socket::Stream;
use interprocess::os::windows::named_pipe::{DuplexPipeStream, pipe_mode};
use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt as _, BorrowedSecurityDescriptor, SecurityDescriptor,
};
use windows_sys::Win32::{
    Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Pipes::PeekNamedPipe,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// Connect to a nonblocking pipe under the caller's connection deadline.
pub(super) fn connect(path: &Path, timeout: Duration) -> io::Result<Stream> {
    // The local-socket wrapper does not forward its wait mode on Windows
    let pipe = DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
        path.as_os_str(),
        ConnectWaitMode::Timeout(timeout),
    )?;
    pipe.set_nonblocking(true)?;
    Ok(Stream::NamedPipe(pipe.into()))
}

/// Read available bytes while distinguishing an idle pipe from a closed peer.
pub(super) fn read(socket: &mut Stream, bytes: &mut [u8]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }

    // interprocess maps ERROR_NO_DATA from an empty PIPE_NOWAIT read to EOF
    let Stream::NamedPipe(pipe) = socket;
    let mut available = 0;
    // SAFETY: the pipe handle stays alive and available is writable. Other outputs are unused.
    if unsafe {
        PeekNamedPipe(
            pipe.as_handle().as_raw_handle(),
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
    pipe.read(bytes)
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
