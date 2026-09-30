// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Exercises launcher failure propagation with a refused registry and a fake guest.

#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Refused initial and later publications stop the guest and reach either caller.
#[test]
fn test_registration_refusals_reach_standalone_and_start_commands() {
    // Never send test publications into an emulator's live registry
    let listener = match TcpListener::bind(("127.0.0.1", 18180)) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("registry lifecycle test skipped: port 18180 is in use");
            return;
        }
        Err(err) => panic!("could not reserve the test registry: {err}"),
    };
    let server = tiny_http::Server::from_listener(listener, None).unwrap();

    // An isolated executable resolves only this fixture's guest and state
    let directory = tempfile::TempDir::new().unwrap();
    let executable = directory.path().join("ark-emulator");
    fs::copy(env!("CARGO_BIN_EXE_ark-emulator"), &executable).unwrap();
    let guest = directory.path().join("qemu-system-guest");
    let pid_file = directory.path().join("qemu-system-guest.pid");
    fs::write(
        &guest,
        "#!/bin/sh\necho $$ > \"$0.pid\"\nexec /bin/sleep 30\n",
    )
    .unwrap();
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).unwrap();
    for file in ["image.ark", "kernel", "initrd"] {
        fs::write(directory.path().join(file), []).unwrap();
    }

    // Accept a configurable number of heartbeats, then refuse publication
    let accepted = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let worker_accepted = accepted.clone();
    let worker_finished = finished.clone();
    let worker_pid = pid_file.clone();
    let worker = thread::spawn(move || {
        while !worker_finished.load(Ordering::SeqCst) {
            let Some(request) = server.recv_timeout(Duration::from_millis(50)).unwrap() else {
                continue;
            };
            let (status, body) = match request.method().as_str() {
                "GET" => (200, r#"{"version":1,"instances":[]}"#),
                "POST" => {
                    // Ensure the guest actually starts before rejecting registration
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !worker_pid.exists() && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(10));
                    }
                    if worker_accepted
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                            count.checked_sub(1)
                        })
                        .is_ok()
                    {
                        (204, "")
                    } else {
                        (403, "registration denied by test registry")
                    }
                }
                "DELETE" => (403, "withdrawal denied by test registry"),
                method => panic!("unexpected registry method: {method}"),
            };
            request
                .respond(tiny_http::Response::from_string(body).with_status_code(status))
                .unwrap();
        }
    });

    // Both the foreground launcher and its parent must retain the server's cause
    for (parent, heartbeats) in [(false, 0), (true, 0), (false, 1), (true, 1)] {
        accepted.store(heartbeats, Ordering::SeqCst);
        let mut command = Command::new(&executable);
        if parent {
            command.arg("start");
        }
        command
            .args(["--headless", "--json", "--timeout", "20", "--image"])
            .arg(directory.path().join("image.ark"))
            .arg("--kernel")
            .arg(directory.path().join("kernel"))
            .arg("--initrd")
            .arg(directory.path().join("initrd"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("CI", "1")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY");
        let output = bounded(&mut command, &pid_file);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "parent={parent}, heartbeats={heartbeats}: {stderr}"
        );
        assert!(
            stderr.contains("HTTP 403: registration denied by test registry"),
            "{stderr}"
        );
        assert!(!stderr.contains("error[timeout]"), "{stderr}");
        if parent {
            let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["error"]["code"], "stopped-unexpectedly");
        }

        // Even a refused withdrawal must leave the guest reaped before exit
        let pid = fs::read_to_string(&pid_file).unwrap();
        assert!(
            !Path::new("/proc").join(pid.trim()).exists(),
            "guest {pid} survived"
        );
        fs::remove_file(&pid_file).unwrap();
    }
    finished.store(true, Ordering::SeqCst);
    worker.join().unwrap();
}

/// Bound a regression's wait and kill only fixture processes if it stops progressing.
fn bounded(command: &mut Command, pid_file: &Path) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            if let Ok(pid) = fs::read_to_string(pid_file) {
                // The pid comes only from the guest this fixture just launched
                let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
            }
            panic!("the registry failure did not reach the caller within 10 s");
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}
