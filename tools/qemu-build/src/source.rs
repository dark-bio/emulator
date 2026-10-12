// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Collects the pinned QEMU checkout, required subprojects and ROM sources.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::{Build, output, run};

/// Restore an extracted source release so rebuilding does not require Git repositories.
pub(crate) fn restore(build: &Build, source: &Path) -> Result<()> {
    let source = source.canonicalize()?;
    ensure!(
        !build.output.starts_with(&source) && !source.starts_with(&build.output),
        "rebuild output must be outside the extracted sources"
    );
    ensure!(
        source.join("qemu/configure").is_file(),
        "the source archive has no QEMU checkout"
    );
    let target = build.output.join("sources");
    fs::create_dir_all(&target)?;
    for name in ["qemu", "debian"] {
        crate::bundle::copy_tree(&source.join(name), &target.join(name))?;
    }
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json"))?)?;
    fs::write(
        build.output.join("origins.json"),
        serde_json::to_vec_pretty(&manifest["origins"])?,
    )?;
    Ok(())
}

/// Export source trees and retain their exact origins for the release manifest.
pub(crate) fn prepare(build: &Build) -> Result<()> {
    let qemu = build.repo.join("third_party/qemu");
    ensure!(
        qemu.join("configure").is_file(),
        "initialize QEMU with git submodule update --init --depth 1 third_party/qemu"
    );
    ensure!(
        output(Command::new("git").arg("-C").arg(&qemu).args([
            "status",
            "--porcelain",
            "--untracked-files=no"
        ]))?
        .is_empty(),
        "the QEMU submodule has local changes"
    );
    let revision = output(
        Command::new("git")
            .arg("-C")
            .arg(&qemu)
            .args(["rev-parse", "HEAD"]),
    )?;
    let pinned = output(Command::new("git").arg("-C").arg(&build.repo).args([
        "ls-files",
        "--stage",
        "third_party/qemu",
    ]))?;
    ensure!(
        pinned.split_whitespace().nth(1) == Some(&revision),
        "QEMU does not match the recorded submodule revision"
    );
    let sources = build.output.join("sources");
    fs::create_dir_all(&sources)?;
    let marker = sources.join("qemu-revision");
    if marker.exists() {
        ensure!(
            fs::read_to_string(&marker)? == revision,
            "QEMU revision changed; use a fresh QEMU_OUTPUT directory"
        );
    } else {
        export(&qemu, &sources.join("qemu"), &build.output.join("qemu.tar"))?;
        fs::write(&marker, &revision)?;
    }
    let mut origins = vec![
        json!({"name": "qemu", "url": "https://gitlab.com/qemu-project/qemu.git", "revision": revision}),
    ];

    // Meson wraps pin these build inputs independently of QEMU's submodules.
    for name in [
        "keycodemapdb",
        "berkeley-softfloat-3",
        "berkeley-testfloat-3",
    ] {
        let wrap = fs::read_to_string(qemu.join(format!("subprojects/{name}.wrap")))?;
        let value = |key: &str| -> Result<&str> {
            wrap.lines()
                .find_map(|line| line.strip_prefix(&format!("{key} = ")))
                .with_context(|| format!("missing {key} in {name}.wrap"))
        };
        let url = value("url")?;
        let revision = value("revision")?;
        fetch(
            build,
            name,
            url,
            revision,
            &sources.join(format!("qemu/subprojects/{name}")),
        )?;
        let overlay = sources.join(format!("qemu/subprojects/packagefiles/{name}"));
        if overlay.is_dir() {
            crate::bundle::copy_tree(&overlay, &sources.join(format!("qemu/subprojects/{name}")))?;
        }
        origins.push(json!({"name": name, "url": url, "revision": revision}));
    }

    // The shipped BIOS and network ROMs have their own corresponding sources.
    for name in ["seabios", "ipxe", "edk2"] {
        let path = format!("roms/{name}");
        let entry = output(
            Command::new("git")
                .arg("-C")
                .arg(&qemu)
                .args(["ls-tree", "HEAD", &path]),
        )?;
        let revision = entry
            .split_whitespace()
            .nth(2)
            .context("missing ROM source revision")?;
        let url = output(Command::new("git").arg("-C").arg(&qemu).args([
            "config",
            "-f",
            ".gitmodules",
            "--get",
            &format!("submodule.{path}.url"),
        ]))?;
        fetch(
            build,
            name,
            &url,
            revision,
            &sources.join(format!("qemu/{path}")),
        )?;
        origins.push(json!({"name": name, "url": url, "revision": revision}));
    }
    fs::write(
        build.output.join("origins.json"),
        serde_json::to_vec_pretty(&origins)?,
    )?;
    patch_blobs(build)?;
    prune_blobs(build)?;
    Ok(())
}

/// Apply the firmware packaging fix while accepting an already patched build tree.
fn patch_blobs(build: &Build) -> Result<()> {
    let name = "no-unused-blobs.patch";
    fs::copy(
        build.repo.join(".github/packaging/qemu").join(name),
        build.output.join("sources").join(name),
    )?;
    let path = format!("/work/sources/{name}");
    let forward = build
        .container("/work/sources/qemu")?
        .args(["patch", "--dry-run", "--forward", "-p1", "-i", &path])
        .output()?;
    if forward.status.success() {
        run(build.container("/work/sources/qemu")?.args([
            "patch",
            "--forward",
            "-p1",
            "-i",
            &path,
        ]))?;
    } else {
        run(build.container("/work/sources/qemu")?.args([
            "patch",
            "--dry-run",
            "--reverse",
            "-p1",
            "-i",
            &path,
        ]))?;
    }
    Ok(())
}

/// Keep only shipped ROM binaries in the source distribution.
fn prune_blobs(build: &Build) -> Result<()> {
    let bios = build.output.join("sources/qemu/pc-bios");
    let manifest = fs::read_to_string(build.repo.join(".github/packaging/qemu/amd64.roms"))?;
    let keep: Vec<_> = manifest.lines().collect();
    let meson = fs::read_to_string(bios.join("meson.build"))?;
    let blobs = meson
        .split_once("blobs = [")
        .context("QEMU ROM list is missing")?
        .1
        .split_once(']')
        .context("QEMU ROM list is incomplete")?
        .0;
    for line in blobs.lines() {
        let name = line.trim().trim_end_matches(',').trim_matches('\'');
        if !name.is_empty() && !keep.contains(&name) && bios.join(name).is_file() {
            fs::remove_file(bios.join(name))?;
        }
    }
    for entry in fs::read_dir(&bios)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().ends_with(".fd.bz2")
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Download an immutable Git revision and export it without repository metadata.
fn fetch(build: &Build, name: &str, url: &str, revision: &str, destination: &Path) -> Result<()> {
    let cache = build.output.join("git").join(name);
    fs::create_dir_all(&cache)?;
    if !cache.join("HEAD").exists() {
        run(Command::new("git").arg("init").arg("--bare").arg(&cache))?;
    }
    if !Command::new("git")
        .arg("-C")
        .arg(&cache)
        .args(["cat-file", "-e", &format!("{revision}^{{commit}}")])
        .output()?
        .status
        .success()
    {
        run(Command::new("git")
            .arg("-C")
            .arg(&cache)
            .args(["fetch", "--depth", "1", url, revision]))?;
    }
    let archive = build.output.join(format!("{name}.tar"));
    run(Command::new("git")
        .arg("-C")
        .arg(&cache)
        .arg("archive")
        .arg(format!("--output={}", archive.display()))
        .arg(revision))?;
    fs::create_dir_all(destination)?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(destination))?;
    fs::remove_file(archive)?;
    Ok(())
}

/// Export the current QEMU revision without touching its checkout.
fn export(repo: &Path, destination: &Path, archive: &Path) -> Result<()> {
    run(Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("archive")
        .arg(format!("--output={}", archive.display()))
        .arg("HEAD"))?;
    fs::create_dir_all(destination)?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(destination))?;
    fs::remove_file(archive)?;
    Ok(())
}
