// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! A failure as both outputs render it: a stable code, which is what a caller
//! matches on, the exit class that code belongs to, a sentence saying what
//! went wrong, and the hints that say what to do next.

/// Every code a command can exit with. Each one names its exit class here and
/// nowhere else, so the class is written once per code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Code {
    /// The arguments do not make sense, or a help topic does not exist.
    Usage,
    /// A question that could not be asked was not answered with its flag.
    ConfirmationRequired,
    /// The named image is not there.
    DiskMissing,
    /// A file could not be read or written.
    Io,
    /// This build carries no firmware for the architecture asked for.
    FirmwareMissing,
    /// The QEMU this build would run could not be run.
    QemuMissing,
    /// The guest would run under software emulation.
    NoAcceleration,
    /// Every port in the range is taken.
    PortExhausted,
    /// QEMU died while the emulator was starting.
    StoppedUnexpectedly,
    /// A bare run could not bring the emulator up.
    CouldNotStart,
    /// An emulator is booted from that image.
    DiskBusy,
    /// Nothing running matches the selector.
    NoEmulator,
    /// Several running emulators match the selector, or none was named
    /// while several run.
    AmbiguousEmulator,
    /// Something answered on the registry's port and could not be read.
    RegistryUnreachable,
    /// The launcher did not advertise direct button control.
    ControlUnsupported,
    /// The launcher's direct endpoint could not be reached or understood.
    ControlUnreachable,
    /// Hardware disconnected, restarted or could not accept a button input.
    ButtonUnavailable,
    /// A wait on a machine ran out.
    Timeout,
}

impl Code {
    /// Every code, in the order the output topic lists them, which the test
    /// on that topic walks.
    #[cfg(test)]
    pub(crate) const ALL: [Code; 18] = [
        Code::Usage,
        Code::ConfirmationRequired,
        Code::DiskMissing,
        Code::Io,
        Code::FirmwareMissing,
        Code::QemuMissing,
        Code::NoAcceleration,
        Code::PortExhausted,
        Code::StoppedUnexpectedly,
        Code::CouldNotStart,
        Code::DiskBusy,
        Code::NoEmulator,
        Code::AmbiguousEmulator,
        Code::RegistryUnreachable,
        Code::ControlUnsupported,
        Code::ControlUnreachable,
        Code::ButtonUnavailable,
        Code::Timeout,
    ];

    /// The code as a caller reads it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Code::Usage => "usage",
            Code::ConfirmationRequired => "confirmation-required",
            Code::DiskMissing => "disk-missing",
            Code::Io => "io",
            Code::FirmwareMissing => "firmware-missing",
            Code::QemuMissing => "qemu-missing",
            Code::NoAcceleration => "no-acceleration",
            Code::PortExhausted => "port-exhausted",
            Code::StoppedUnexpectedly => "stopped-unexpectedly",
            Code::CouldNotStart => "could-not-start",
            Code::DiskBusy => "disk-busy",
            Code::NoEmulator => "no-emulator",
            Code::AmbiguousEmulator => "ambiguous-emulator",
            Code::RegistryUnreachable => "registry-unreachable",
            Code::ControlUnsupported => "control-unsupported",
            Code::ControlUnreachable => "control-unreachable",
            Code::ButtonUnavailable => "button-unavailable",
            Code::Timeout => "timeout",
        }
    }

    /// The exit class, one of the numbers the house tools share: 1 for a
    /// local file or a confirmation, 2 for usage, 3 for an emulator that
    /// cannot be reached, 7 for a wait that ran out.
    pub(crate) fn exit(self) -> i32 {
        match self {
            Code::Usage => 2,
            Code::ConfirmationRequired
            | Code::DiskMissing
            | Code::Io
            | Code::FirmwareMissing
            | Code::QemuMissing
            | Code::NoAcceleration
            | Code::PortExhausted
            | Code::StoppedUnexpectedly
            | Code::CouldNotStart => 1,
            Code::DiskBusy
            | Code::NoEmulator
            | Code::AmbiguousEmulator
            | Code::RegistryUnreachable
            | Code::ControlUnsupported
            | Code::ControlUnreachable
            | Code::ButtonUnavailable => 3,
            Code::Timeout => 7,
        }
    }
}

/// A failure, in the shape both outputs render it from.
#[derive(Debug)]
pub(crate) struct Error {
    /// What went wrong, by its stable code.
    pub(crate) code: Code,

    /// What went wrong, in a sentence.
    pub(crate) message: String,

    /// What to do next, wherever the tool knows.
    pub(crate) hints: Vec<String>,
}

impl Error {
    /// A failure with no next step to name.
    pub(crate) fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hints: Vec::new(),
        }
    }

    /// A failure in this computer's own files, which is the exit class a
    /// caller fixes locally.
    pub(crate) fn io(error: impl std::fmt::Display) -> Self {
        Self::new(Code::Io, error.to_string())
    }

    /// The same failure, with one more line saying what to do about it.
    pub(crate) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }

    /// The exit code this failure ends the run with.
    pub(crate) fn exit(&self) -> i32 {
        self.code.exit()
    }

    /// The error object, which is what `--json` carries.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::json!({"code": self.code.name(), "message": self.message})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_every_code_has_a_distinct_name() {
        let mut names: Vec<&str> = Code::ALL.iter().map(|code| code.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Code::ALL.len());
    }
}
