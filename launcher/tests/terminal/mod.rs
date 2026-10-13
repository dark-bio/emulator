// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Tests of the prompt's cleanup and the discovery notice, on a pseudo-terminal.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{local, support};

/// A private terminal whose slave side stays open, so its mode reads after the
/// child exits.
struct Terminal {
    /// The side the test reads output from and types keys into.
    master: File,
    /// The side the child runs on.
    slave: File,
    /// Output collected so far.
    output: String,
}

impl Terminal {
    /// Opens an isolated terminal with ordinary echo and line input.
    fn new() -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Sound since the descriptors are writable and optional pointers are null
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(size),
                )
            },
            0,
            "open pseudo-terminal"
        );
        // Sound since openpty returned two new descriptors owned by these files
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        Self {
            master,
            slave,
            output: String::new(),
        }
    }

    /// Reads until `literal` appears, or fails at the test's deadline.
    fn until(&mut self, text: &str, deadline: Instant) {
        while !self.output.contains(text) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut descriptor = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Sound since the descriptor and its one-element poll buffer stay live
            let ready = unsafe {
                libc::poll(
                    &mut descriptor,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            assert!(ready > 0, "waiting for {text:?}: {:?}", self.output);
            let mut bytes = [0; 4096];
            let count = self.master.read(&mut bytes).unwrap();
            assert!(count > 0, "terminal closed before {text:?}");
            self.output
                .push_str(&String::from_utf8_lossy(&bytes[..count]));
        }
    }

    /// The terminal's current flags, read from the slave side.
    fn mode(&self) -> libc::termios {
        // Sound since tcgetattr fills one writable termios before it is read
        unsafe {
            let mut mode = std::mem::zeroed();
            assert_eq!(
                libc::tcgetattr(self.slave.as_raw_fd(), &mut mode),
                0,
                "read mode"
            );
            mode
        }
    }
}

/// A child reaped even when an assertion fails.
struct Process(
    /// The running executable owned by this fixture.
    Child,
);

impl Process {
    /// Waits for the exit, failing at a deadline so a broken signal path never
    /// hangs the suite.
    fn exited(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::yield_now();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs a fixture in a process of its own, so its namespace never changes
/// under another test.
fn isolated(name: &str) -> bool {
    if std::env::var_os("ARK_EMULATOR_TERMINAL_TEST").is_some() {
        return false;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("ARK_EMULATOR_TERMINAL_TEST", "1")
        .env(local::TEST_NAMESPACE, local::identity())
        .output()
        .unwrap();
    assert!(output.status.success(), "fixture: {output:?}");
    true
}

/// SIGINT and SIGTERM from outside restore the terminal's mode before the
/// question exits.
#[test]
fn test_wipe_prompt_restores_terminal_on_signals() {
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    if isolated("terminal::test_wipe_prompt_restores_terminal_on_signals") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("prompt.ark");
    let contents = b"the prompt must leave this image alone";
    std::fs::write(&image, contents).unwrap();
    for (signal, code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143), (0, 130)] {
        let mut terminal = Terminal::new();
        let original = terminal.mode();
        let mut child = Process(
            Command::new(support::executable())
                .arg("wipe")
                .arg(&image)
                .env("CI", "1")
                .env("NO_COLOR", "1")
                .env("TERM", "xterm")
                .stdin(terminal.slave.try_clone().unwrap())
                .stdout(Stdio::null())
                .stderr(terminal.slave.try_clone().unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        terminal.until("(y/N) ", deadline);
        while terminal.mode().c_lflag & (libc::ICANON | libc::ECHO) != 0 {
            assert!(Instant::now() < deadline, "prompt did not enter raw mode");
            std::thread::yield_now();
        }
        if signal == 0 {
            terminal.master.write_all(&[3]).unwrap();
        } else {
            // Sound since the child remains owned and kill carries no pointers
            assert_eq!(
                unsafe { libc::kill(child.0.id() as i32, signal) },
                0,
                "signal {signal}"
            );
        }
        assert_eq!(child.exited().code(), Some(code), "signal {signal}");
        let restored = terminal.mode();
        assert_eq!(
            restored.c_iflag, original.c_iflag,
            "signal {signal}, input flags"
        );
        assert_eq!(
            restored.c_oflag, original.c_oflag,
            "signal {signal}, output flags"
        );
        assert_eq!(
            restored.c_cflag, original.c_cflag,
            "signal {signal}, control flags"
        );
        // The kernel may mark pending input for retyping when line input returns
        assert_eq!(
            restored.c_lflag & !libc::PENDIN,
            original.c_lflag & !libc::PENDIN,
            "signal {signal}, local flags"
        );
        assert_eq!(
            restored.c_cc, original.c_cc,
            "signal {signal}, control keys"
        );
        assert_eq!(
            std::fs::read(&image).unwrap(),
            contents,
            "signal {signal}, image"
        );
    }
}

/// With stderr blocked, the answer and the error still leave the terminal
/// restored and a signal able to end the process.
#[test]
fn test_wipe_prompt_restores_input_before_blocked_output() {
    use std::io::{self, PipeReader, PipeWriter};
    use std::os::unix::process::ExitStatusExt as _;

    /// Fills the pipe, then restores blocking writes for the child.
    fn fill(writer: &mut PipeWriter) {
        let fd = writer.as_raw_fd();
        // Sound since the writer stays open and fcntl receives no pointers
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0, "read pipe flags");
        // Sound since these flags belong to this open pipe
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0,
            "set nonblocking writes"
        );
        for bytes in [&[0; 4096][..], &[0][..]] {
            loop {
                match writer.write(bytes) {
                    Ok(count) => assert!(count > 0, "fill stderr"),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => panic!("fill stderr: {err}"),
                }
            }
        }
        // Sound since the descriptor stays open and flags came from F_GETFL
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags) },
            0,
            "restore blocking writes"
        );
    }

    /// Reads the question, which ends without a newline.
    fn question(reader: &mut PipeReader) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut text = Vec::new();
        while !text.ends_with(b"(y/N) ") {
            let mut descriptor = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            // Sound since the descriptor and its poll buffer stay live
            assert!(
                unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis() as i32) } > 0,
                "waiting for question: {text:?}"
            );
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            text.push(byte[0]);
        }
    }

    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    if isolated("terminal::test_wipe_prompt_restores_input_before_blocked_output") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("prompt.ark");
    let contents = b"blocked output must leave this image alone";
    std::fs::write(&image, contents).unwrap();
    for signal in [libc::SIGINT, libc::SIGTERM] {
        for key in [b'n', 3] {
            // Keep stderr separate from the terminal that supplies input
            let mut terminal = Terminal::new();
            let original = terminal.mode();
            let (mut reader, mut writer) = io::pipe().unwrap();
            let mut child = Process(
                Command::new(support::executable())
                    .arg("wipe")
                    .arg(&image)
                    .env("CI", "1")
                    .env("NO_COLOR", "1")
                    .stdin(terminal.slave.try_clone().unwrap())
                    .stdout(Stdio::null())
                    .stderr(writer.try_clone().unwrap())
                    .spawn()
                    .unwrap(),
            );
            question(&mut reader);
            let deadline = Instant::now() + Duration::from_secs(5);
            while terminal.mode().c_lflag & (libc::ICANON | libc::ECHO) != 0 {
                assert!(Instant::now() < deadline, "prompt did not enter raw mode");
                std::thread::yield_now();
            }
            fill(&mut writer);

            // Repeat the answer until the type-ahead window lets it through
            // Sound since the master stays open and these are descriptor flags
            let flags = unsafe { libc::fcntl(terminal.master.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0, "read terminal flags");
            // Sound since the descriptor stays open for this change
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        terminal.master.as_raw_fd(),
                        libc::F_SETFL,
                        flags | libc::O_NONBLOCK,
                    )
                },
                0,
                "set nonblocking input"
            );
            while terminal.mode().c_lflag & (libc::ICANON | libc::ECHO) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "input stayed raw before blocked output, key {key}"
                );
                match terminal.master.write(&[key]) {
                    Ok(count) => assert_eq!(count, 1, "key {key}"),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) => panic!("type key {key}: {err}"),
                }
                std::thread::yield_now();
            }

            // Leave stderr full while the restored signal action ends the process
            let restored = terminal.mode();
            assert_eq!(restored.c_iflag, original.c_iflag, "key {key}, input flags");
            assert_eq!(
                restored.c_oflag, original.c_oflag,
                "key {key}, output flags"
            );
            assert_eq!(
                restored.c_cflag, original.c_cflag,
                "key {key}, control flags"
            );
            assert_eq!(
                restored.c_lflag & !libc::PENDIN,
                original.c_lflag & !libc::PENDIN,
                "key {key}, local flags"
            );
            assert_eq!(restored.c_cc, original.c_cc, "key {key}, control keys");
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "key {key}, stderr remains blocked"
            );
            // Sound since the child remains owned and kill carries no pointers
            assert_eq!(
                unsafe { libc::kill(child.0.id() as i32, signal) },
                0,
                "signal {signal}"
            );
            let status = child.exited();
            assert_eq!(
                status
                    .code()
                    .or_else(|| status.signal().map(|signal| 128 + signal)),
                Some(128 + signal),
                "key {key}, signal {signal}"
            );
            assert_eq!(
                terminal.mode().c_lflag & (libc::ICANON | libc::ECHO),
                original.c_lflag & (libc::ICANON | libc::ECHO),
                "key {key}, mode after exit"
            );
            assert_eq!(std::fs::read(&image).unwrap(), contents, "key {key}, image");
        }
    }
}

/// A held registry reply shows the notice at once, and its end erases it.
#[test]
fn test_start_announces_delayed_registry_read() {
    use sha2::{Digest as _, Sha256};
    use std::os::unix::ffi::OsStrExt as _;

    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    if isolated("terminal::test_start_announces_delayed_registry_read") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().canonicalize().unwrap().join("running.ark");
    std::fs::write(&image, []).unwrap();
    let digest = format!("{:x}", Sha256::digest(image.as_os_str().as_bytes()));
    let listing = serde_json::json!({"version": 1, "instances": [{
        "port": 18181, "disk": "running.ark", "disk_id": &digest[..16], "ready": true,
    }]})
    .to_string();
    let server = local::Server::bind_test("registry-18180").unwrap();
    let mut terminal = Terminal::new();
    let mut command = Command::new(support::executable());
    command
        .arg("start")
        .arg("--image")
        .arg(&image)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(terminal.slave.try_clone().unwrap());
    let mut child = Process(command.spawn().unwrap());

    // Hold the reply until the terminal shows the command has printed
    let first = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    terminal.until(
        "Looking for running emulators",
        Instant::now() + Duration::from_millis(100),
    );
    first
        .respond(tiny_http::Response::from_string(listing.clone()))
        .unwrap();
    let ready = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    ready
        .respond(tiny_http::Response::from_string(listing))
        .unwrap();
    terminal.until(
        "\r\x1b[2Knote: running.ark is already booted",
        Instant::now() + Duration::from_secs(5),
    );
    assert!(
        child.exited().success(),
        "start result: {}",
        terminal.output
    );
}
