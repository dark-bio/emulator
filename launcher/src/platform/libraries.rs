// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dynamic library search paths for bundled QEMU processes.

use std::ffi::OsString;
use std::path::Path;

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
