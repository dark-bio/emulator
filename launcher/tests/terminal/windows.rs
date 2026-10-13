// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Tests that send real control events, in a console only the test process
//! owns.

use std::fs::File;
use std::io::Read as _;
use std::os::windows::io::AsRawHandle as _;
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, ENABLE_PROCESSED_INPUT, GenerateConsoleCtrlEvent,
    GetConsoleMode, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

use super::{local, support};

/// A command reaped even when its console handling fails an assertion.
struct Process(
    /// The running executable owned by this fixture.
    Child,
);

impl Process {
    /// Waits for the event to end the command, with a failure deadline.
    fn exited(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "console event did not end the command"
            );
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

/// The console's flags, read through the handle the fixture keeps.
fn mode(input: &File) -> u32 {
    let mut mode = 0;
    // Sound since input stays open and mode is a writable word
    assert_ne!(
        unsafe { GetConsoleMode(input.as_raw_handle(), &mut mode) },
        0,
        "console mode"
    );
    mode
}

/// Keeps the controller alive when its private console broadcasts an event.
unsafe extern "system" fn keep_controller(_event: u32) -> i32 {
    1
}

/// Ctrl-C and Ctrl-Break end a waiting command and restore the prompt's input.
#[test]
fn test_console_events_end_commands_and_restore_prompt() {
    // The controller gets its own console, so broadcasts cannot reach the runner
    if std::env::var_os("ARK_EMULATOR_CONSOLE_TEST").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal::test_console_events_end_commands_and_restore_prompt",
                "--nocapture",
            ])
            .env("ARK_EMULATOR_CONSOLE_TEST", "1")
            .env(local::TEST_NAMESPACE, local::identity())
            .creation_flags(CREATE_NEW_CONSOLE)
            .output()
            .unwrap();
        assert!(output.status.success(), "console fixture: {output:?}");
        return;
    }
    // Sound since this callback has the documented ABI and lives for the process
    assert_ne!(
        unsafe { SetConsoleCtrlHandler(Some(keep_controller), 1) },
        0,
        "controller handler"
    );
    let input = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let original = mode(&input);
    assert_ne!(
        original & ENABLE_PROCESSED_INPUT,
        0,
        "ordinary console input"
    );
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("prompt.ark");
    let contents = b"console events must leave this image alone";
    std::fs::write(&image, contents).unwrap();

    for event in [CTRL_C_EVENT, CTRL_BREAK_EVENT] {
        for arguments in [
            vec!["start", "--image", image.to_str().unwrap()],
            vec!["stop"],
            vec!["list"],
            vec!["button", "release"],
            vec!["wipe", image.to_str().unwrap()],
        ] {
            let prompt = arguments[0] == "wipe";
            let server = local::Server::bind_test("registry-18180").unwrap();
            let mut child = Process(
                Command::new(support::executable())
                    .args(&arguments)
                    .env("CI", "1")
                    .env("NO_COLOR", "1")
                    .stdin(input.try_clone().unwrap())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );

            // A pending registry request shows the command installed its handler
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if prompt {
                request
                    .respond(tiny_http::Response::from_string(
                        r#"{"version":1,"instances":[]}"#,
                    ))
                    .unwrap();
                let mut stderr = child.0.stderr.take().unwrap();
                let (shown, question) = mpsc::channel();
                let reader = std::thread::spawn(move || {
                    let mut text = Vec::new();
                    let mut byte = [0];
                    while stderr.read(&mut byte).unwrap() != 0 {
                        text.push(byte[0]);
                        if text.ends_with(b"(y/N) ") {
                            shown.send(()).unwrap();
                            return;
                        }
                    }
                    panic!("no prompt: {}", String::from_utf8_lossy(&text));
                });
                question.recv_timeout(Duration::from_secs(5)).unwrap();
                reader.join().unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while mode(&input) & ENABLE_PROCESSED_INPUT != 0 {
                    assert!(Instant::now() < deadline, "prompt did not enter raw mode");
                    std::thread::yield_now();
                }
            }

            // Sound since this process owns the isolated console and both event
            // values are documented broadcasts to its attached processes.
            assert_ne!(
                unsafe { GenerateConsoleCtrlEvent(event, 0) },
                0,
                "event {event}"
            );
            assert_eq!(
                child.exited().code(),
                Some(130),
                "{arguments:?}, event {event}"
            );
            assert_eq!(
                mode(&input),
                original,
                "{arguments:?}, event {event}, input restored"
            );
            assert_eq!(
                std::fs::read(&image).unwrap(),
                contents,
                "{arguments:?}, event {event}, image"
            );
        }
    }
}
