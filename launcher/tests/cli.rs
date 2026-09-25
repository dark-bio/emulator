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
        .env("CI", "1")
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

/// The private update entry point does nothing and prints nothing under CI.
#[test]
fn test_update_entry_point_is_silent_under_ci() {
    let output = run(&["__update"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// A fresh isolated answer prints one note while help and invalid invocations stay quiet.
#[cfg(unix)]
#[test]
fn test_update_note_preserves_command_output_and_excludes_noncommands() {
    // macOS uses Library/Caches while other Unix targets use XDG_CACHE_HOME
    let directory = tempfile::TempDir::new().unwrap();
    let home = directory.path().join("home");
    let xdg_cache = directory.path().join("cache");
    let cache = if cfg!(target_os = "macos") {
        home.join("Library/Caches/bio.dark.emulator")
    } else {
        xdg_cache.join("bio.dark.emulator")
    };
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let version = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    let newest = format!("{}.0.0", version.major + 1);
    let answer = serde_json::to_vec(&serde_json::json!({
        "asked": time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339).unwrap(),
        "newest": newest,
    }))
    .unwrap();
    std::fs::write(cache.join("update.json"), &answer).unwrap();
    let invoke = |arguments: &[&str], ci: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ark-emulator"));
        command
            .args(arguments)
            .env_remove("CI")
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", &xdg_cache)
            .env("NO_COLOR", "1");
        if let Some(ci) = ci {
            command.env("CI", ci);
        }
        command.output().unwrap()
    };

    // The note repeats in both outputs without changing the result or exit
    let message = format!(
        "Ark Emulator {newest} is available, this is {version}; download it from https://github.com/dark-bio/emulator"
    );
    for json in [false, true] {
        let mut arguments = vec!["list"];
        if json {
            arguments.push("--json");
        }
        let baseline = invoke(&arguments, Some("1"));
        for ci in [None, Some("")] {
            let output = invoke(&arguments, ci);
            assert_eq!(
                output.status.code(),
                baseline.status.code(),
                "json={json}, CI={ci:?}"
            );
            assert_eq!(output.stdout, baseline.stdout, "json={json}, CI={ci:?}");
            let text = stderr(&output);
            if json {
                let events: Vec<serde_json::Value> = text
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(
                    events[0],
                    serde_json::json!({"event":"note", "message":message})
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["event"] == "note")
                        .count(),
                    1
                );
            } else {
                assert!(text.starts_with("note: Ark Emulator"), "{text}");
                assert!(
                    text.split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .starts_with(&format!("note: {message}")),
                    "{text}"
                );
                assert_eq!(
                    text.lines()
                        .filter(|line| line.starts_with("note:"))
                        .count(),
                    1
                );
            }
        }

        // Quiet and CI suppress the notice without changing the result
        arguments.push("-q");
        let quiet = invoke(&arguments, None);
        assert_eq!(quiet.stdout, baseline.stdout);
        assert_eq!(quiet.status.code(), baseline.status.code());
        assert!(!stderr(&quiet).contains("is available"));
        assert!(!stderr(&baseline).contains("is available"));
    }

    // These paths must neither announce nor refresh; doctor and the window never run here
    for arguments in [
        vec!["help"],
        vec!["help", "--all"],
        vec!["-h"],
        vec!["--help"],
        vec!["list", "--help"],
        vec!["completions", "zsh"],
        vec!["--version"],
        vec!["--json", "--version"],
        vec!["list", "--bogus"],
        vec!["list", "--timeout", "0"],
        vec!["list", "-q", "-v"],
        vec!["--version", "list"],
        vec!["--image", "unused.ark", "list"],
        vec!["__update", "extra"],
    ] {
        let output = invoke(&arguments, None);
        assert!(!stderr(&output).contains("is available"), "{arguments:?}");
    }
    assert_eq!(std::fs::read(cache.join("update.json")).unwrap(), answer);
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
    let plain = run(&["list", "--bogus"]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(stdout(&plain).is_empty());
    let err = stderr(&plain);
    assert!(err.starts_with("error[usage]: "), "{err}");
    assert!(err.contains("\nhint: "), "{err}");

    let json = run(&["--json", "list", "--bogus"]);
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
    // Wrapped, so a phrase is looked for across line breaks.
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for link in [
        "https://github.com/dark-bio/cli",
        "https://github.com/dark-bio/examples",
        "ark help agents",
    ] {
        assert!(flat.contains(link), "{link}");
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
