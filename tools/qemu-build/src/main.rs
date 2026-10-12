// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Builds and checks the QEMU runtimes shipped with the emulator.
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
//!
//! macOS builds natively with Apple's SDK. Windows cross-compiles in an Ubuntu
//! container and runs its checks on Windows. Both build the pinned dependency
//! sources in `portable-sources.json` as static libraries, so runtime linking
//! requires only operating system libraries. Their source archives contain
//! every dependency, configuration and rebuild recipe.
//!
//! `QEMU_APT_MIRROR` can override the Windows toolchain's package snapshot for
//! diagnostics. The manifest records an override and the resulting image ID.

mod bundle;
mod check;
mod portable;
mod portable_check;
mod source;
mod target;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use target::Target;

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
    /// Build and stage a runtime and its corresponding source archive.
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
        /// Runtime to build, defaulting to the host platform.
        #[arg(long, value_enum)]
        target: Option<Target>,
    },
    /// Verify the isolated runtime and stage it for Tauri packaging.
    Check {
        /// Emulator repository containing the build recipes.
        #[arg(long)]
        repo: PathBuf,
        /// Directory containing the completed runtime.
        #[arg(long)]
        output: PathBuf,
        /// Runtime to verify, defaulting to the host platform.
        #[arg(long, value_enum)]
        target: Option<Target>,
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
    /// Runtime selected for compilation and packaging.
    target: Target,
    /// Optional Windows toolchain mirror override, recorded in build provenance.
    apt_mirror: Option<String>,
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
            target,
        } => {
            ensure!(jobs > 0, "--jobs must be positive");
            let build = Build::new(&repo, &output, target)?;
            build.toolchain()?;
            if let Some(sources) = sources {
                source::restore(&build, &sources)?;
            } else {
                source::prepare(&build)?;
            }
            if build.target == Target::LinuxAmd64 {
                build.compile(jobs)?;
                bundle::package(&build)?;
            } else {
                portable::build(&build, jobs)?;
                portable::package(&build)?;
            }
            build.check()?;
            bundle::stage(&build)?;
        }
        Operation::Check {
            repo,
            output,
            target,
        } => {
            let build = Build::new(&repo, &output, target)?;
            build.check()?;
            bundle::stage(&build)?;
        }
    }
    Ok(())
}

impl Build {
    /// Resolve paths and reject unsupported build hosts.
    fn new(repo: &Path, output: &Path, target: Option<Target>) -> Result<Self> {
        let native = Target::native()?;
        let target = target.unwrap_or(native);
        ensure!(
            target == native || (native == Target::LinuxAmd64 && target == Target::WindowsAmd64),
            "build macOS on its native architecture and cross-compile Windows on Linux amd64"
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
        let marker = output.join("build-target");
        if marker.exists() {
            ensure!(
                fs::read_to_string(&marker)? == target.label(),
                "target changed; use a fresh QEMU_OUTPUT directory"
            );
        } else {
            fs::write(marker, target.label())?;
        }
        let dockerfile = if target.is_macos() {
            Vec::new()
        } else {
            let name = if target == Target::WindowsAmd64 {
                "windows"
            } else {
                "linux"
            };
            fs::read(repo.join(format!(".github/packaging/qemu/{name}.Dockerfile")))?
        };
        let base_image = std::str::from_utf8(&dockerfile)?
            .lines()
            .find_map(|line| line.strip_prefix("FROM "))
            .unwrap_or("")
            .to_owned();
        let apt_mirror = if target == Target::WindowsAmd64 {
            std::env::var("QEMU_APT_MIRROR").ok()
        } else {
            None
        };
        let mut image_inputs = dockerfile.clone();
        if let Some(mirror) = &apt_mirror {
            image_inputs.extend_from_slice(mirror.as_bytes());
        }
        let image = format!(
            "ark-qemu-{}:{}",
            target.label(),
            &bundle::digest(&image_inputs)[..16]
        );
        Ok(Self {
            repo,
            output,
            image,
            base_image,
            target,
            apt_mirror,
            _lock: lock,
        })
    }

    /// Build the compiler container from its pinned base and package snapshot.
    fn toolchain(&self) -> Result<()> {
        if self.target.is_macos() {
            for program in [
                "clang",
                "cmake",
                "ninja",
                "pkg-config",
                "python3",
                "make",
                "patch",
                "xz",
            ] {
                run(Command::new("/usr/bin/which").arg(program))?;
            }
            let python = self.output.join("host-python/bin/python3");
            if !python.is_file() {
                run(Command::new("python3")
                    .args(["-m", "venv"])
                    .arg(self.output.join("host-python")))?;
            }
            run(Command::new(&python)
                .args([
                    "-m",
                    "pip",
                    "install",
                    "--require-hashes",
                    "--only-binary=:all:",
                    "-r",
                ])
                .arg(
                    self.repo
                        .join(".github/packaging/qemu/python-requirements.txt"),
                ))?;
            return Ok(());
        }
        ensure!(
            cfg!(target_os = "linux"),
            "cross-compile Windows with make qemu-windows on Linux amd64"
        );
        let name = if self.target == Target::WindowsAmd64 {
            "windows"
        } else {
            "linux"
        };
        let mut command = Command::new("docker");
        command.args(["build"]);
        if let Some(mirror) = &self.apt_mirror {
            command.args(["--build-arg", &format!("APT_MIRROR={mirror}")]);
        }
        run(command
            .args(["--platform", "linux/amd64", "-t", &self.image, "-f"])
            .arg(
                self.repo
                    .join(format!(".github/packaging/qemu/{name}.Dockerfile")),
            )
            .arg(self.repo.join(".github/packaging/qemu")))
    }

    /// Prepare a container command with only generated build files writable.
    pub(crate) fn container(&self, directory: &str) -> Result<Command> {
        if self.target.is_macos() {
            let mut command = Command::new("env");
            command
                .current_dir(
                    self.output.join(
                        directory
                            .trim_start_matches("/work")
                            .trim_start_matches('/'),
                    ),
                )
                .env("MACOSX_DEPLOYMENT_TARGET", "15.0")
                .env("PKG_CONFIG_PATH", "")
                .env(
                    "PKG_CONFIG_LIBDIR",
                    self.output.join("prefix/lib/pkgconfig"),
                );
            let mut paths = vec![self.output.join("host-python/bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            command.env("PATH", std::env::join_paths(paths)?);
            return Ok(command);
        }
        self.container_image(directory, &self.image)
    }

    /// Translate a path under the output directory into the build host's view.
    pub(crate) fn work_path(&self, relative: &str) -> String {
        if self.target.is_macos() {
            self.output.join(relative).to_string_lossy().into_owned()
        } else {
            format!("/work/{relative}")
        }
    }

    /// Verify the selected runtime using its platform's executable format.
    fn check(&self) -> Result<()> {
        if self.target == Target::LinuxAmd64 {
            check::runtime(self)
        } else {
            portable::check(self)
        }
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
            .args(["--workdir", directory]);
        if self.target == Target::WindowsAmd64 {
            command.args([
                "--env",
                "PKG_CONFIG=pkg-config",
                "--env",
                "PKG_CONFIG_PATH=",
                "--env",
                "PKG_CONFIG_LIBDIR=/work/prefix/lib/pkgconfig",
            ]);
        }
        command.arg(image);
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
