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

mod support;

/// Every command the tool has, in the order the root lists them.
const COMMANDS: [&str; 9] = [
    "start",
    "list",
    "stop",
    "button press",
    "button release",
    "wipe",
    "doctor",
    "completions",
    "help",
];

/// Run the built binary with `arguments` and hand back what it did.
fn run(arguments: &[&str]) -> Output {
    Command::new(support::executable())
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

    // Headless startup must fail before binding a registry or starting QEMU
    let data = directory.path().join("data");
    let application_data = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/bio.dark.emulator")
    } else {
        data.join("bio.dark.emulator")
    };
    std::fs::create_dir_all(application_data.parent().unwrap()).unwrap();
    std::fs::write(application_data, "not a directory").unwrap();

    // A fresh cached version exercises notices without starting network lookups
    let output = run(&["--json", "--version"]);
    assert!(output.status.success());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let version = semver::Version::parse(document["version"].as_str().unwrap()).unwrap();
    let newest = format!("{}.0.0", version.major + 1);
    let answer = serde_json::to_vec(&serde_json::json!({
        "asked": time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339).unwrap(),
        "newest": newest,
    }))
    .unwrap();
    std::fs::write(cache.join("update.json"), &answer).unwrap();
    let invoke = |arguments: &[&str], ci: Option<&str>| {
        let mut command = Command::new(support::executable());
        command
            .args(arguments)
            .env_remove("CI")
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", &xdg_cache)
            .env("XDG_DATA_HOME", &data)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
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
    for case in [
        vec!["list"],
        vec!["button", "press", "emulator:0"],
        vec!["button", "press", "emulator:0", "--release-after", "0"],
        vec!["button", "release", "emulator:0"],
        vec!["--headless"],
    ] {
        for json in [false, true] {
            let mut arguments = case.clone();
            if json {
                arguments.push("--json");
            }
            let baseline = invoke(&arguments, Some("1"));
            if case[0] == "--headless" {
                assert_eq!(baseline.status.code(), Some(1));
                assert!(stderr(&baseline).contains("could not create the data directory"));
            } else if case[0] == "button" {
                assert_eq!(baseline.status.code(), Some(3));
            }
            for ci in [None, Some("")] {
                let output = invoke(&arguments, ci);
                assert_eq!(
                    output.status.code(),
                    baseline.status.code(),
                    "{case:?}, json={json}, CI={ci:?}"
                );
                assert_eq!(
                    output.stdout, baseline.stdout,
                    "{case:?}, json={json}, CI={ci:?}"
                );
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
    }

    // These paths must neither announce nor refresh; doctor and the window never run here
    for arguments in [
        vec!["help"],
        vec!["help", "--all"],
        vec!["-h"],
        vec!["--help"],
        vec!["list", "--help"],
        vec!["button", "press", "--help"],
        vec!["button", "release", "-h"],
        vec!["--headless", "--help"],
        vec!["completions", "zsh"],
        vec!["--version"],
        vec!["--json", "--version"],
        vec!["list", "--bogus"],
        vec!["list", "--timeout", "0"],
        vec!["button", "press", "--release-after", "-1"],
        vec!["button", "release", "--release-after", "0"],
        vec!["--headless", "button", "press"],
        vec!["--headless", "--kernel", "kernel"],
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
        let mut arguments: Vec<_> = command.split_whitespace().collect();
        arguments.push("-h");
        let scan = run(&arguments);
        assert_eq!(scan.status.code(), Some(0), "{command} -h");
        assert!(!stdout(&scan).contains("Requires:"), "{command} -h");

        *arguments.last_mut().unwrap() = "--help";
        let contract = run(&arguments);
        assert_eq!(contract.status.code(), Some(0), "{command} --help");
        assert!(stdout(&contract).contains("Requires:"), "{command} --help");

        let mut arguments = vec!["help"];
        arguments.extend(command.split_whitespace());
        let page = run(&arguments);
        assert_eq!(stdout(&contract).trim(), stdout(&page).trim(), "{command}");
    }
}

/// Nested commands retain global flags, reject incomplete input and open no UI.
#[test]
fn test_button_group_and_invalid_arguments() {
    let group = run(&["button", "--help"]);
    assert!(group.status.success());
    assert!(!stdout(&group).contains("Requires:"));
    assert!(stdout(&group).contains("Each subcommand has its own contract"));
    assert_eq!(stdout(&group), stdout(&run(&["help", "button"])));
    for arguments in [
        vec!["--json", "button"],
        vec!["button", "press", "--all", "--json"],
        vec!["button", "--json", "release", "--timeout", "0"],
        vec!["button", "press", "one", "two", "--json"],
        vec!["--headless", "button", "press", "--json"],
    ] {
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["error"]["code"], "usage", "{arguments:?}");
    }
    for arguments in [
        vec!["--json", "button", "press", "--help"],
        vec!["button", "--json", "press", "--help"],
        vec!["button", "release", "--json", "--help"],
    ] {
        let output = run(&arguments);
        assert!(output.status.success(), "{arguments:?}");
        assert!(stdout(&output).contains("cli_pressed"));
    }
}

/// Timed release is a press-only option with a bounded nonnegative duration.
#[test]
fn test_timed_release_option_validation_and_help() {
    // Both help forms advertise the option only on the command that accepts it
    for flag in ["-h", "--help"] {
        assert!(stdout(&run(&["button", "press", flag])).contains("--release-after"));
        assert!(!stdout(&run(&["button", "release", flag])).contains("--release-after"));
    }

    // Both duration limits reach selection, using a port no emulator can hold
    for seconds in ["0", "4294967295"] {
        let output = run(&[
            "button",
            "press",
            "emulator:0",
            "--release-after",
            seconds,
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(3), "{seconds}");
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["error"]["code"], "no-emulator");
    }

    // Invalid durations fail as usage errors before discovery or hardware access
    for seconds in ["-1", "1.5", "NaN", "inf", "4294967296", "seconds"] {
        let output = run(&["button", "press", "--release-after", seconds, "--json"]);
        assert_eq!(output.status.code(), Some(2), "{seconds}");
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["error"]["code"], "usage");
    }
    let output = run(&["button", "release", "--release-after", "1", "--json"]);
    assert_eq!(output.status.code(), Some(2));
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

/// Headless is a boot option in both modes, never a management option.
#[test]
fn test_headless_option_placement_and_help() {
    for arguments in [vec!["--help"], vec!["start", "--help"]] {
        let output = run(&arguments);
        assert!(output.status.success());
        assert!(stdout(&output).contains("--headless"));
    }
    for arguments in [
        vec!["--headless", "list"],
        vec!["list", "--headless"],
        vec!["start", "--headless", "--kernel", "kernel"],
    ] {
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(stderr(&output).starts_with("error[usage]:"));
    }
}

/// Startup failures in headless mode use CLI errors without touching a display.
#[cfg(target_os = "linux")]
#[test]
fn test_headless_startup_error_without_a_display_or_no_input_flag() {
    let directory = tempfile::TempDir::new().unwrap();
    std::fs::write(
        directory.path().join("bio.dark.emulator"),
        "not a directory",
    )
    .unwrap();
    let output = Command::new(support::executable())
        .args(["--headless", "--json"])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_DATA_HOME", directory.path())
        .env("CI", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let events: Vec<serde_json::Value> = stderr(&output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let error = events
        .iter()
        .find(|event| event["event"] == "error")
        .unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("could not create the data directory")
    );
}
