// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Prepares Tauri, and stamps the version `ark-emulator --version` prints.
//!
//! The release workflow sets `ARK_EMULATOR_VERSION` for the binaries it
//! publishes. Any other build is a local one, marked by a `-local` suffix and
//! the `emulator_local` cfg, which turns the release lookup off.

use std::env;

/// Sets `ARK_EMULATOR_VERSION` for the crate, and `emulator_local` for a
/// local build.
fn main() {
    tauri_build::build();

    // Rerun when the workflow's stamp changes
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=ARK_EMULATOR_VERSION");
    println!("cargo::rustc-check-cfg=cfg(emulator_local)");
    let version = env::var("ARK_EMULATOR_VERSION").unwrap_or_else(|_| {
        let version = env::var("CARGO_PKG_VERSION").expect("Cargo sets the package version");
        println!("cargo::rustc-cfg=emulator_local");
        format!("{version}-local")
    });
    println!("cargo::rustc-env=ARK_EMULATOR_VERSION={version}");
}
