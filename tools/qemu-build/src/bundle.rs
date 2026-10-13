// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Packages the linked runtime, notices and matching source distribution.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Build, capture, output, run};

/// Compute a lowercase SHA-256 digest for a build input or release file.
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Collect runtime files and the source materials for every bundled dependency.
pub(crate) fn package(build: &Build) -> Result<()> {
    // Create a clean runtime so removed dependencies cannot survive a rebuild
    let runtime = build.output.join("runtime");
    if runtime.exists() {
        fs::remove_dir_all(&runtime)?;
    }
    let binaries = runtime.join("binaries");
    let libs = runtime.join("qemu-libs");
    let licenses = libs.join("licenses");
    fs::create_dir_all(&binaries)?;
    fs::create_dir_all(&licenses)?;
    let mut libraries = BTreeMap::new();

    // Strip both executables and collect their complete shared library closure
    for (source, name) in [
        ("qemu-system-x86_64", "qemu-system-guest"),
        ("qemu-img", "qemu-img"),
    ] {
        let target = format!("{name}-x86_64-unknown-linux-gnu");
        fs::copy(
            build.output.join("build").join(source),
            binaries.join(&target),
        )?;
        run(build.container("/work/runtime/binaries")?.args([
            "strip",
            "--strip-unneeded",
            &target,
        ]))?;
        let linked = output(build.container("/work/build")?.args(["ldd", source]))?;
        for (name, path) in parse_ldd(&linked)? {
            libraries.insert(name, path);
        }
    }
    let mut packages = BTreeMap::new();
    let allowed: Vec<_> = fs::read_to_string(build.repo.join(".github/packaging/qemu/linux.libs"))?
        .lines()
        .map(str::to_owned)
        .collect();

    // Match each dependency to its exact distribution source package and notices
    for (name, path) in &libraries {
        ensure!(
            allowed.contains(name),
            "unexpected runtime dependency {name}; review the feature configuration and license before changing linux.libs"
        );
        run(build.container("/work")?.args([
            "cp",
            "-L",
            path,
            &format!("/work/runtime/qemu-libs/{name}"),
        ]))?;
        let owner =
            output(
                build
                    .container("/work")?
                    .args(["dpkg-query", "-S", &format!("*/{name}")]),
            )?;
        let package = owner
            .lines()
            .next()
            .context("missing library package")?
            .rsplit_once(": ")
            .context("invalid package owner")?
            .0;
        let info = output(build.container("/work")?.args([
            "dpkg-query",
            "-W",
            "-f=${source:Package}\t${source:Version}",
            package,
        ]))?;
        let (source, version) = info
            .split_once('\t')
            .context("invalid source package metadata")?;
        packages.insert(source.to_owned(), version.to_owned());
        let binary_package = package
            .split(':')
            .next()
            .context("invalid binary package")?;
        let copyright = capture(
            build
                .container("/work")?
                .arg("cat")
                .arg(format!("/usr/share/doc/{binary_package}/copyright")),
        )?
        .stdout;
        fs::write(
            licenses.join(format!("{binary_package}.copyright")),
            copyright,
        )?;
    }

    // Keep license texts alongside the allowlisted upstream firmware binaries
    run(build.container("/work")?.args([
        "cp",
        "-RL",
        "/usr/share/common-licenses",
        "/work/runtime/qemu-libs/licenses/common",
    ]))?;
    let qemu = build.output.join("sources/qemu");
    for name in ["LICENSE", "COPYING", "COPYING.LIB"] {
        fs::copy(qemu.join(name), licenses.join(format!("QEMU-{name}")))?;
    }
    for (name, path) in [
        ("SeaBIOS", "roms/seabios/COPYING"),
        ("SeaBIOS-LGPL", "roms/seabios/COPYING.LESSER"),
        ("iPXE", "roms/ipxe/COPYING"),
        ("iPXE-GPL", "roms/ipxe/COPYING.GPLv2"),
        ("iPXE-UBDL", "roms/ipxe/COPYING.UBDL"),
        ("EDK2", "roms/edk2/License.txt"),
        ("keycodemapdb", "subprojects/keycodemapdb/LICENSE.BSD"),
        ("keycodemapdb-GPL", "subprojects/keycodemapdb/LICENSE.GPL2"),
    ] {
        fs::copy(qemu.join(path), licenses.join(name))
            .with_context(|| format!("copying license {path}"))?;
    }
    for rom in fs::read_to_string(build.repo.join(".github/packaging/qemu/amd64.roms"))?.lines() {
        fs::copy(qemu.join("pc-bios").join(rom), libs.join(rom))?;
    }
    fs::write(
        libs.join("THIRD_PARTY_NOTICES.txt"),
        "This runtime contains QEMU, SeaBIOS, iPXE and the libraries listed in manifest.json.\nTheir copyright notices and licenses are in licenses/.\nComplete corresponding sources and build recipes accompany this emulator release\nas its *-linux-amd64-qemu-sources.tar.xz download at\nhttps://github.com/dark-bio/emulator/releases\nQEMU and its dependencies retain their own licenses; the emulator's BSD license\ndoes not replace them.\n",
    )?;

    // apt verifies source archives against the signed snapshot's package indexes
    let debian = build.output.join("sources/debian");
    fs::create_dir_all(&debian)?;
    for (name, version) in &packages {
        run(build.container("/work/sources/debian")?.args([
            "apt-get",
            "source",
            "--download-only",
            "--only-source",
            &format!("{name}={version}"),
        ]))?;
    }

    // Record the shipped bytes and build inputs before making release archives
    let origins: Value = serde_json::from_slice(&fs::read(build.output.join("origins.json"))?)?;
    let files = inventory(&runtime)?;
    let image_id = output(Command::new("docker").args([
        "image",
        "inspect",
        "--format",
        "{{.Id}}",
        &build.image,
    ]))?;
    let manifest = json!({"target": "linux-amd64", "qemu": fs::read_to_string(qemu.join("VERSION"))?.trim(), "origins": origins, "source_packages": packages, "libraries": libraries, "files": files,
        "configure": fs::read_to_string(build.repo.join(".github/packaging/qemu/linux.args"))?.lines().collect::<Vec<_>>(),
        "toolchain_image": build.image, "toolchain_image_id": image_id});
    fs::write(
        libs.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    source_archive(build)?;
    let archives = build.output.join("artifacts");
    fs::create_dir_all(&archives)?;
    run(Command::new("tar")
        .args(["-cJf"])
        .arg(archives.join("linux-amd64-qemu-runtime.tar.xz"))
        .arg("-C")
        .arg(&runtime)
        .arg("."))?;
    let mut hashes = String::new();
    for name in [
        "linux-amd64-qemu-runtime.tar.xz",
        "linux-amd64-qemu-sources.tar.xz",
    ] {
        hashes.push_str(&format!(
            "{}  {name}\n",
            digest(&fs::read(archives.join(name))?)
        ));
    }
    fs::write(archives.join("SHA256SUMS"), hashes)?;
    eprintln!(
        "runtime files: {:.2} MiB",
        files
            .iter()
            .map(|file| file["bytes"].as_u64().unwrap_or(0))
            .sum::<u64>() as f64
            / 1_048_576.0
    );
    Ok(())
}

/// Reject unresolved libraries and retain only dependencies outside glibc.
fn parse_ldd(text: &str) -> Result<BTreeMap<String, String>> {
    let mut libraries = BTreeMap::new();
    for line in text.lines() {
        ensure!(
            !line.contains("not found"),
            "unresolved runtime dependency: {line}"
        );
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "=>" {
            continue;
        }
        let name = fields[0];
        if matches!(
            name,
            "libc.so.6"
                | "libm.so.6"
                | "libpthread.so.0"
                | "libdl.so.2"
                | "librt.so.1"
                | "libresolv.so.2"
                | "libutil.so.1"
                | "libgcc_s.so.1"
        ) {
            continue;
        }
        ensure!(
            fields[2].starts_with('/'),
            "unrecognized library path: {line}"
        );
        libraries.insert(name.to_owned(), fields[2].to_owned());
    }
    Ok(libraries)
}

/// Archive all collected sources alongside the exact build recipes.
fn source_archive(build: &Build) -> Result<()> {
    let sources = build.output.join("sources");
    let recipes = sources.join("build-recipe");
    copy_tree(
        &build.repo.join("tools/qemu-build"),
        &recipes.join("tools/qemu-build"),
    )?;
    copy_tree(
        &build.repo.join(".github/packaging/qemu"),
        &recipes.join(".github/packaging/qemu"),
    )?;
    fs::copy(build.repo.join("Cargo.lock"), recipes.join("Cargo.lock"))?;
    fs::copy(build.repo.join("LICENSE"), recipes.join("LICENSE"))?;
    fs::copy(build.repo.join("Makefile"), recipes.join("Makefile"))?;
    fs::write(
        recipes.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"tools/qemu-build\"]\n",
    )?;
    // Keep selected tool dependencies while pruning unrelated workspace packages
    capture(
        Command::new("cargo")
            .args([
                "metadata",
                "--offline",
                "--format-version",
                "1",
                "--filter-platform",
                "x86_64-unknown-linux-gnu",
                "--manifest-path",
            ])
            .arg(recipes.join("Cargo.toml")),
    )?;
    fs::copy(
        build.output.join("runtime/qemu-libs/manifest.json"),
        sources.join("manifest.json"),
    )?;
    fs::copy(
        build.output.join("build/config-host.h"),
        sources.join("config-host.h"),
    )?;
    fs::copy(
        build.output.join("build/x86_64-softmmu-config-devices.mak"),
        sources.join("config-devices.mak"),
    )?;
    let packages = output(build.container("/work")?.args([
        "dpkg-query",
        "-W",
        "-f=${binary:Package}\t${Version}\n",
    ]))?;
    fs::write(sources.join("toolchain-packages.tsv"), packages)?;
    fs::write(
        sources.join("REBUILD.txt"),
        "Install Rust, Docker, Git, tar and Make on Linux amd64.\nFrom the extracted sources directory, run:\n\ncargo run --locked --manifest-path build-recipe/Cargo.toml -p qemu-build -- build --repo build-recipe --output /path/outside/these/sources --sources .\n\nThe Ubuntu snapshot supplies the exact library binaries recorded in toolchain-packages.tsv.\nTheir complete source packages are in debian/, including upstream archives and distribution patches.\nUse dpkg-source -x on a .dsc file to extract a library's source and Debian build instructions.\nThe qemu/roms/Makefile contains the build recipes for the bundled upstream ROMs.\n",
    )?;
    fs::create_dir_all(build.output.join("artifacts"))?;
    run(Command::new("tar")
        .arg("-cJf")
        .arg(
            build
                .output
                .join("artifacts/linux-amd64-qemu-sources.tar.xz"),
        )
        .arg("-C")
        .arg(&build.output)
        .arg("sources"))
}

/// Copy generated runtime files into Tauri's sidecar and resource directories.
pub(crate) fn stage(build: &Build) -> Result<()> {
    for directory in ["binaries", "qemu-libs"] {
        let target = build.repo.join("launcher").join(directory);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        copy_tree(&build.output.join("runtime").join(directory), &target)?;
    }
    Ok(())
}

/// Copy source and runtime files while preserving relative and dangling links.
pub(crate) fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_symlink() {
            if destination.symlink_metadata().is_ok() {
                fs::remove_file(&destination)?;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &destination)?;
            #[cfg(not(unix))]
            anyhow::bail!("source archives must be copied on Linux");
        } else {
            fs::copy(entry.path(), &destination).with_context(|| {
                format!(
                    "copying {} to {}",
                    entry.path().display(),
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Record sizes and hashes in stable path order.
fn inventory(root: &Path) -> Result<Vec<Value>> {
    let mut pending = vec![root.to_owned()];
    let mut paths = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                paths.insert(entry.path());
            }
        }
    }
    paths.into_iter().map(|path| {
        let bytes = fs::read(&path)?;
        Ok(json!({"path": path.strip_prefix(root)?.to_string_lossy(), "bytes": bytes.len(), "sha256": digest(&bytes)}))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Missing shared libraries must fail packaging instead of producing a broken release.
    #[test]
    fn test_unresolved_library_is_rejected() {
        assert!(parse_ldd("libslirp.so.0 => not found").is_err());
    }

    /// Upstream source archives include directory links and absent optional SDKs.
    #[test]
    #[cfg(unix)]
    fn test_source_links_survive_repeated_copy() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::create_dir_all(source.join("headers")).unwrap();
        fs::write(source.join("headers/api.h"), "header").unwrap();
        std::os::unix::fs::symlink("headers", source.join("include")).unwrap();
        std::os::unix::fs::symlink("absent-sdk", source.join("optional")).unwrap();
        copy_tree(&source, &target).unwrap();
        copy_tree(&source, &target).unwrap();
        assert_eq!(
            fs::read_link(target.join("include")).unwrap(),
            Path::new("headers")
        );
        assert_eq!(
            fs::read_link(target.join("optional")).unwrap(),
            Path::new("absent-sdk")
        );
        assert_eq!(
            fs::read_to_string(target.join("include/api.h")).unwrap(),
            "header"
        );
    }
}
