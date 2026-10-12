// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Exercises the runtime in a bare Ubuntu container with networking disabled.

use std::fs;
use std::process::Command;

use anyhow::{Result, ensure};
use serde_json::Value;

use crate::{Build, output, run};

/// Verify packaged files, dependency isolation, devices and writable disk behavior.
pub(crate) fn runtime(build: &Build) -> Result<()> {
    let root = build.output.join("runtime");
    let archives = build.output.join("artifacts");
    let mut checked = std::collections::BTreeSet::new();
    for line in fs::read_to_string(archives.join("SHA256SUMS"))?.lines() {
        let (hash, name) = line
            .split_once("  ")
            .ok_or_else(|| anyhow::anyhow!("invalid archive checksum"))?;
        ensure!(
            matches!(
                name,
                "linux-amd64-qemu-runtime.tar.xz" | "linux-amd64-qemu-sources.tar.xz"
            ),
            "unexpected archive {name}"
        );
        ensure!(
            crate::bundle::digest(&fs::read(archives.join(name))?) == hash,
            "archive checksum mismatch for {name}"
        );
        checked.insert(name.to_owned());
    }
    ensure!(
        checked.len() == 2,
        "checksums must cover both runtime and source archives"
    );

    // Verify each staged file before executing it in the bare container
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("qemu-libs/manifest.json"))?)?;
    for file in manifest["files"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest has no files"))?
    {
        let path = file["path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid manifest path"))?;
        ensure!(
            !std::path::Path::new(path).is_absolute() && !path.split('/').any(|part| part == ".."),
            "invalid manifest path {path}"
        );
        ensure!(
            crate::bundle::digest(&fs::read(root.join(path))?) == file["sha256"],
            "runtime checksum mismatch for {path}"
        );
    }
    for name in ["qemu-system-guest", "qemu-img"] {
        let deps = output(
            build
                .container_image("/work/runtime", &build.base_image)?
                .args([
                    "env",
                    "LD_LIBRARY_PATH=/work/runtime/qemu-libs",
                    "ldd",
                    &format!("binaries/{name}-x86_64-unknown-linux-gnu"),
                ]),
        )?;
        ensure!(
            !deps.contains("not found"),
            "unresolved packaged dependencies: {deps}"
        );
        for library in manifest["libraries"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("manifest has no libraries"))?
            .keys()
        {
            if let Some(line) = deps
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("{library} ")))
            {
                ensure!(
                    line.contains("=> /work/runtime/qemu-libs/"),
                    "library resolved outside the runtime: {line}"
                );
            }
        }
    }
    let accelerators = output(qemu(build)?.args(["-accel", "help"]))?;
    ensure!(
        accelerators.lines().any(|line| line == "kvm")
            && accelerators.lines().any(|line| line == "tcg"),
        "KVM or TCG is missing"
    );
    let devices = output(qemu(build)?.args(["-device", "help"]))?;
    for name in [
        "virtio-blk-pci",
        "virtio-net-pci",
        "virtio-serial-pci",
        "virtserialport",
    ] {
        ensure!(
            devices.contains(&format!("\"{name}\"")),
            "required device {name} is missing"
        );
    }
    fs::create_dir_all(build.output.join("check"))?;
    run(img(build)?.args(["create", "-f", "qcow2", "/work/check/disk.qcow2", "64M"]))?;
    smoke(build)?;
    run(img(build)?.args(["check", "/work/check/disk.qcow2"]))?;
    Ok(())
}

/// Prepare the packaged system emulator in the isolated container.
fn qemu(build: &Build) -> Result<Command> {
    let mut cmd = build.container_image("/work/runtime", &build.base_image)?;
    cmd.args([
        "env",
        "LD_LIBRARY_PATH=/work/runtime/qemu-libs",
        "timeout",
        "40",
        "binaries/qemu-system-guest-x86_64-unknown-linux-gnu",
        "-L",
        "qemu-libs",
    ]);
    Ok(cmd)
}

/// Prepare the packaged image utility in the isolated container.
fn img(build: &Build) -> Result<Command> {
    let mut cmd = build.container_image("/work/runtime", &build.base_image)?;
    cmd.args([
        "env",
        "LD_LIBRARY_PATH=/work/runtime/qemu-libs",
        "binaries/qemu-img-x86_64-unknown-linux-gnu",
    ]);
    Ok(cmd)
}

/// Exercise QMP, socket character devices, SLIRP and disk locks on a paused q35 VM.
#[cfg(unix)]
fn smoke(build: &Build) -> Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use crate::capture;

    /// Wait for a command response while allowing asynchronous QMP events.
    fn request(reader: &mut BufReader<UnixStream>, command: Value) -> Result<Value> {
        writeln!(reader.get_mut(), "{command}")?;
        loop {
            let mut line = String::new();
            ensure!(
                reader.read_line(&mut line)? > 0,
                "QMP closed before replying to {command}"
            );
            let reply: Value = serde_json::from_str(&line)?;
            if reply.get("event").is_some() {
                continue;
            }
            ensure!(
                reply.get("return").is_some(),
                "QMP {command} failed: {reply}"
            );
            return Ok(reply["return"].clone());
        }
    }

    let directory = build.output.join("check");
    for name in ["qmp.sock", "serial.sock"] {
        let path = directory.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let mut child = qemu(build)?.args([
        "-M", "q35", "-cpu", "max", "-accel", "tcg", "-m", "128", "-S", "-nographic", "-monitor", "none", "-serial", "null",
        "-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:18181-:18181", "-device", "virtio-net-pci,netdev=net0",
        "-drive", "file=/work/check/disk.qcow2,if=none,id=disk0,format=qcow2,discard=unmap,detect-zeroes=unmap",
        "-device", "virtio-blk-pci,drive=disk0", "-device", "virtio-serial-pci",
        "-chardev", "socket,id=hw,path=/work/check/serial.sock,server=on,wait=off",
        "-device", "virtserialport,chardev=hw,name=ark.hardware",
        "-qmp", "unix:/work/check/qmp.sock,server=on,wait=off",
    ]).spawn()?;
    let result = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let stream = loop {
            if let Ok(stream) = UnixStream::connect(directory.join("qmp.sock")) {
                break stream;
            }
            ensure!(
                child.try_wait()?.is_none(),
                "QEMU exited before exposing QMP"
            );
            ensure!(Instant::now() < deadline, "QMP startup timed out");
            std::thread::sleep(Duration::from_millis(50));
        };
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        ensure!(
            serde_json::from_str::<Value>(&line)?["QMP"].is_object(),
            "missing QMP greeting"
        );
        for command in [
            "qmp_capabilities",
            "query-status",
            "query-pci",
            "query-block",
        ] {
            request(&mut reader, serde_json::json!({"execute": command}))?;
        }
        let _serial = UnixStream::connect(directory.join("serial.sock"))?;
        // Exercise qcow2 writes and discard through the live block device
        for operation in ["write -P 0x5a 0 131072", "discard 0 65536"] {
            request(
                &mut reader,
                serde_json::json!({"execute": "human-monitor-command", "arguments": {"command-line": format!("qemu-io disk0 \"{operation}\"")}}),
            )?;
        }
        let locked = img(build)?
            .args(["info", "/work/check/disk.qcow2"])
            .output()?;
        ensure!(
            !locked.status.success() && String::from_utf8_lossy(&locked.stderr).contains("lock"),
            "qemu-img did not reject a disk held by QEMU"
        );
        // Keep the channel open until QEMU acknowledges the shutdown request
        request(&mut reader, serde_json::json!({"execute": "quit"}))?;
        Ok(())
    })();
    // The container timeout also bounds failures before the QMP quit command
    let status = child.wait()?;
    result?;
    ensure!(status.success(), "QEMU exited with {status}");
    let info = capture(img(build)?.args(["info", "--output=json", "/work/check/disk.qcow2"]))?;
    ensure!(
        serde_json::from_slice::<Value>(&info.stdout)?["format"] == "qcow2",
        "disk format changed"
    );
    run(img(build)?.args([
        "convert",
        "-O",
        "raw",
        "/work/check/disk.qcow2",
        "/work/check/disk.raw",
    ]))?;
    let mut data = vec![0_u8; 131072];
    fs::File::open(directory.join("disk.raw"))?.read_exact(&mut data)?;
    ensure!(
        data[..65536].iter().all(|byte| *byte == 0),
        "discard did not clear the first block"
    );
    ensure!(
        data[65536..].iter().all(|byte| *byte == 0x5a),
        "qcow2 write did not persist"
    );
    Ok(())
}

/// Reject unsupported hosts without importing Unix socket APIs there.
#[cfg(not(unix))]
fn smoke(_build: &Build) -> Result<()> {
    anyhow::bail!("the Linux runtime checks require Unix sockets")
}
