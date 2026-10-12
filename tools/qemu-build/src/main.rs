// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Builds and checks the Linux amd64 QEMU runtime in an Ubuntu container.
//!
//! `make qemu-linux` produces sidecars, licenses, a manifest and matching
//! sources under `target/qemu-linux`, and stages the runtime for Tauri. The
//! source submodule stays untouched. Docker, Git, tar and Rust are required.
//!
//! The Dockerfile pins Ubuntu 22.04 and a package snapshot. The device and
//! feature lists retain q35, KVM, TCG, qcow2, SLIRP and virtio serial sockets.
//! Packaging rejects libraries outside `linux.libs` and collects their source
//! packages, ROM sources and notices. `make qemu-linux-check` verifies hashes,
//! library resolution and live disk operations in a bare Ubuntu container,
//! then stages the checked runtime. Firmware boot checks run on the final
//! AppImage in CI. Corresponding sources include standalone rebuild instructions.

mod bundle;
mod check;
mod source;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};

/// Arguments for the build helper invoked by Make.
#[derive(Parser)]
struct Args {
    /// Operation to perform.
    #[command(subcommand)]
    operation: Operation,
}

/// Build and verification entry points.
#[derive(Subcommand)]
enum Operation {
    /// Build and stage the Linux runtime and corresponding source archive.
    Build {
        /// Emulator repository containing the pinned QEMU submodule.
        #[arg(long)]
        repo: PathBuf,
        /// Directory for sources, build products and release files.
        #[arg(long)]
        output: PathBuf,
        /// Maximum number of compiler processes.
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        /// Extracted corresponding-source directory to rebuild without Git.
        #[arg(long)]
        sources: Option<PathBuf>,
    },
    /// Verify the isolated runtime and stage it for Tauri packaging.
    Check {
        /// Emulator repository containing the build recipes.
        #[arg(long)]
        repo: PathBuf,
        /// Directory containing the completed runtime.
        #[arg(long)]
        output: PathBuf,
    },
}

/// Paths and container identity shared by the build stages.
pub(crate) struct Build {
    /// Canonical emulator repository path.
    repo: PathBuf,
    /// Canonical directory for generated files.
    output: PathBuf,
    /// Docker image containing the pinned compiler and development libraries.
    image: String,
    /// Bare Ubuntu image used to detect dependencies missing from the bundle.
    base_image: String,
    /// Exclusive lock preventing concurrent builds from changing the same output.
    _lock: fs::File,
}

/// Report build errors without losing the failed command's context.
fn main() -> Result<()> {
    match Args::parse().operation {
        Operation::Build {
            repo,
            output,
            jobs,
            sources,
        } => {
            ensure!(jobs > 0, "--jobs must be positive");
            let build = Build::new(&repo, &output)?;
            build.toolchain()?;
            if let Some(sources) = sources {
                source::restore(&build, &sources)?;
            } else {
                source::prepare(&build)?;
            }
            build.compile(jobs)?;
            bundle::package(&build)?;
            check::runtime(&build)?;
            bundle::stage(&build)?;
        }
        Operation::Check { repo, output } => {
            let build = Build::new(&repo, &output)?;
            check::runtime(&build)?;
            bundle::stage(&build)?;
        }
    }
    Ok(())
}

impl Build {
    /// Resolve paths and reject unsupported build hosts.
    fn new(repo: &Path, output: &Path) -> Result<Self> {
        ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64")),
            "this builder requires Linux amd64"
        );
        fs::create_dir_all(output)?;
        let repo = repo.canonicalize()?;
        let output = output.canonicalize()?;
        ensure!(
            output != repo && !repo.starts_with(&output),
            "output must not contain the repository"
        );
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(output.join(".lock"))?;
        lock.try_lock()
            .context("another QEMU build or check is using this output directory")?;
        let dockerfile = fs::read(repo.join(".github/packaging/qemu/linux.Dockerfile"))?;
        let text = std::str::from_utf8(&dockerfile)?;
        let base_image = text
            .lines()
            .find_map(|line| line.strip_prefix("FROM "))
            .context("Dockerfile has no base image")?
            .to_owned();
        let image = format!("ark-qemu-linux:{}", &bundle::digest(&dockerfile)[..16]);
        Ok(Self {
            repo,
            output,
            image,
            base_image,
            _lock: lock,
        })
    }

    /// Build the compiler container from its pinned base and package snapshot.
    fn toolchain(&self) -> Result<()> {
        run(Command::new("docker")
            .args([
                "build",
                "--platform",
                "linux/amd64",
                "-t",
                &self.image,
                "-f",
            ])
            .arg(self.repo.join(".github/packaging/qemu/linux.Dockerfile"))
            .arg(self.repo.join(".github/packaging/qemu")))
    }

    /// Prepare a container command with only generated build files writable.
    pub(crate) fn container(&self, directory: &str) -> Result<Command> {
        self.container_image(directory, &self.image)
    }

    /// Prepare a container with an explicit image for build or portability checks.
    pub(crate) fn container_image(&self, directory: &str, image: &str) -> Result<Command> {
        let uid = output(Command::new("id").arg("-u"))?;
        let gid = output(Command::new("id").arg("-g"))?;
        let mut command = Command::new("docker");
        let network = if image == self.base_image {
            "none"
        } else {
            "bridge"
        };
        command
            .args([
                "run",
                "--rm",
                "--network",
                network,
                "--platform",
                "linux/amd64",
                "--user",
                &format!("{uid}:{gid}"),
                "--env",
                "HOME=/tmp",
                "--volume",
            ])
            .arg(format!("{}:/work", self.output.display()))
            .args(["--workdir", directory, image]);
        Ok(command)
    }

    /// Configure the allowlisted devices and build only the shipped executables.
    fn compile(&self, jobs: usize) -> Result<()> {
        let sources = self.output.join("sources/qemu");
        fs::copy(
            self.repo.join(".github/packaging/qemu/amd64.mak"),
            sources.join("configs/devices/x86_64-softmmu/ark.mak"),
        )?;
        fs::create_dir_all(self.output.join("build"))?;
        let args = fs::read_to_string(self.repo.join(".github/packaging/qemu/linux.args"))?;
        run(self
            .container("/work/build")?
            .arg("/work/sources/qemu/configure")
            .args(args.lines()))?;
        run(self.container("/work/build")?.args([
            "ninja",
            "-j",
            &jobs.to_string(),
            "qemu-system-x86_64",
            "qemu-img",
        ]))
    }
}

/// Run a command while retaining its own progress and error output.
pub(crate) fn run(command: &mut Command) -> Result<()> {
    eprintln!("running {command:?}");
    let status = command
        .status()
        .with_context(|| format!("could not run {command:?}"))?;
    ensure!(status.success(), "{command:?} exited with {status}");
    Ok(())
}

/// Capture stdout and include stderr when a command fails.
pub(crate) fn capture(command: &mut Command) -> Result<Output> {
    let result = command
        .output()
        .with_context(|| format!("could not run {command:?}"))?;
    if !result.status.success() {
        bail!(
            "{command:?} exited with {}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(result)
}

/// Read a command's UTF-8 stdout with trailing whitespace removed.
pub(crate) fn output(command: &mut Command) -> Result<String> {
    Ok(String::from_utf8(capture(command)?.stdout)?
        .trim_end()
        .to_owned())
}
