// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Fetches pinned QEMU, subproject and ROM sources into the build directory.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::{Build, output, run, run_retry};

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
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json"))?)?;
    ensure!(
        manifest["target"] == build.target.label(),
        "source archive target does not match the selected runtime"
    );
    let target = build.output.join("sources");
    fs::create_dir_all(&target)?;
    for name in ["qemu", "debian", "dependencies"] {
        if source.join(name).is_dir() {
            crate::bundle::copy_tree(&source.join(name), &target.join(name))?;
        }
    }
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    fs::write(
        build.output.join("origins.json"),
        serde_json::to_vec_pretty(&manifest["origins"])?,
    )?;
    Ok(())
}

/// Export source trees and retain their exact origins for the release manifest.
pub(crate) fn prepare(build: &Build) -> Result<()> {
    let pinned: serde_json::Value = serde_json::from_slice(&fs::read(
        build.repo.join(".github/packaging/qemu/source.json"),
    )?)?;
    let url = pinned["url"].as_str().context("missing QEMU source URL")?;
    let revision = pinned["revision"]
        .as_str()
        .context("missing QEMU source revision")?;
    let sources = build.output.join("sources");
    fs::create_dir_all(&sources)?;
    let marker = sources.join("qemu-revision");
    if marker.exists() {
        ensure!(
            fs::read_to_string(&marker)? == revision,
            "QEMU revision changed; use a fresh QEMU_OUTPUT directory"
        );
    }

    // Keep the Git cache separate from the exported tree that receives patches
    let qemu = build.output.join("git/qemu");
    fetch(&qemu, url, revision)?;
    if !marker.exists() {
        export(
            &qemu,
            revision,
            &sources.join("qemu"),
            &build.output.join("qemu.tar"),
        )?;
        fs::write(&marker, revision)?;
    }
    let mut origins = vec![json!({"name": "qemu", "url": url, "revision": revision})];

    // Meson wraps pin these build inputs independently of QEMU's submodules.
    let mut wraps = vec![
        "keycodemapdb",
        "berkeley-softfloat-3",
        "berkeley-testfloat-3",
    ];
    if build.target.arch() == "arm64" {
        wraps.push("dtc");
    }
    for name in wraps {
        let wrap = fs::read_to_string(sources.join(format!("qemu/subprojects/{name}.wrap")))?;
        let value = |key: &str| -> Result<&str> {
            wrap.lines()
                .find_map(|line| line.strip_prefix(&format!("{key} = ")))
                .with_context(|| format!("missing {key} in {name}.wrap"))
        };
        let url = value("url")?;
        let revision = value("revision")?;
        let cache = build.output.join("git").join(name);
        fetch(&cache, url, revision)?;
        export(
            &cache,
            revision,
            &sources.join(format!("qemu/subprojects/{name}")),
            &build.output.join(format!("{name}.tar")),
        )?;
        let overlay = sources.join(format!("qemu/subprojects/packagefiles/{name}"));
        if overlay.is_dir() {
            crate::bundle::copy_tree(&overlay, &sources.join(format!("qemu/subprojects/{name}")))?;
        }
        origins.push(json!({"name": name, "url": url, "revision": revision}));
    }

    // Only amd64 ships a BIOS; discard source trees for unbundled firmware
    for name in ["seabios", "ipxe", "edk2"] {
        let path = format!("roms/{name}");
        if name != "seabios" || build.target.arch() != "amd64" {
            let unused = sources.join("qemu").join(&path);
            if unused.is_dir() {
                fs::remove_dir_all(unused)?;
            }
            continue;
        }
        let entry = output(
            Command::new("git")
                .arg("-C")
                .arg(&qemu)
                .args(["ls-tree", revision, &path]),
        )?;
        let revision = entry
            .split_whitespace()
            .nth(2)
            .context("missing ROM source revision")?;
        let url = output(
            Command::new("git")
                .args(["config", "-f"])
                .arg(sources.join("qemu/.gitmodules"))
                .args(["--get", &format!("submodule.{path}.url")]),
        )?;
        let cache = build.output.join("git").join(name);
        fetch(&cache, &url, revision)?;
        export(
            &cache,
            revision,
            &sources.join(format!("qemu/{path}")),
            &build.output.join(format!("{name}.tar")),
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
    let path = build.work_path(&format!("sources/{name}"));
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
    let manifest = fs::read_to_string(build.repo.join(format!(
        ".github/packaging/qemu/{}.roms",
        build.target.arch()
    )))?;
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

/// Fetch an immutable Git commit into a bare cache, reusing it without network access.
fn fetch(cache: &Path, url: &str, revision: &str) -> Result<()> {
    ensure!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "source revision must be a full Git commit hash"
    );
    fs::create_dir_all(cache)?;
    if !cache.join("HEAD").exists() {
        run(Command::new("git").arg("init").arg("--bare").arg(cache))?;
    }
    if !Command::new("git")
        .arg("-C")
        .arg(cache)
        .args(["cat-file", "-e", &format!("{revision}^{{commit}}")])
        .output()?
        .status
        .success()
    {
        run_retry(
            Command::new("git")
                .args([
                    "-c",
                    "http.lowSpeedLimit=1024",
                    "-c",
                    "http.lowSpeedTime=60",
                ])
                .env("GIT_TERMINAL_PROMPT", "0")
                .arg("-C")
                .arg(cache)
                .args(["fetch", "--depth", "1", url, revision]),
        )?;
    }
    Ok(())
}

/// Export the pinned tree without repository metadata or a working checkout.
fn export(repo: &Path, revision: &str, destination: &Path, archive: &Path) -> Result<()> {
    run(Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("archive")
        .arg(format!("--output={}", archive.display()))
        .arg(revision))?;
    fs::create_dir_all(destination)?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(destination))?;
    fs::remove_file(archive)?;
    Ok(())
}

/// Checks immutable source selection and reuse without the upstream repository.
#[cfg(test)]
mod tests {
    use super::*;

    /// Export the selected commit even when upstream advances or becomes unavailable.
    #[test]
    fn test_pinned_sources_survive_upstream_changes() -> Result<()> {
        // Create two upstream revisions so fetching HEAD would export the wrong file
        let directory = tempfile::tempdir()?;
        let upstream = directory.path().join("upstream");
        run(Command::new("git").arg("init").arg(&upstream))?;
        let mut revision = String::new();
        for content in ["pinned source", "new upstream source"] {
            fs::write(upstream.join("source.txt"), content)?;
            run(Command::new("git")
                .arg("-C")
                .arg(&upstream)
                .args(["add", "source.txt"]))?;
            run(Command::new("git").arg("-C").arg(&upstream).args([
                "-c",
                "user.name=Source Test",
                "-c",
                "user.email=source@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                content,
            ]))?;
            if revision.is_empty() {
                revision = output(
                    Command::new("git")
                        .arg("-C")
                        .arg(&upstream)
                        .args(["rev-parse", "HEAD"]),
                )?;
            }
        }

        // A full hash fetches the older commit without following a moving reference
        let cache = directory.path().join("cache");
        let url = upstream.to_str().context("non-UTF-8 fixture path")?;
        assert!(fetch(&cache, url, "HEAD").is_err());
        fetch(&cache, url, &revision)?;
        let destination = directory.path().join("exported sources");
        let archive = directory.path().join("source.tar");
        export(&cache, &revision, &destination, &archive)?;
        assert_eq!(
            fs::read_to_string(destination.join("source.txt"))?,
            "pinned source"
        );
        assert!(!destination.join(".git").exists());

        // Once cached, the same source can be exported without contacting upstream
        fs::rename(&upstream, directory.path().join("offline"))?;
        fetch(&cache, url, &revision)?;
        fs::remove_dir_all(&destination)?;
        export(&cache, &revision, &destination, &archive)?;
        assert_eq!(
            fs::read_to_string(destination.join("source.txt"))?,
            "pinned source"
        );
        Ok(())
    }
}
