// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Publishes running emulators and carries discovery, shutdown and button requests.

pub(crate) mod control;
pub(crate) mod discovery;
pub(crate) mod registry;

#[cfg(test)]
pub(crate) mod testing;
