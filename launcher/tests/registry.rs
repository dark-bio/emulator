// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Exercises registry failures and direct shutdown with isolated fake guests.

#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[path = "../src/ipc/local.rs"]
#[allow(dead_code)] // Scripted peers use the production transport without every client helper.
mod local;

/// Serializes scenarios that own the public discovery port within this test process.
static REGISTRY_TEST: Mutex<()> = Mutex::new(());

/// Refused initial and later publications stop the guest and reach either caller.
#[test]
fn test_registration_refusals_reach_standalone_and_start_commands() {
    let _exclusive = REGISTRY_TEST.lock().unwrap();
    // Never send test publications into an emulator's live registry
    let listener = match TcpListener::bind(("127.0.0.1", 18180)) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("registry lifecycle test skipped: port 18180 is in use");
            return;
        }
        Err(err) => panic!("could not reserve the test registry: {err}"),
    };
    let _reservation = listener;
    let server = local::Server::bind("registry-18180").unwrap();

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
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
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

/// Direct stop shuts down real launchers after their registry disappears.
#[test]
fn test_direct_stop_all_survives_registry_loss_after_selection() {
    let _exclusive = REGISTRY_TEST.lock().unwrap();
    let listener = match TcpListener::bind(("127.0.0.1", 18180)) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("direct stop lifecycle test skipped: port 18180 is in use");
            return;
        }
        Err(err) => panic!("could not reserve the test registry: {err}"),
    };
    let _reservation = listener;
    let server = local::Server::bind("registry-18180").unwrap();

    // Publish both launches, then remove discovery as soon as the CLI selects them
    let (published, ready) = mpsc::channel();
    let registry = thread::spawn(move || {
        let mut instances = std::collections::BTreeMap::new();
        loop {
            let mut request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(request.url(), "/v1/instances");
            match request.method().as_str() {
                "GET" => {
                    let body = serde_json::json!({"version":1,"instances":instances.values().collect::<Vec<_>>()});
                    request
                        .respond(tiny_http::Response::from_string(body.to_string()))
                        .unwrap();
                    if instances.len() == 2 {
                        break;
                    }
                }
                "POST" => {
                    let instance: serde_json::Value =
                        serde_json::from_reader(request.as_reader()).unwrap();
                    assert!(instance["control"]["id"].is_string());
                    let fresh = instances
                        .insert(instance["port"].as_u64().unwrap(), instance)
                        .is_none();
                    request.respond(tiny_http::Response::empty(204)).unwrap();
                    if fresh {
                        published.send(()).unwrap();
                    }
                }
                method => panic!("unexpected registry request: {method}"),
            }
        }
    });

    // Each source-build fixture resolves its own fake guest and writes its own pid
    let directory = tempfile::TempDir::new().unwrap();
    let ports = [
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
    ];
    let mut launchers = Vec::new();
    let mut expected = Vec::new();
    for (index, reserved) in ports.into_iter().enumerate() {
        let port = reserved.local_addr().unwrap().port();
        let root = directory.path().join(index.to_string());
        fs::create_dir(&root).unwrap();
        fs::copy(
            env!("CARGO_BIN_EXE_ark-emulator"),
            root.join("ark-emulator"),
        )
        .unwrap();
        let guest = root.join("qemu-system-guest");
        fs::write(
            &guest,
            "#!/bin/sh\necho $$ > \"$0.pid\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).unwrap();
        for file in ["image.ark", "kernel", "initrd"] {
            fs::write(root.join(file), []).unwrap();
        }
        let child = Command::new(root.join("ark-emulator"))
            .args([
                "--headless",
                "--json",
                "--port",
                &port.to_string(),
                "--image",
            ])
            .arg(root.join("image.ark"))
            .arg("--kernel")
            .arg(root.join("kernel"))
            .arg("--initrd")
            .arg(root.join("initrd"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("CI", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(reserved);
        launchers.push(Launcher { child, root });
        expected.push(format!("emulator:{port}"));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    // Wait for both guest processes to start before exercising their termination
    let deadline = Instant::now() + Duration::from_secs(5);
    while launchers
        .iter()
        .any(|launcher| !launcher.root.join("qemu-system-guest.pid").exists())
    {
        assert!(Instant::now() < deadline, "fake guests did not start");
        thread::sleep(Duration::from_millis(10));
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_ark-emulator"));
    command
        .args(["stop", "--all", "--json", "--timeout", "5"])
        .env("CI", "1")
        .env("XDG_DATA_HOME", directory.path().join("cli"));
    let output = bounded(&mut command, &directory.path().join("unused.pid"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    expected.sort_by_key(|locator| locator.split_once(':').unwrap().1.parse::<u16>().unwrap());
    assert_eq!(result, serde_json::json!({"stopped":expected}));
    registry.join().unwrap();

    // The control acknowledgement must result in both launcher and guest exit
    for launcher in &mut launchers {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = launcher.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "launcher survived its stop");
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(launcher.root.join("qemu-system-guest.pid")).unwrap();
        let process = Path::new("/proc").join(pid.trim());
        // A reparented guest can briefly remain a zombie until init reaps it
        if process.exists() {
            let status = fs::read_to_string(process.join("stat")).unwrap_or_default();
            assert!(status.contains(") Z "), "guest {pid} still runs: {status}");
        }
    }

    /// A fixture launcher killed on test failure so its guest cannot remain running.
    struct Launcher {
        /// Foreground process started by this test.
        child: std::process::Child,
        /// Isolated sidecar and process id files.
        root: std::path::PathBuf,
    }
    impl Drop for Launcher {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A refused later stop retains the earlier confirmed result without registry fallback.
#[test]
fn test_direct_stop_all_preserves_partial_output_on_refusal() {
    let _exclusive = REGISTRY_TEST.lock().unwrap();
    let listener = match TcpListener::bind(("127.0.0.1", 18180)) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("partial stop test skipped: port 18180 is in use");
            return;
        }
        Err(err) => panic!("could not reserve the test registry: {err}"),
    };
    let _reservation = listener;
    let registry = local::Server::bind("registry-18180").unwrap();

    // The lower guest port stops; the other control endpoint refuses the request
    let mut guests = vec![
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
        TcpListener::bind(("127.0.0.1", 0)).unwrap(),
    ];
    guests.sort_by_key(|guest| guest.local_addr().unwrap().port());
    let first_port = guests[0].local_addr().unwrap().port();
    let mut instances = Vec::new();
    let mut workers = Vec::new();
    for (index, guest) in guests.into_iter().enumerate() {
        let id = local::identity();
        let control = local::Server::bind(&format!("c-{id}")).unwrap();
        instances.push(serde_json::json!({
            "port":guest.local_addr().unwrap().port(),"disk":format!("{index}.ark"),"disk_id":format!("{index}"),"ready":false,
            "control":{"id":id}
        }));
        workers.push(thread::spawn(move || {
            let request = control
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(request.url(), "/v1/stop");
            let (status, body) = if index == 0 {
                (202, r#"{"stopping":true}"#)
            } else {
                (403, r#"{"error":"stop refused by test launcher"}"#)
            };
            request
                .respond(tiny_http::Response::from_string(body).with_status_code(status))
                .unwrap();
            drop(guest);
        }));
    }
    let registry_worker = thread::spawn(move || {
        let request = registry
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.method(), &tiny_http::Method::Get);
        request
            .respond(tiny_http::Response::from_string(
                serde_json::json!({"version":1,"instances":instances}).to_string(),
            ))
            .unwrap();
    });

    // stdout retains completed work while stderr reports the failed target
    let directory = tempfile::TempDir::new().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ark-emulator"));
    command
        .args(["stop", "--all", "--json", "--timeout", "5"])
        .env("CI", "1")
        .env("XDG_DATA_HOME", directory.path());
    let output = bounded(&mut command, &directory.path().join("unused.pid"));
    assert_eq!(output.status.code(), Some(3));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result,
        serde_json::json!({"stopped":[format!("emulator:{first_port}")]})
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("control-unreachable"), "{stderr}");
    assert!(
        stderr.contains("HTTP 403: stop refused by test launcher"),
        "{stderr}"
    );
    registry_worker.join().unwrap();
    for worker in workers {
        worker.join().unwrap();
    }
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
