// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Per-OS quirks, kept out of the rest of the launcher. Each entry below is
//! one small `pub(crate)` function whose platform-specific bodies sit next to
//! it, so callers invoke it unconditionally and never carry a `cfg` of their
//! own.
//!
//!   - **Library search path**: `DYLD_LIBRARY_PATH` on macOS,
//!     `LD_LIBRARY_PATH` on Linux, `PATH` on Windows, which has no dedicated
//!     variable but does search every `PATH` directory for DLLs.
//!   - **Verbatim paths**: Windows' `\\?\` prefix, which Tauri's resource
//!     resolver adds and QEMU's mingw-w64 build cannot open through.
//!   - **Child consoles**: Windows auto-allocates one for a console-subsystem
//!     child of a GUI process, which is exactly a packaged launcher spawning
//!     QEMU. Suppressing it only stops a *new* console being allocated; a
//!     release build started from an existing terminal still inherits its
//!     stdio handles and still prints there.
//!   - **Attaching a console**: a Windows release build links as a GUI app and
//!     has nowhere to print, so a command run from a terminal borrows the one
//!     that started it.
//!   - **Acceleration**: KVM on Linux, Hypervisor.framework on macOS, WHPX on
//!     Windows, each behind a runtime probe, falling back to TCG emulation.
//!   - **Opening a URL**: `xdg-open`, `open` and `cmd /c start`, none of which
//!     share a name across platforms.
//!   - **Application menu**: macOS has an app-menu action for starting another
//!     emulator explicitly.
//!   - **Detaching a child**: a process group of its own, so a Ctrl-C meant
//!     for a command's wait does not reach the emulator it started, and on
//!     Windows the flags and handles that keep it off the console.
//!   - **Native console**: a Windows console carries the full palette without
//!     anything in the environment saying so.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::diagnostics::{self, log};

#[cfg(target_os = "macos")]
#[path = "macos_menu.rs"]
mod macos_menu;

/// Install the platform's native actions for starting another emulator.
#[cfg(target_os = "macos")]
pub(crate) fn install_menus(app: &tauri::App) -> anyhow::Result<()> {
    macos_menu::install(app)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn install_menus(_app: &tauri::App) -> anyhow::Result<()> {
    Ok(())
}

/// Strips Windows' `\\?\` extended-length-path ("verbatim") prefix. Tauri's
/// resource resolver canonicalizes paths on Windows, which adds it, and
/// QEMU's mingw-w64 build cannot open files through it. A no-op on other
/// platforms and on paths that never had the prefix, so callers apply it
/// unconditionally.
pub(crate) fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    match path.to_string_lossy().strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// Name of the platform's dynamic-linker library search-path variable.
pub(crate) fn library_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// `dir` prepended onto the current value of [`library_path_var`], pointing a
/// spawned QEMU at its bundled libraries without patching the binaries. The
/// loader consults this search path before a dependency's recorded path, even
/// an absolute one, which is why the fetch scripts leave rpaths alone.
pub(crate) fn prepend_library_path(dir: &Path) -> OsString {
    let existing = std::env::var_os(library_path_var());
    let existing = existing.iter().flat_map(std::env::split_paths);
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(existing))
        .unwrap_or_else(|_| dir.as_os_str().to_owned())
}

/// Stops Windows allocating a console window for a console-subsystem child,
/// which QEMU's official builds are. Only release builds suppress it, so
/// `cargo run` keeps QEMU's serial output in the terminal.
#[cfg(windows)]
pub(crate) fn suppress_child_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    if !cfg!(debug_assertions) {
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
}

#[cfg(not(windows))]
pub(crate) fn suppress_child_console(_cmd: &mut Command) {}

/// Borrow the console that started this process, so that a command typed at a
/// terminal can be answered there. A Windows release build links as a GUI app
/// and is given no console of its own; output that was redirected already has
/// somewhere to go and is left alone.
///
/// Only a command line run calls this. A window run has nothing to print, and
/// an emulator started in the background must stay off the console entirely,
/// since closing a console takes everything attached to it down.
#[cfg(windows)]
pub(crate) fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_UNKNOWN, GetFileType,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };

    // SAFETY: every call is a documented Win32 entry point called with the
    // arguments it documents. The console device names are null-terminated
    // wide strings owned for the length of the call, the two pointer arguments
    // are the nulls CreateFileW documents as "no security attributes" and "no
    // template", and a handle is only ever handed back to Win32.
    unsafe {
        // AttachConsole can overwrite inherited pipes and files unless the
        // parent used STARTF_USESTDHANDLES, so save them before attaching
        let streams = [
            (STD_INPUT_HANDLE, "CONIN$"),
            (STD_OUTPUT_HANDLE, "CONOUT$"),
            (STD_ERROR_HANDLE, "CONOUT$"),
        ]
        .map(|(stream, device)| {
            let handle = GetStdHandle(stream);
            let inherited = (!handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && GetFileType(handle) != FILE_TYPE_UNKNOWN)
                .then_some(handle);
            (stream, device, inherited)
        });
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for (stream, device, inherited) in streams {
            if let Some(handle) = inherited {
                SetStdHandle(stream, handle);
                continue;
            }
            let held = GetStdHandle(stream);
            if !held.is_null() && held != INVALID_HANDLE_VALUE {
                continue;
            }
            let device: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
            let handle = CreateFileW(
                device.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if handle != INVALID_HANDLE_VALUE {
                SetStdHandle(stream, handle);
            }
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn attach_console() {}

/// Put a child in a process group of its own, so that a Ctrl-C at the terminal
/// ends the command's wait and leaves the emulator it started booting.
#[cfg(unix)]
pub(crate) fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

/// The same, plus the flag that keeps Windows from giving a console-subsystem
/// child a console window of its own, with this process's own streams made
/// private first. The child gets null streams, but Windows would still hand it
/// an inheritable copy of these, and a file that the command's output was
/// redirected to would then stay open for as long as the emulator runs.
#[cfg(windows)]
pub(crate) fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: a standard handle this process owns, or null, or the invalid
        // value, is read and then only handed back to Win32 to clear one flag
        // on it; nothing is dereferenced.
        unsafe {
            let handle = GetStdHandle(stream);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Whether this stream is a Windows console, which carries the full palette
/// without anything in the environment saying so.
#[cfg(windows)]
pub(crate) fn native_console(terminal: &console::Term) -> bool {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    let mut mode = 0;
    // SAFETY: a handle this process owns in, a status code out. The call
    // reads nothing through the pointer beyond the mode it writes.
    unsafe { GetConsoleMode(terminal.as_raw_handle(), &mut mode) != 0 }
}

#[cfg(not(windows))]
pub(crate) fn native_console(_terminal: &console::Term) -> bool {
    false
}

/// Hand a URL to whatever the desktop has set as its browser. Spawned and
/// left alone: the launcher has no use for the browser's exit status, and
/// waiting on it would block for as long as the browser runs.
pub(crate) fn open_url(url: &str) -> anyhow::Result<()> {
    let mut cmd = url_opener(url);
    suppress_child_console(&mut cmd);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn url_opener(url: &str) -> Command {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(url);
    cmd
}

#[cfg(target_os = "macos")]
fn url_opener(url: &str) -> Command {
    let mut cmd = Command::new("open");
    cmd.arg(url);
    cmd
}

/// `start` is a `cmd` builtin rather than an executable, and it reads a leading
/// quoted argument as the new window's title, so it gets an empty one before
/// the URL or the URL itself would be swallowed as the title.
#[cfg(windows)]
fn url_opener(url: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/c", "start", "", url]);
    cmd
}

/// QEMU `-accel` flags for this host. Cross-architecture guests get none,
/// since only a native guest can use hardware acceleration and TCG is QEMU's
/// default anyway.
pub(crate) fn accel_flags(native: bool) -> &'static [&'static str] {
    let flags: &'static [&'static str] = if native { native_accel_flags() } else { &[] };
    // The first entry is the accelerator actually asked for; anything after it
    // is QEMU's own fallback list. An empty list means plain TCG by default.
    diagnostics::record("Accel", *flags.get(1).unwrap_or(&"tcg"));
    flags
}

#[cfg(target_os = "linux")]
fn native_accel_flags() -> &'static [&'static str] {
    if kvm_available() {
        &["-accel", "kvm", "-accel", "tcg"]
    } else {
        log!(
            "[launcher] /dev/kvm is not accessible; falling back to software \
             emulation, which will be slower. Access normally comes from \
             membership of the kvm group."
        );
        &["-accel", "tcg"]
    }
}

/// Whether KVM is usable, meaning `/dev/kvm` exists and this user may open it
/// for reading and writing, which normally comes from the `kvm` group.
///
/// Probed for the same reason as [`hvf_available`] and [`whpx_probe`]: QEMU's
/// `-accel kvm,tcg` list is not the clean fallback it looks like, and a
/// refused `/dev/kvm` is much the most common way for a Linux host to end up
/// without acceleration.
#[cfg(target_os = "linux")]
fn kvm_available() -> bool {
    // SAFETY: a null-terminated literal in, a status code out. `access` reads
    // nothing through the pointer beyond the string itself.
    unsafe { libc::access(c"/dev/kvm".as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

#[cfg(target_os = "macos")]
fn native_accel_flags() -> &'static [&'static str] {
    if hvf_available() {
        &["-accel", "hvf", "-accel", "tcg"]
    } else {
        log!(
            "[launcher] Hypervisor.framework unavailable on this Mac; \
             falling back to software emulation, which will be slower"
        );
        &["-accel", "tcg"]
    }
}

#[cfg(windows)]
fn native_accel_flags() -> &'static [&'static str] {
    match whpx_probe() {
        Ok(()) => &["-accel", "whpx", "-accel", "tcg"],
        Err(reason) => {
            log!(
                "[launcher] Windows Hypervisor Platform unusable ({reason:#}); falling \
                 back to software emulation, which will be slower. If the feature is \
                 simply switched off, enable it with: DISM /online /Enable-Feature \
                 /FeatureName:HypervisorPlatform /All"
            );
            &["-accel", "tcg"]
        }
    }
}

/// Unlike the `orphan` module, an unhandled target here is harmless: it just
/// means TCG, which is QEMU's default. The crate still refuses to build on a
/// fourth OS, because `orphan` has no such catch-all.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn native_accel_flags() -> &'static [&'static str] {
    &[]
}

/// Whether Hypervisor.framework is usable, via `sysctl kern.hv_support`. QEMU
/// treats a real `hv_vm_create()` failure as fatal rather than falling
/// through its `-accel hvf,tcg` list, so the list alone cannot be trusted.
/// Most commonly false when nested inside another hypervisor, which Apple
/// does not support.
#[cfg(target_os = "macos")]
fn hvf_available() -> bool {
    Command::new("sysctl")
        .args(["-n", "kern.hv_support"])
        .output()
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "1")
        .unwrap_or(false)
}

/// Whether Windows Hypervisor Platform is usable, established by building the
/// same partition QEMU will build and then tearing it down again. The error
/// carries the reason it is not, for the caller to report.
///
/// Asking `WHvGetCapability(WHvCapabilityCodeHypervisorPresent)` on its own is
/// not enough, and that is not a theoretical gap: a Windows guest inside
/// another hypervisor answers that a hypervisor is present, and only refuses
/// once the partition is actually configured. QEMU is not trusted to fall
/// through its `-accel whpx,tcg` list for the same reason as
/// [`hvf_available`], and it demonstrably does not. It reports falling back to
/// TCG, keeps the WHPX interrupt controller it had already selected, and then
/// dies calling into it.
///
/// Nested virtualization is requested here because QEMU requests it for this
/// guest and treats a refusal as fatal. A probe that skipped it would succeed
/// on exactly the machines where QEMU still fails, which is the whole
/// scenario this exists to catch.
///
/// Loaded dynamically because `WinHvPlatform.dll` is absent entirely when the
/// feature is disabled: a static import would fail at process load and the
/// launcher would never start.
#[cfg(windows)]
fn whpx_probe() -> anyhow::Result<()> {
    use std::ffi::c_void;

    use anyhow::bail;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type GetCapability = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;
    type CreatePartition = unsafe extern "system" fn(*mut *mut c_void) -> i32;
    type SetProperty = unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> i32;
    type WithPartition = unsafe extern "system" fn(*mut c_void) -> i32;

    // WHV_CAPABILITY_CODE and WHV_PARTITION_PROPERTY_CODE values, from
    // WinHvPlatformDefs.h. Spelled out rather than pulled from a binding
    // crate, to keep this on the same dynamic-load path as the calls.
    const HYPERVISOR_PRESENT: u32 = 0x0000_0000;
    const NESTED_VIRTUALIZATION: u32 = 0x0000_0004;
    const PROCESSOR_COUNT: u32 = 0x0000_1fff;

    let dll: Vec<u16> = "WinHvPlatform.dll".encode_utf16().chain(Some(0)).collect();

    // SAFETY: the module handle is deliberately leaked rather than freed, so
    // the resolved pointers stay valid for the calls below. Each is a standard
    // Win32 dynamic-load or WHPX entry point called with a null-terminated
    // name and the signature it is documented with; every property buffer is
    // sized from the type being read out of it; and the partition handle is
    // only ever handed back to WHPX, on every path including the failing ones.
    unsafe {
        let module = LoadLibraryW(dll.as_ptr());
        if module.is_null() {
            bail!("WinHvPlatform.dll is not installed");
        }

        macro_rules! entry_point {
            ($name:literal, $signature:ty) => {
                match GetProcAddress(module, concat!($name, "\0").as_ptr()) {
                    Some(symbol) => std::mem::transmute::<_, $signature>(symbol),
                    None => bail!("WinHvPlatform.dll exports no {}", $name),
                }
            };
        }

        let get_capability = entry_point!("WHvGetCapability", GetCapability);
        let create_partition = entry_point!("WHvCreatePartition", CreatePartition);
        let set_property = entry_point!("WHvSetPartitionProperty", SetProperty);
        let setup_partition = entry_point!("WHvSetupPartition", WithPartition);
        let delete_partition = entry_point!("WHvDeletePartition", WithPartition);

        // Cheap early out: the feature is off by default, and there is no
        // point building a partition to discover that.
        let mut present: u32 = 0;
        let mut written: u32 = 0;
        let hr = get_capability(
            HYPERVISOR_PRESENT,
            std::ptr::addr_of_mut!(present).cast(),
            std::mem::size_of::<u32>() as u32,
            &mut written,
        );
        if hr < 0 || written as usize != std::mem::size_of::<u32>() || present == 0 {
            bail!("no hypervisor present");
        }

        let mut partition: *mut c_void = std::ptr::null_mut();
        let hr = create_partition(&mut partition);
        if hr < 0 {
            bail!("WHvCreatePartition failed, hr={hr:08x}");
        }

        let processors: u32 = 1;
        let hr = set_property(
            partition,
            PROCESSOR_COUNT,
            std::ptr::addr_of!(processors).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            delete_partition(partition);
            bail!("processor count refused, hr={hr:08x}");
        }

        let nested: u32 = 1;
        let hr = set_property(
            partition,
            NESTED_VIRTUALIZATION,
            std::ptr::addr_of!(nested).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        if hr < 0 {
            delete_partition(partition);
            bail!("nested virtualization unavailable, hr={hr:08x}");
        }

        let hr = setup_partition(partition);
        if hr < 0 {
            delete_partition(partition);
            bail!("WHvSetupPartition failed, hr={hr:08x}");
        }

        delete_partition(partition);
        Ok(())
    }
}
