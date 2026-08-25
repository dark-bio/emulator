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
//!   - **Acceleration**: KVM on Linux, Hypervisor.framework on macOS, WHPX on
//!     Windows, each behind a runtime probe, falling back to TCG emulation.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// QEMU `-accel` flags for this host. Cross-architecture guests get none,
/// since only a native guest can use hardware acceleration and TCG is QEMU's
/// default anyway.
pub(crate) fn accel_flags(native: bool) -> &'static [&'static str] {
    if native {
        native_accel_flags()
    } else {
        &[]
    }
}

#[cfg(target_os = "linux")]
fn native_accel_flags() -> &'static [&'static str] {
    &["-accel", "kvm", "-accel", "tcg"]
}

#[cfg(target_os = "macos")]
fn native_accel_flags() -> &'static [&'static str] {
    if hvf_available() {
        &["-accel", "hvf", "-accel", "tcg"]
    } else {
        eprintln!(
            "[launcher] Hypervisor.framework unavailable on this Mac; \
             falling back to software emulation, which will be slower"
        );
        &["-accel", "tcg"]
    }
}

#[cfg(windows)]
fn native_accel_flags() -> &'static [&'static str] {
    if whpx_available() {
        &["-accel", "whpx", "-accel", "tcg"]
    } else {
        eprintln!(
            "[launcher] Windows Hypervisor Platform unavailable; falling back \
             to software emulation, which will be slower. Enable it with: \
             DISM /online /Enable-Feature /FeatureName:HypervisorPlatform /All"
        );
        &["-accel", "tcg"]
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

/// Whether Windows Hypervisor Platform is usable, via
/// `WHvGetCapability(WHvCapabilityCodeHypervisorPresent)`. The feature is off
/// by default, and QEMU is not trusted to fall through its `-accel whpx,tcg`
/// list for the same reason as [`hvf_available`], so every failure path here
/// returns false and the caller emits plain TCG.
///
/// Loaded dynamically because `WinHvPlatform.dll` is absent entirely when the
/// feature is disabled: a static import would fail at process load and the
/// launcher would never start.
#[cfg(windows)]
fn whpx_available() -> bool {
    use std::ffi::c_void;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type GetCapability = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;
    const HYPERVISOR_PRESENT: u32 = 0;

    let dll: Vec<u16> = "WinHvPlatform.dll".encode_utf16().chain(Some(0)).collect();

    // SAFETY: the module handle is deliberately leaked rather than freed, so
    // the resolved pointer stays valid for the call. Both are standard Win32
    // dynamic-load calls with null-terminated names, and the capability
    // buffer is sized from the type being written into it.
    unsafe {
        let module = LoadLibraryW(dll.as_ptr());
        if module.is_null() {
            return false;
        }
        let Some(symbol) = GetProcAddress(module, c"WHvGetCapability".as_ptr() as *const u8) else {
            return false;
        };
        let get_capability: GetCapability = std::mem::transmute(symbol);

        let mut present: u32 = 0;
        let mut written: u32 = 0;
        let hr = get_capability(
            HYPERVISOR_PRESENT,
            std::ptr::addr_of_mut!(present).cast(),
            std::mem::size_of::<u32>() as u32,
            &mut written,
        );
        hr >= 0 && written as usize == std::mem::size_of::<u32>() && present != 0
    }
}
