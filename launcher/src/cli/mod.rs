// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Parses commands, renders terminal output and runs headless emulators.

pub(crate) mod args;
pub(crate) mod commands;
mod doctor;
pub(crate) mod help;
pub(crate) mod output;
mod style;

use anyhow::Result;

use crate::bundle::Paths;
use crate::error::{Code, Error};
use crate::platform;
use crate::runtime::disk::Resolved;
use crate::runtime::qemu::ensure_disk;
use crate::runtime::{self, Runtime};
use args::Boot;
use output::Output;

/// Run in the foreground until QEMU exits or shutdown is requested.
pub(crate) fn headless(paths: &Paths, boot: Boot, output: Output) -> Result<()> {
    let runtime = Runtime::new()?;
    let (pending, settings, resolved) = runtime::prepare(paths, boot, true)?;
    let Resolved::Boot(disk) = resolved else {
        unreachable!("headless disk selection never asks for input")
    };
    let (memory, env) = runtime::effective(Some(&pending.boot), &settings);
    ensure_disk(&disk, pending.qemu_libs.as_deref())?;
    runtime.launch(pending, &disk, memory, &env, move |result| match result {
        Ok(status) => {
            runtime::shut_down(platform::interrupted(status).or(status.code()).unwrap_or(0))
        }
        Err(err) => {
            output.error(&Error::new(Code::StoppedUnexpectedly, format!("{err:#}")));
            runtime::shut_down(1);
        }
    })?;
    loop {
        std::thread::park();
    }
}
