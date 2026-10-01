// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Native IPC regressions run on each packaging platform without a guest.

#[path = "../src/ipc/local.rs"]
#[allow(dead_code)] // Standalone transport tests use a subset of the application API.
mod local;
