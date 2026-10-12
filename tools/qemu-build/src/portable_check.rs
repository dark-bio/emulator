// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Checks portable dependencies and exercises native QEMU through QMP.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{Build, Target, bundle, capture, output, run};

/// Verify the distributed files and run executable checks on the target host.
pub(crate) fn check(build: &Build) -> Result<()> {
    let runtime = build.output.join("runtime");
    let artifacts = build.output.join("artifacts");
    let sums = fs::read_to_string(artifacts.join("SHA256SUMS"))?;
    for kind in ["runtime", "sources"] {
        let name = format!("{}-qemu-{kind}.tar.xz", build.target.label());
        let expected = format!(
            "{}  {name}",
            bundle::digest(&fs::read(artifacts.join(&name))?)
        );
        ensure!(
            sums.lines().any(|line| line == expected),
            "invalid checksum for {name}"
        );
    }
    let manifest: Value =
        serde_json::from_slice(&fs::read(runtime.join("qemu-libs/manifest.json"))?)?;
    ensure!(
        manifest["target"] == build.target.label(),
        "runtime target mismatch"
    );
    for file in manifest["files"]
        .as_array()
        .context("missing runtime inventory")?
    {
        let path = file["path"].as_str().context("invalid inventory path")?;
        ensure!(
            Path::new(path)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "invalid inventory path {path}"
        );
        ensure!(
            bundle::digest(&fs::read(runtime.join(path))?) == file["sha256"],
            "runtime checksum mismatch for {path}"
        );
    }

    // Every non-system library is built into the executable, including transitive imports
    for name in ["qemu-system-guest", "qemu-img"] {
        let path = binary(build, name);
        if build.target.is_macos() {
            let listing = output(Command::new("otool").arg("-L").arg(&path))?;
            for line in listing.lines().skip(1) {
                let dependency = line
                    .split_whitespace()
                    .next()
                    .context("invalid Mach-O dependency")?;
                ensure!(
                    dependency.starts_with("/usr/lib/")
                        || dependency.starts_with("/System/Library/"),
                    "non-system dependency {dependency}"
                );
            }
            run(Command::new("codesign")
                .args(["--verify", "--strict"])
                .arg(&path))?;
        } else {
            let imports = if cfg!(target_os = "windows") {
                output(
                    Command::new("dumpbin")
                        .args(["/nologo", "/dependents"])
                        .arg(&path),
                )?
            } else {
                output(Command::new("objdump").arg("-p").arg(&path))?
            };
            let mut count = 0;
            for line in imports.lines() {
                let name = line.trim().strip_prefix("DLL Name:").unwrap_or(line).trim();
                if name.to_ascii_lowercase().ends_with(".dll") && !name.contains(' ') {
                    ensure!(system_dll(name), "non-system Windows import {name}");
                    count += 1;
                }
            }
            ensure!(count > 0, "no PE imports were inspected");
        }
    }
    if build.target == Target::native()? {
        smoke(build)?;
    }
    Ok(())
}

/// Match Windows libraries supplied by the operating system rather than MinGW.
fn system_dll(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("api-ms-win-")
        || name.starts_with("ext-ms-win-")
        || matches!(
            name.as_str(),
            "advapi32.dll"
                | "bcrypt.dll"
                | "crypt32.dll"
                | "dbghelp.dll"
                | "dnsapi.dll"
                | "gdi32.dll"
                | "imm32.dll"
                | "iphlpapi.dll"
                | "kernel32.dll"
                | "msvcrt.dll"
                | "ntdll.dll"
                | "ole32.dll"
                | "oleaut32.dll"
                | "psapi.dll"
                | "secur32.dll"
                | "setupapi.dll"
                | "shell32.dll"
                | "shlwapi.dll"
                | "user32.dll"
                | "userenv.dll"
                | "ucrtbase.dll"
                | "version.dll"
                | "winmm.dll"
                | "ws2_32.dll"
                | "wtsapi32.dll"
        )
}

/// Resolve the Tauri sidecar filename within the verified runtime.
fn binary(build: &Build, name: &str) -> PathBuf {
    build.output.join("runtime/binaries").join(format!(
        "{name}-{}{}",
        build.target.triple(),
        build.target.extension()
    ))
}

/// Execute a sidecar with dependency search limited to system libraries.
fn command(build: &Build, name: &str) -> Command {
    let mut command = Command::new(binary(build, name));
    command
        .env_remove("DYLD_LIBRARY_PATH")
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH");
    if let Some(root) = std::env::var_os("SystemRoot") {
        let root = PathBuf::from(root);
        command.env(
            "PATH",
            std::env::join_paths([root.join("System32"), root])
                .expect("system paths contain no separators"),
        );
    }
    command
}

/// Choose an available loopback port for a short-lived native QMP check.
fn port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Stop the guest even when a QMP assertion or socket operation fails.
struct Guest(Child);

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait for a QMP response while accepting asynchronous lifecycle events.
fn request(reader: &mut BufReader<TcpStream>, value: Value) -> Result<Value> {
    writeln!(reader.get_mut(), "{value}")?;
    loop {
        let mut line = String::new();
        ensure!(
            reader.read_line(&mut line)? > 0,
            "QMP closed before replying"
        );
        let response: Value = serde_json::from_str(&line)?;
        if response.get("event").is_some() {
            continue;
        }
        ensure!(
            response.get("return").is_some(),
            "QMP command failed: {response}"
        );
        return Ok(response["return"].clone());
    }
}

/// Check acceleration availability, transports, qcow2 writes, discard and locks.
fn smoke(build: &Build) -> Result<()> {
    let accelerators = output(command(build, "qemu-system-guest").args(["-accel", "help"]))?;
    let accelerator = if build.target.is_macos() {
        "hvf"
    } else {
        "whpx"
    };
    ensure!(
        accelerators.lines().any(|line| line == accelerator)
            && accelerators.lines().any(|line| line == "tcg"),
        "required acceleration backend missing"
    );
    let devices = output(command(build, "qemu-system-guest").args(["-device", "help"]))?;
    for device in [
        "virtio-blk-pci",
        "virtio-net-pci",
        "virtio-serial-pci",
        "virtserialport",
    ] {
        ensure!(
            devices.contains(&format!("\"{device}\"")),
            "missing device {device}"
        );
    }
    let directory = build.output.join("check");
    fs::create_dir_all(&directory)?;
    let disk = directory.join("disk.qcow2");
    run(command(build, "qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(&disk)
        .arg("64M"))?;

    // TCP character devices work on both native platforms without Unix socket emulation
    let qmp = port()?;
    let serial = port()?;
    let forward = port()?;
    let machine = if build.target.arch() == "arm64" {
        "virt"
    } else {
        "q35"
    };
    let mut guest = Guest(
        command(build, "qemu-system-guest")
            .current_dir(&directory)
            // QEMU appends '/' to this path, which Windows verbatim paths reject
            .arg("-L")
            .arg("../runtime/qemu-libs")
            .args([
                "-M",
                machine,
                "-cpu",
                "max",
                "-accel",
                "tcg",
                "-m",
                "128",
                "-S",
                "-nographic",
                "-monitor",
                "none",
                "-serial",
                "null",
                "-netdev",
                &format!("user,id=net0,hostfwd=tcp:127.0.0.1:{forward}-:18181"),
                "-device",
                "virtio-net-pci,netdev=net0",
                "-drive",
                "file=disk.qcow2,if=none,id=disk0,format=qcow2,discard=unmap,detect-zeroes=unmap",
                "-device",
                "virtio-blk-pci,drive=disk0",
                "-device",
                "virtio-serial-pci",
                "-chardev",
                &format!("socket,id=hw,host=127.0.0.1,port={serial},server=on,wait=off"),
                "-device",
                "virtserialport,chardev=hw,name=ark.hardware",
                "-qmp",
                &format!("tcp:127.0.0.1:{qmp},server=on,wait=off"),
            ])
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let stream = loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", qmp)) {
            break stream;
        }
        ensure!(
            guest.0.try_wait()?.is_none(),
            "QEMU exited before QMP startup"
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
        "invalid QMP greeting"
    );
    request(&mut reader, json!({"execute":"qmp_capabilities"}))?;
    let _serial = TcpStream::connect(("127.0.0.1", serial))?;
    for operation in ["write -P 0x5a 0 131072", "discard 0 65536"] {
        request(
            &mut reader,
            json!({"execute":"human-monitor-command", "arguments":{"command-line":format!("qemu-io disk0 \"{operation}\"")}}),
        )?;
    }
    // QEMU's Windows file backend has no locking; the launcher checks image usage
    if build.target.is_macos() {
        let locked = command(build, "qemu-img").arg("info").arg(&disk).output()?;
        ensure!(
            !locked.status.success(),
            "qemu-img opened a disk held by QEMU"
        );
    }
    request(&mut reader, json!({"execute":"quit"}))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = guest.0.try_wait()? {
            ensure!(status.success(), "QEMU exited with {status}");
            break;
        }
        ensure!(Instant::now() < deadline, "QEMU shutdown timed out");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Inspect persisted bytes after closing the process that owned the qcow2 image
    let info = capture(
        command(build, "qemu-img")
            .args(["info", "--output=json"])
            .arg(&disk),
    )?;
    ensure!(
        serde_json::from_slice::<Value>(&info.stdout)?["format"] == "qcow2",
        "unexpected disk format"
    );
    let raw = directory.join("disk.raw");
    run(command(build, "qemu-img")
        .args(["convert", "-O", "raw"])
        .arg(&disk)
        .arg(&raw))?;
    let mut bytes = vec![0; 131072];
    fs::File::open(raw)?.read_exact(&mut bytes)?;
    ensure!(
        bytes[..65536].iter().all(|byte| *byte == 0)
            && bytes[65536..].iter().all(|byte| *byte == 0x5a),
        "qcow2 write or discard did not persist"
    );
    run(command(build, "qemu-img").arg("check").arg(disk))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portable runtime must reject MinGW and package-manager DLL imports.
    #[test]
    fn test_external_windows_imports_are_rejected() {
        assert!(system_dll("KERNEL32.dll"));
        assert!(system_dll("api-ms-win-crt-heap-l1-1-0.dll"));
        for name in [
            "libglib-2.0-0.dll",
            "libgcc_s_seh-1.dll",
            "libwinpthread-1.dll",
            "msys-2.0.dll",
        ] {
            assert!(!system_dll(name), "{name}");
        }
    }
}
