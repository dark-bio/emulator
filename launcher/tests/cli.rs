// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The built executable against what its help promises: both help forms on
//! every command, a mistake typed at the command line in the house error
//! shape on both outputs, and the manual with its links and its footers.
//! Nothing here boots a guest, so it runs wherever the binary builds.

use std::process::{Command, Output};

/// Every command the tool has, in the order the root lists them.
const COMMANDS: [&str; 7] = [
    "start",
    "list",
    "stop",
    "wipe",
    "doctor",
    "completions",
    "help",
];

/// Run the built binary with `arguments` and hand back what it did.
fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ark-emulator"))
        .args(arguments)
        .env("NO_COLOR", "1")
        .output()
        .expect("the binary runs")
}

/// What a run put on stdout.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What a run put on stderr.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn test_both_help_forms_answer_on_every_command() {
    for command in COMMANDS {
        let scan = run(&[command, "-h"]);
        assert_eq!(scan.status.code(), Some(0), "{command} -h");
        assert!(!stdout(&scan).contains("Requires:"), "{command} -h");

        let contract = run(&[command, "--help"]);
        assert_eq!(contract.status.code(), Some(0), "{command} --help");
        assert!(stdout(&contract).contains("Requires:"), "{command} --help");

        let page = run(&["help", command]);
        assert_eq!(stdout(&contract).trim(), stdout(&page).trim(), "{command}");
    }
}

#[test]
fn test_a_usage_error_takes_the_house_shape_on_both_outputs() {
    let plain = run(&["stop"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(stdout(&plain).is_empty());
    let err = stderr(&plain);
    assert!(err.starts_with("error[usage]: "), "{err}");
    assert!(err.contains("\nhint: "), "{err}");

    let json = run(&["--json", "stop"]);
    assert_eq!(json.status.code(), Some(2));
    let document: serde_json::Value =
        serde_json::from_str(&stdout(&json)).expect("one document on stdout");
    assert_eq!(document["error"]["code"], "usage");
    let first = stderr(&json);
    let first = first.lines().next().unwrap_or_default();
    let event: serde_json::Value = serde_json::from_str(first).expect("one event per line");
    assert_eq!(event["event"], "error");
    assert_eq!(event["error"]["code"], "usage");
}

#[test]
fn test_the_manual_keeps_its_shape_and_its_links_in_a_pipe() {
    let manual = run(&["help", "--all"]);
    assert_eq!(manual.status.code(), Some(0));
    let text = stdout(&manual);
    for link in [
        "https://github.com/dark-bio/cli",
        "https://github.com/dark-bio/examples",
        "ark help agents",
    ] {
        assert!(text.contains(link), "{link}");
    }
    assert_eq!(text.matches("\nRequires: ").count(), COMMANDS.len() + 1);
    assert!(!text.contains("\n  $ "), "examples carry no prompt");
    for line in text.lines() {
        assert!(line.chars().count() <= 80, "{line}");
    }
}

#[test]
fn test_completions_are_shell_text_even_under_json() {
    let script = run(&["--json", "completions", "zsh"]);
    assert_eq!(script.status.code(), Some(0));
    let text = stdout(&script);
    assert!(text.contains("ark-emulator"), "{text}");
    assert!(!text.trim_start().starts_with('{'), "{text}");
}
