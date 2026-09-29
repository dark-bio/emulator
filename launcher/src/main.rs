// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dispatches commands and starts the headless or desktop emulator.
//!
//! [`runtime`] owns the guest lifecycle and hardware socket. [`desktop`]
//! adapts hardware state and user inputs for the webview. Both launch modes
//! use the same disk selection, QEMU, registry and shutdown paths.
//! Management commands in [`cli::commands`] return before either mode starts.
//!
//! The backing image is an unencrypted qcow2 file. The emulator is for
//! development and demos; real data belongs on hardware.

// A packaged Windows app borrows a console only for command line use
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bundle;
mod cli;
mod desktop;
mod diagnostics;
mod error;
mod ipc;
mod platform;
mod runtime;
mod settings;
mod update;

use clap::{FromArgMatches as _, Parser};

use bundle::Paths;
use cli::args::Cli;
use cli::{commands, help, output};
use error::{Code, Error};

/// Render clap's complaint in the command line's error format.
fn usage(error: &clap::Error) -> Error {
    let message = error.to_string();
    let message = message
        .split("\n\n")
        .next()
        .unwrap_or(&message)
        .trim_start_matches("error: ")
        .trim();
    Error::new(Code::Usage, message).hint("`ark-emulator help` lists the commands and the topics")
}

/// Dispatch management commands before creating a runtime or a window.
fn main() {
    // Handle the detached copy before parsing or initializing command or window state
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() == 2 && arguments[1] == update::ENTRY_POINT {
        update::run();
        return;
    }

    // Parse through the help tree before attaching a console for command output
    let json = arguments
        .iter()
        .skip(1)
        .take_while(|argument| *argument != "--")
        .any(|argument| argument == "--json");
    let matches = match help::parser().try_get_matches_from_mut(&arguments) {
        Ok(matches) => matches,
        Err(error) => {
            platform::attach_console();
            if error.exit_code() == 0 {
                let _ = error.print();
                std::process::exit(0);
            }
            let mut global = Cli::parse_from(["ark-emulator"]).global;
            global.json = json;
            output::Output::new(&global).error(&usage(&error));
            std::process::exit(2);
        }
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| {
        platform::attach_console();
        let _ = error.print();
        std::process::exit(2);
    });
    if cli.version || cli.command.is_some() || cli.boot.headless {
        platform::attach_console();
    }
    let output = output::Output::new(&cli.global);
    desktop::reporting(&output, cli.global.no_input || cli.boot.headless);
    diagnostics::level(cli.global.log);

    // This reads compiled metadata without initializing a window system
    let context = tauri::generate_context!();
    if let Err(err) = cli.validate() {
        output.error(&err);
        std::process::exit(err.exit());
    }
    if cli.version || cli.command.is_some() {
        std::process::exit(commands::run(
            cli.command,
            &cli.global,
            &output,
            &context.config().identifier,
            context.package_info(),
        ));
    }

    // A source build gives stdout to the guest console in either launch mode
    output.release_stdout();
    if output.json() {
        diagnostics::log_sink(diagnostics::Sink::Events(output.clone()));
    }
    if cli.boot.headless {
        update::start(&output, time::OffsetDateTime::now_utc());
        let result = Paths::resolve(&context.config().identifier, context.package_info())
            .and_then(|paths| cli::headless(&paths, cli.boot, output.clone()));
        if let Err(err) = result {
            diagnostics::log!("[launcher] could not start: {err:#}");
            output.error(&Error::io(format!("{err:#}")));
            runtime::shut_down(1);
        }
        return;
    }

    desktop::run(context, cli.boot, cli.global.no_input, &output);
}
