// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Names the shipped runtimes and their build and execution hosts.

use anyhow::{Result, bail};
use clap::ValueEnum;

/// A native guest and the operating system hosting its QEMU executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Target {
    /// Linux with the q35 machine and KVM acceleration.
    LinuxAmd64,
    /// Intel macOS with the q35 machine and Hypervisor.framework.
    MacosAmd64,
    /// Apple Silicon with the virt machine and Hypervisor.framework.
    MacosArm64,
    /// Windows with the q35 machine and Windows Hypervisor Platform.
    WindowsAmd64,
}

impl Target {
    /// Select the runtime that executes on this build helper's host.
    pub(crate) fn native() -> Result<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Ok(Self::LinuxAmd64),
            ("macos", "x86_64") => Ok(Self::MacosAmd64),
            ("macos", "aarch64") => Ok(Self::MacosArm64),
            ("windows", "x86_64") => Ok(Self::WindowsAmd64),
            _ => bail!("unsupported QEMU build host"),
        }
    }

    /// Return the platform name used in release artifacts.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::MacosAmd64 => "macos-amd64",
            Self::MacosArm64 => "macos-arm64",
            Self::WindowsAmd64 => "windows-amd64",
        }
    }

    /// Return the public architecture used by the device and ROM allowlists.
    pub(crate) fn arch(self) -> &'static str {
        if self == Self::MacosArm64 {
            "arm64"
        } else {
            "amd64"
        }
    }

    /// Return the CPU name used by QEMU's target configuration.
    pub(crate) fn cpu(self) -> &'static str {
        if self == Self::MacosArm64 {
            "aarch64"
        } else {
            "x86_64"
        }
    }

    /// Return Tauri's sidecar target suffix.
    pub(crate) fn triple(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "x86_64-unknown-linux-gnu",
            Self::MacosAmd64 => "x86_64-apple-darwin",
            Self::MacosArm64 => "aarch64-apple-darwin",
            Self::WindowsAmd64 => "x86_64-pc-windows-msvc",
        }
    }

    /// Return the executable filename extension on the target system.
    pub(crate) fn extension(self) -> &'static str {
        if self == Self::WindowsAmd64 {
            ".exe"
        } else {
            ""
        }
    }

    /// Whether the runtime builds natively with Apple's SDK.
    pub(crate) fn is_macos(self) -> bool {
        matches!(self, Self::MacosAmd64 | Self::MacosArm64)
    }
}
