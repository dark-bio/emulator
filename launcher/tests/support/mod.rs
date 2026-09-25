// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Selects the executable used by local and packaged CLI tests.

use std::path::PathBuf;

/// Use an explicit artifact path or the executable built by Cargo for these tests.
pub fn executable() -> PathBuf {
    std::env::var_os("ARK_EMULATOR_TEST_EXE")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_ark-emulator").map(PathBuf::from))
        .expect("set ARK_EMULATOR_TEST_EXE to the packaged executable's absolute path")
}
