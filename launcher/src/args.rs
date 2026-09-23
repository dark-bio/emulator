// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The command line as typed: the options every run takes, what a guest boots
//! from, and the commands that manage emulators instead of opening a window.
//!
//! The types here are parsed through the help tree, so that the pages the
//! manual prints and the arguments a run accepts are one and the same thing.

use std::path::PathBuf;

use clap::Parser;

use crate::error::{Code, Error};
use crate::qemu::GuestArch;
use crate::settings;

/// What the tool is, which opens every help page.
pub(crate) const ABOUT: &str = "Emulated Ark enclave for development and demos\n\n\
     An emulated Ark runs the real firmware in QEMU, with an optional window \
     for the device's face. It is not a vault, since everything \
     lives in one plain disk image on this computer, so real data belongs on \
     hardware. Talk to it with `ark` from https://github.com/dark-bio/cli, \
     exactly as you would to hardware, where the owner approves on their phone \
     in Ark Companion.";

/// Longest wait for a machine, in seconds. One number for the whole tool, so
/// there is one to remember, and it is generous enough to cover a boot with no
/// hardware acceleration behind it.
pub(crate) const DEFAULT_TIMEOUT: u64 = 120;

/// The command line as parsed. A bare run opens the device window; a command
/// manages emulators without one.
#[derive(Parser)]
// The help flag is added by the help renderer, on the root and on every
// command, so that its line can say where the manual is.
#[command(
    name = "ark-emulator",
    about = ABOUT,
    disable_help_flag = true,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Everything a guest needs to be booted.
    #[command(flatten)]
    pub(crate) boot: Boot,

    /// The options that apply whatever is being run.
    #[command(flatten)]
    pub(crate) global: Global,

    /// Emulator, bundled firmware and QEMU versions
    #[arg(short = 'V', long)]
    pub(crate) version: bool,

    /// What to do, or nothing at all, which opens the device window.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

impl Cli {
    /// Reject the combinations clap cannot express. The boot options belong
    /// to the bare run and to `start`, so naming one beside a command is a
    /// mistake rather than something to guess at.
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let message = if self.global.quiet && self.global.verbose {
            "--quiet cannot be combined with --verbose"
        } else if self.version && self.command.is_some() {
            "--version cannot be combined with a command"
        } else if self.command.is_some() && self.boot.named() {
            "the boot options belong to a bare run or to `ark-emulator start`"
        } else {
            return Ok(());
        };
        Err(Error::new(Code::Usage, message))
    }
}

/// The options every command carries, spelled the way the house tools spell
/// them. They are accepted at any level, so `--no-input` before or after a
/// command name means the same thing.
#[derive(clap::Args)]
pub(crate) struct Global {
    /// Print results as JSON and events as JSON Lines
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Whole wait for a start or a stop; each network wait in doctor
    #[arg(long, global = true, default_value_t = DEFAULT_TIMEOUT, value_name = "SECONDS", value_parser = parse_timeout)]
    pub(crate) timeout: u64,

    /// Never ask; take the default image and exit on failure
    // A launch with nobody at the keyboard, such as a test run, takes the
    // default image instead of asking where to keep one, and a failure prints
    // its report and exits instead of opening a window.
    #[arg(long, global = true)]
    pub(crate) no_input: bool,

    /// Diagnostics: debug, or trace with the registry too
    #[arg(
        long,
        global = true,
        value_name = "LEVEL",
        value_enum,
        hide_possible_values = true
    )]
    pub(crate) log: Option<Log>,

    /// Hide progress and notes; keep errors and hints
    #[arg(short = 'q', long, global = true)]
    pub(crate) quiet: bool,

    /// Show steps
    #[arg(short = 'v', long, global = true)]
    pub(crate) verbose: bool,
}

/// How much diagnostic detail a run asks for, independent of step narration.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub(crate) enum Log {
    /// The launcher's own lines.
    Debug,
    /// Those and every request to the registry.
    Trace,
}

/// Reject a wait that cannot be waited out, or one so long that a deadline
/// cannot be computed from it.
fn parse_timeout(value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(seconds) if seconds > 0 && seconds <= MAX_TIMEOUT => Ok(seconds),
        _ => Err(format!(
            "the timeout is a number of seconds from 1 to {MAX_TIMEOUT}"
        )),
    }
}

/// A year, which is as long as any wait could sensibly be asked for.
const MAX_TIMEOUT: u64 = 365 * 24 * 60 * 60;

/// A port an emulator can hold, which is any but the one that means none.
fn parse_port(value: &str) -> Result<u16, String> {
    match value.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err("the port is a number from 1 to 65535".to_owned()),
    }
}

/// What an emulator boots from, shared by the bare run and by the command that
/// boots one in the background.
#[derive(clap::Args)]
pub(crate) struct Boot {
    /// Run without a window or prompts
    #[arg(long)]
    pub(crate) headless: bool,

    /// Image to boot, created if missing
    // Read for this run only. It neither consults nor updates the settings
    // file, so a one-off boot from another image leaves the remembered choice
    // alone.
    #[arg(long, value_name = "PATH")]
    pub(crate) image: Option<PathBuf>,

    /// Cloud environment for a new image
    // An existing image keeps the environment it was created with, since the
    // firmware burns that binding in on its first boot.
    #[arg(long, value_name = "ENV", value_parser = settings::ENVS, hide_possible_values = true)]
    pub(crate) env: Option<String>,

    /// Guest RAM in MiB
    // Lower it on a machine with little memory to spare. The remembered
    // amount answers when this does not.
    #[arg(long, value_name = "MIB")]
    pub(crate) memory: Option<u32>,

    /// Firmware architecture: arm64 or amd64
    // This computer's own by default. It is the only one that gets hardware
    // acceleration, and the only one a packaged build carries a QEMU for.
    #[arg(long, value_name = "ARCH", value_enum, hide_possible_values = true)]
    pub(crate) arch: Option<GuestArch>,

    /// Kernel image, with --initrd
    // A source build bundles no firmware, so the two are its only way to boot.
    #[arg(long, value_name = "PATH", requires = "initrd")]
    pub(crate) kernel: Option<PathBuf>,

    /// Initramfs, with --kernel
    #[arg(long, value_name = "PATH", requires = "kernel")]
    pub(crate) initrd: Option<PathBuf>,

    /// Port to hold; the first free from 18181 otherwise
    // The guest's own port is fixed, so this is the host side of the forward
    // and the number an emulator is known by.
    #[arg(long, value_name = "PORT", value_parser = parse_port)]
    pub(crate) port: Option<u16>,
}

impl Boot {
    /// Whether any of these was typed.
    pub(crate) fn named(&self) -> bool {
        self.headless
            || self.image.is_some()
            || self.env.is_some()
            || self.memory.is_some()
            || self.arch.is_some()
            || self.kernel.is_some()
            || self.initrd.is_some()
            || self.port.is_some()
    }
}

/// What the command line can be asked to do instead of opening a window.
#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Boot an emulator and print its locator once it is ready
    Start {
        /// The image, environment, memory and firmware the emulator boots on.
        #[command(flatten)]
        boot: Boot,
    },

    /// Show the emulators running on this computer
    List,

    /// Shut an emulator down, like closing its window
    Stop {
        /// Locator, serial, name or image, as ark -d takes it; the only one otherwise
        #[arg(value_name = "EMULATOR", conflicts_with = "all")]
        emulator: Option<String>,

        /// Stop every emulator on this computer
        #[arg(long)]
        all: bool,
    },

    /// Reset a stopped image so its next boot is a fresh device
    Wipe {
        /// Image to reset; the image start would boot otherwise
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,

        /// Reset without being asked to confirm
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Check this computer and this build; suggest fixes
    Doctor,

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_name = "SHELL", value_enum)]
        shell: clap_complete::Shell,
    },

    /// Help for a command or a topic
    Help {
        /// Command or topic to explain: agents, output, images, registry
        #[arg(value_name = "COMMAND_OR_TOPIC")]
        name: Option<String>,

        /// Print the whole manual: every command page and every topic
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_timeout_is_a_positive_number_of_seconds_with_a_ceiling() {
        assert_eq!(parse_timeout("120"), Ok(120));
        assert!(parse_timeout("0").is_err());
        assert!(parse_timeout("-1").is_err());
        assert!(parse_timeout(&u64::MAX.to_string()).is_err());
    }

    #[test]
    fn test_a_port_is_a_number_an_emulator_can_hold() {
        assert_eq!(parse_port("18181"), Ok(18181));
        for bad in ["0", "65536", "127.0.0.1:18181", "port"] {
            assert!(parse_port(bad).is_err(), "{bad}");
        }
    }
}
