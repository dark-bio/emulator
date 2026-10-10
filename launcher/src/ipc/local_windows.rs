// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! User-owned Windows directories and stale AF_UNIX socket recovery.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;

use sha2::{Digest as _, Sha256};

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SE_FILE_OBJECT,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorOwner, GetTokenInformation, INHERITED_ACE, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CreateDirectoryW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FileAttributeTagInfo, GetFileInformationByHandleEx, READ_CONTROL,
    },
    System::{
        SystemInformation::GetSystemWindowsDirectoryW,
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, IO_REPARSE_TAG_AF_UNIX},
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// Maximum UTF-8 path bytes, leaving one byte for the AF_UNIX terminator.
const SOCKET_PATH_LIMIT: usize = 107;
/// Maximum root bytes after reserving the longest native or QEMU socket suffix.
const ROOT_LIMIT: usize = SOCKET_PATH_LIMIT - super::SOCKET_SUFFIX_LEN;
/// Application directory beneath the user's local application data.
const APP_DIRECTORY: &str = "bio.dark.emulator";
/// Socket directory beneath the application directory.
const IPC_DIRECTORY: &str = "ipc";
/// Hexadecimal characters identifying the user in a short fallback root.
const USER_ID_LEN: usize = 32;

/// Resolve an IPC root that leaves room for every native and QEMU socket.
pub(super) fn directory() -> io::Result<PathBuf> {
    if let Some(root) = dirs::data_local_dir().and_then(|path| app_directory(&path)) {
        return Ok(root);
    }
    short_directory(&system_directory()?, &sid()?)
}

/// Keep application data only when its UTF-8 path fits the longest socket suffix.
fn app_directory(base: &Path) -> Option<PathBuf> {
    let root = base.join(APP_DIRECTORY).join(IPC_DIRECTORY);
    root.to_str()
        .is_some_and(|path| path.len() <= ROOT_LIMIT)
        .then_some(root)
}

/// Name a user-only directory directly below the system drive's root.
fn short_directory(system: &Path, sid: &str) -> io::Result<PathBuf> {
    let drive = match system.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) if drive.is_ascii_alphabetic() => {
                drive
            }
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        },
        _ => return Err(io::ErrorKind::InvalidInput.into()),
    };
    let user = format!("{:x}", Sha256::digest(sid.as_bytes()));
    Ok(PathBuf::from(format!(
        "{}:\\{}{}",
        char::from(drive),
        super::ROOT_PREFIX,
        &user[..USER_ID_LEN]
    )))
}

/// Read the shared Windows directory without consulting per-user environment paths.
fn system_directory() -> io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;

    // Ask for the required size, including the terminator, before allocating
    // SAFETY: a null buffer with zero capacity requests the required buffer size
    let size = unsafe { GetSystemWindowsDirectoryW(ptr::null_mut(), 0) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0; size as usize];
    // SAFETY: buffer has the capacity passed to the API and stays live for the call
    let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), size) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= size {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(PathBuf::from(OsString::from_wide(
        &buffer[..length as usize],
    )))
}

/// Descriptor allocated by the Windows local heap.
struct Descriptor(
    /// Allocation released with `LocalFree`.
    PSECURITY_DESCRIPTOR,
);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the security APIs returned this allocation for LocalFree
        unsafe {
            LocalFree(self.0);
        }
    }
}

impl Descriptor {
    /// Construct a protected DACL inherited only by this user's children.
    fn private() -> io::Result<Self> {
        let sid = sid()?;
        let sddl: Vec<_> = format!("O:{sid}D:P(A;OICI;FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = ptr::null_mut();
        // SAFETY: sddl is terminated and descriptor is writable
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
        Ok(Self(descriptor))
    }
}

/// Create a new directory with its user-only DACL, rejecting existing paths.
pub(super) fn create_directory(path: &Path) -> io::Result<()> {
    let descriptor = Descriptor::private()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path_wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: the terminated path and descriptor remain live for the call
    if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reject reparse points, foreign owners and directory access granted to other users.
pub(super) fn verify_directory(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC directory is redirected",
        ));
    }

    // Inspect the opened directory itself, rather than following its path again
    let mut owner = ptr::null_mut();
    let mut acl: *mut ACL = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: the file is live and the requested output pointers are writable
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = Descriptor(descriptor);
    let expected = Descriptor::private()?;
    let mut current_user = ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: expected contains a valid owner and both outputs are writable
    if unsafe { GetSecurityDescriptorOwner(expected.0, &mut current_user, &mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: owner and acl belong to the live descriptor when non-null
    if owner.is_null()
        || acl.is_null()
        || unsafe { EqualSid(owner, current_user) == 0 || (*acl).AceCount != 1 }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC directory is not private",
        ));
    }
    let mut entry = ptr::null_mut();
    // SAFETY: the ACL contains one entry and entry is a writable output
    if unsafe { GetAce(acl, 0, &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: an ACE starts with ACE_HEADER, inspected before its allowed-ACE fields
    let header = unsafe { &*entry.cast::<ACE_HEADER>() };
    if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
        || header.AceSize < std::mem::size_of::<ACCESS_ALLOWED_ACE>() as u16
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC directory has an unsupported ACL",
        ));
    }
    // SAFETY: the security API returned a valid ACCESS_ALLOWED_ACE and embedded SID
    let allowed = unsafe { &*entry.cast::<ACCESS_ALLOWED_ACE>() };
    let flags = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    // SAFETY: both SIDs are backed by descriptors retained above
    let same_user = unsafe {
        EqualSid(
            (&allowed.SidStart as *const u32).cast_mut().cast(),
            current_user,
        ) != 0
    };
    if allowed.Mask != FILE_ALL_ACCESS
        || u32::from(header.AceFlags) & !INHERITED_ACE != flags
        || !same_user
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC directory is not private",
        ));
    }
    Ok(())
}

/// Hold a persistent ownership lock before deleting a stale registry socket.
pub(super) fn prepare(path: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path.with_extension("lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC lock is not a regular file",
        ));
    }
    lock.try_lock().map_err(|err| match err {
        std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::AddrInUse),
        std::fs::TryLockError::Error(err) => err,
    })?;

    // AF_UNIX endpoints are reparse points, but ordinary files and links are preserved
    match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    {
        Ok(file) => {
            let mut info = FILE_ATTRIBUTE_TAG_INFO {
                FileAttributes: 0,
                ReparseTag: 0,
            };
            // SAFETY: file is live and info is a writable buffer of the declared size
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileAttributeTagInfo,
                    (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                    std::mem::size_of_val(&info) as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if info.ReparseTag != IO_REPARSE_TAG_AF_UNIX {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "local endpoint is not a socket",
                ));
            }
            drop(file);
            std::fs::remove_file(path)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    Ok(lock)
}

/// Read the process user's SID for directory ownership and access checks.
fn sid() -> io::Result<String> {
    let mut token = ptr::null_mut();
    // SAFETY: the output handle is writable and GetCurrentProcess is a valid pseudo-handle
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned an owned handle closed exactly once
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    // SAFETY: a null buffer requests the required allocation size
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
    // SAFETY: the allocation is large enough and aligned for TOKEN_USER
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
    let mut text = ptr::null_mut();
    // SAFETY: the SID is live and the API allocates the output string
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: text is terminated UTF-16 allocated on the Windows local heap
    Ok(unsafe {
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        sid
    })
}

/// Windows filesystem access checks exercised before any socket connection.
#[cfg(test)]
mod tests {
    use super::*;

    /// Every socket fits with the longest retained app-data root or the short root.
    #[test]
    fn test_longest_socket_paths_fit() {
        use super::super::{CHANNEL_ID_LEN, CHANNEL_PREFIX, CHANNEL_SOCKET_NAME, SOCKET_NAME_LEN};

        // Fill the root budget without relying on this test runner's user profile
        let count = ROOT_LIMIT - "C:\\".len() - 1 - APP_DIRECTORY.len() - 1 - IPC_DIRECTORY.len();
        let base = PathBuf::from(format!("C:\\{}", "x".repeat(count)));
        let longest = app_directory(&base).unwrap();
        let longer = PathBuf::from(format!("{}x", base.display()));
        assert!(app_directory(&longer).is_none());

        // The fallback keeps only the drive and a fixed-size hash of the user's SID
        let system = PathBuf::from(format!("Z:\\{}\\Windows", "long-directory\\".repeat(100)));
        let short = short_directory(&system, "S-1-5-21-123-456-789-1001").unwrap();
        assert_ne!(
            short,
            short_directory(&system, "S-1-5-21-123-456-789-1002").unwrap()
        );
        for root in [longest, short] {
            let native = root.join("f".repeat(SOCKET_NAME_LEN));
            let channel = root
                .join(format!("{CHANNEL_PREFIX}{}", "f".repeat(CHANNEL_ID_LEN)))
                .join(CHANNEL_SOCKET_NAME);
            for path in [native, channel] {
                assert!(path.to_str().unwrap().len() <= 107, "{}", path.display());
                socket2::SockAddr::unix(&path).unwrap();
            }
        }
    }

    /// App-data selection counts UTF-8 bytes and rejects paths AF_UNIX cannot encode.
    #[test]
    fn test_app_directory_bounds_unicode_paths() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;

        // A two-byte character crosses the boundary before its character count does
        let count = ROOT_LIMIT - "C:\\".len() - 1 - APP_DIRECTORY.len() - 1 - IPC_DIRECTORY.len();
        let fits = PathBuf::from(format!("C:\\{}é", "x".repeat(count - 2)));
        let exceeds = PathBuf::from(format!("C:\\{}é", "x".repeat(count - 1)));
        assert!(app_directory(&fits).is_some());
        assert!(app_directory(&exceeds).is_none());

        // Windows filenames can contain an unpaired surrogate, unlike AF_UNIX paths
        let invalid = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xd800]));
        assert!(app_directory(&invalid).is_none());
    }

    /// The short fallback root has the same ownership and DACL checks as app data.
    #[test]
    fn test_short_root_access_controls() {
        let root = short_directory(&system_directory().unwrap(), &sid().unwrap()).unwrap();
        let created = match create_directory(&root) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
            Err(err) => panic!("could not create the fallback root: {err}"),
        };

        // Retain the ownership result until a newly created root has been removed
        let result = verify_directory(&root);
        if created {
            std::fs::remove_dir(&root).unwrap();
        }
        result.unwrap();
    }

    /// Directories are private before binding, and an unrestricted DACL is refused.
    #[test]
    fn test_directory_access_controls() {
        use windows_sys::Win32::Security::{
            Authorization::SetSecurityInfo, PROTECTED_DACL_SECURITY_INFORMATION,
        };
        let directory = super::super::private_directory().unwrap();
        verify_directory(directory.path()).unwrap();

        // An existing directory must never be silently accepted with a permissive ACL
        let file = OpenOptions::new()
            .access_mode(FILE_ALL_ACCESS)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(directory.path())
            .unwrap();
        // SAFETY: file is live; a null DACL intentionally grants access on this fixture
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            )
        };
        assert_eq!(status, 0);
        assert_eq!(
            verify_directory(directory.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            super::super::ensure_directory(directory.path())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            matches!(super::super::Stream::connect_path(&directory.path().join("hw.sock"),
            std::time::Duration::from_secs(1)), Err(err) if err.kind() == io::ErrorKind::PermissionDenied)
        );
    }
}
