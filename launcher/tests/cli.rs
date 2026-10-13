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

#[cfg(unix)]
#[path = "terminal/mod.rs"]
mod terminal;

#[cfg(unix)]
#[path = "../src/ipc/local.rs"]
#[allow(dead_code)] // only its listener serves the fixtures
mod local;

#[cfg(unix)]
#[path = "../src/ipc/http.rs"]
#[allow(dead_code)] // only its request reader serves the fixtures
mod http;

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
/// On Unix, the calling test holds [`local::PROCESS_TEST`] through this call.
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

/// The version is one plain line, or one JSON field, with nothing on stderr.
#[test]
fn test_version_is_one_line_or_one_json_field() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    let json = run(&["--json", "--version"]);
    assert!(json.status.success(), "JSON version: {}", stderr(&json));
    assert!(json.stderr.is_empty(), "JSON version stderr");
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(document.as_object().unwrap().len(), 1, "version document");
    let version = document["version"].as_str().unwrap();
    if let Some(stamp) = option_env!("ARK_EMULATOR_VERSION") {
        assert_eq!(version, stamp, "build script's stamp");
    }
    for flag in ["--version", "-V"] {
        let output = run(&[flag]);
        assert!(output.status.success(), "{flag}: {}", stderr(&output));
        assert!(output.stderr.is_empty(), "{flag} stderr");
        assert_eq!(
            stdout(&output),
            format!("ark-emulator {version}\n"),
            "{flag}"
        );
    }
}

/// A list sorts by port, and a start on a booted image prints its note and
/// keeps its whole JSON.
#[cfg(unix)]
#[test]
fn test_running_emulator_results() {
    use sha2::{Digest as _, Sha256};
    use std::os::unix::ffi::OsStrExt as _;
    use std::time::Duration;

    // Keep the fixture and its commands apart from any live emulator
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    if std::env::var_os("ARK_EMULATOR_RESULT_TEST").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "test_running_emulator_results", "--nocapture"])
            .env("ARK_EMULATOR_RESULT_TEST", "1")
            .env(local::TEST_NAMESPACE, local::identity())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture: {}{}",
            stdout(&output),
            stderr(&output)
        );
        return;
    }

    // The registry knows the invented image by its path's digest
    let directory = tempfile::TempDir::new().unwrap();
    let home = directory.path().canonicalize().unwrap();
    let image = home.join("lab.ark");
    std::fs::write(&image, []).unwrap();
    let digest = format!("{:x}", Sha256::digest(image.as_os_str().as_bytes()));
    let server = local::Server::bind_test("registry-18180").unwrap();
    // The expiry is 2026-11-12 at 23:30 UTC
    let instance = serde_json::json!({
        "port": 18181, "disk": "lab.ark", "disk_id": &digest[..16],
        "ready": true, "name": "Lab Emulator", "serial": "emulator-1234567-1",
        "env": "develop", "expiry": 1_794_526_200_u64,
    });
    let unready = serde_json::json!({
        "port": 18182, "disk": "other.ark", "disk_id": "0123456789abcdef", "ready": false,
    });
    let listing = serde_json::json!({"version": 1, "instances": [unready, instance]});

    // Serve the expected reads without waiting for a guest or a timer
    let worker = std::thread::spawn(move || {
        for _ in 0..18 {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(request.method().as_str(), "GET", "registry request");
            request
                .respond(tiny_http::Response::from_string(listing.to_string()))
                .unwrap();
        }
    });
    let invoke = |args: &[&str]| {
        Command::new(support::executable())
            .args(args)
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("LC_ALL", "en_US.UTF-8")
            .env("TZ", "UTC-1")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .output()
            .unwrap()
    };

    // The local date crosses midnight, and the image stays out of the table
    let list = invoke(&["list"]);
    assert!(list.status.success(), "list: {}", stderr(&list));
    assert_eq!(
        stdout(&list),
        "LOCATOR         NAME          SERIAL              ENVIRONMENT  READY  EXPIRES\nemulator:18181  Lab Emulator  emulator-1234567-1  develop      yes    2026-11-13\nemulator:18182  -             -                   -            no     -\n",
        "list result"
    );
    let json = invoke(&["list", "--json"]);
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(json.status.success(), "JSON list: {}", stderr(&json));
    assert_eq!(document["emulators"][0]["image"], "lab.ark", "JSON image");
    assert_eq!(
        document["emulators"][0]["expires"], "2026-11-12T23:30:00Z",
        "UTC expiry"
    );
    assert_eq!(document["emulators"][1]["port"], 18182, "port order");

    // A booted image gets its note before the titled block
    let started = invoke(&["start", "--image", image.to_str().unwrap()]);
    assert!(started.status.success(), "start: {}", stderr(&started));
    assert_eq!(
        stderr(&started),
        "note: lab.ark is already booted\n\n",
        "already booted note"
    );
    assert_eq!(
        stdout(&started),
        "Lab Emulator\n  Locator      emulator:18181\n  Serial       emulator-1234567-1\n  Image        ~/lab.ark\n  Environment  develop\n  Expires      2026-11-13 00:30 +01:00\n",
        "start result"
    );
    let json = invoke(&["start", "--image", image.to_str().unwrap(), "--json"]);
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(json.status.success(), "JSON start: {}", stderr(&json));
    assert_eq!(document["image"], "lab.ark", "JSON image name");
    assert_eq!(
        document["path"],
        image.display().to_string(),
        "JSON full path"
    );
    assert_eq!(document["ready"], true, "JSON readiness");
    assert_eq!(document["created"], false, "JSON image creation");
    assert_eq!(document["started"], false, "JSON boot result");
    assert_eq!(
        document["expires"], "2026-11-12T23:30:00Z",
        "JSON start expiry"
    );
    assert_eq!(
        stderr(&json),
        "{\"event\":\"note\",\"message\":\"lab.ark is already booted\"}\n",
        "JSON note"
    );
    // Selection errors name the current command and the first matching locator
    for command in ["stop", "button press", "button release"] {
        for selector in [None, Some("emulator")] {
            let mut arguments: Vec<_> = command.split_whitespace().collect();
            arguments.extend(selector);
            let hint = format!(
                "name one, as in `ark-emulator {command} emulator:18182`{}",
                if command == "stop" {
                    ", or pass --all"
                } else {
                    ""
                },
            );
            let reading = invoke(&arguments);
            assert_eq!(reading.status.code(), Some(3), "{arguments:?}");
            assert!(reading.stdout.is_empty(), "{arguments:?}: no result");
            let message = stderr(&reading)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                message.contains(&format!("hint: {hint}")),
                "{arguments:?}: {message}"
            );
            arguments.push("--json");
            let json = invoke(&arguments);
            assert_eq!(json.status.code(), Some(3), "{arguments:?}");
            let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
            assert_eq!(
                document["error"]["code"], "ambiguous-emulator",
                "{arguments:?}"
            );
            assert_eq!(
                document["error"]["hints"],
                serde_json::json!([hint]),
                "{arguments:?}"
            );
        }
    }
    worker.join().unwrap();
}

/// The doctor shows the home directory as `~` for reading, while JSON keeps
/// every path whole.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn test_doctor_shortens_home_paths_only_for_reading() {
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    // Keep the settings and the image away from the owner's own
    let directory = tempfile::TempDir::new().unwrap();
    let test_home = directory.path().join("home");
    let data_root = if cfg!(target_os = "macos") {
        test_home.join("Library/Application Support")
    } else {
        test_home.join(".local/share")
    };
    let data = data_root.join("bio.dark.emulator");
    let image = test_home.join("arks/lab.ark");
    let settings = data.join("settings.toml");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::File::create(&image)
        .unwrap()
        .set_len(19_864_223_744)
        .unwrap();
    std::fs::write(
        &settings,
        format!("version = 1\ndisk = '{}'\n", image.display()),
    )
    .unwrap();
    let invoke = |json: bool| {
        let mut command = Command::new(support::executable());
        command
            .arg("doctor")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("LC_ALL", "en_US.UTF-8")
            .env("HOME", &test_home)
            .env("XDG_DATA_HOME", &data_root);
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    };

    // Both a present and a missing image keep their full path in JSON
    for present in [true, false] {
        if !present {
            std::fs::remove_file(&image).unwrap();
        }
        let reading = invoke(false);
        let text = stdout(&reading);
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let data_path = if cfg!(target_os = "macos") {
            "~/Library/Application Support/bio.dark.emulator"
        } else {
            "~/.local/share/bio.dark.emulator"
        };
        assert!(
            flat.contains(&format!("✓ data {data_path}")),
            "present={present}: {text}"
        );
        assert!(
            flat.contains(&format!("✓ settings {data_path}/settings.toml")),
            "present={present}: {text}"
        );
        assert!(
            flat.contains(if present {
                "✓ image ~/arks/lab.ark, 18.5 GiB"
            } else {
                "✗ image ~/arks/lab.ark is not there"
            }),
            "present={present}: {text}"
        );
        assert!(
            flat.contains(&format!("✓ logs {data_path}/logs")),
            "logs, present={present}: {text}"
        );
        assert!(
            !text.contains(&test_home.display().to_string()),
            "reading home, present={present}: {text}"
        );
        assert!(
            text.starts_with("This computer\n") && !text.ends_with("\n\n"),
            "stdout separators, present={present}: {text}"
        );
        assert!(
            !text.contains('\x1b'),
            "pipe has no ANSI, present={present}"
        );
        if !reading.status.success() {
            assert!(
                stderr(&reading).starts_with("\nerror["),
                "error follows the report, present={present}: {}",
                stderr(&reading)
            );
        }

        let json = invoke(true);
        let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let mut keys: Vec<_> = document
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["checks", "version"], "JSON keys, present={present}");
        let checks = document["checks"].as_array().unwrap();
        assert_eq!(
            checks
                .iter()
                .map(|row| row["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "ark-emulator",
                "qemu",
                "firmware",
                "acceleration",
                "data",
                "settings",
                "image",
                "logs",
                "registry",
                "ports"
            ],
            "check order, present={present}"
        );
        for check in checks {
            let mut keys: Vec<_> = check
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["detail", "hint", "name", "result"],
                "check keys: {check}"
            );
        }
        let version = document["version"].as_str().unwrap();
        assert_eq!(
            checks[0]["result"], "skip",
            "lookup is disabled, present={present}"
        );
        assert_eq!(
            checks[0]["detail"],
            format!(
                "{version}, not looked up {}",
                if version.ends_with("-local") {
                    "for a local build"
                } else {
                    "under CI"
                }
            ),
            "version detail, present={present}"
        );
        for (name, expected) in [
            ("data", data.display().to_string()),
            ("settings", settings.display().to_string()),
            (
                "image",
                format!(
                    "{}{}",
                    image.display(),
                    if present {
                        ", 18.5 GiB"
                    } else {
                        " is not there"
                    }
                ),
            ),
            ("logs", data.join("logs").display().to_string()),
        ] {
            let check = document["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == name)
                .unwrap();
            assert_eq!(check["detail"], expected, "{name}, present={present}");
        }
        if checks[1]["result"] == "ok" {
            let detail = checks[1]["detail"].as_str().unwrap();
            let (version, binary) = detail.split_once(" at ").expect("QEMU version at binary");
            assert!(!version.contains(' '), "QEMU version: {detail}");
            assert!(!binary.is_empty(), "QEMU binary: {detail}");
        }
        for line in stderr(&json).lines() {
            let event: serde_json::Value =
                serde_json::from_str(line).expect("one JSON event per line");
            assert!(
                event["event"].is_string(),
                "JSON event, present={present}: {line}"
            );
            assert_ne!(
                event["event"], "hint",
                "doctor keeps error hints inside the error event"
            );
            if event["event"] == "error" {
                let first = checks.iter().find(|row| row["result"] == "fail").unwrap();
                assert_eq!(
                    event["error"]["message"], first["detail"],
                    "first failure, present={present}"
                );
            }
        }
    }

    // A blocked log directory reports its path and hint, and later checks still run
    let logs = data.join("logs");
    std::fs::remove_dir(&logs).unwrap();
    std::fs::write(&logs, b"blocked").unwrap();
    let json = invoke(true);
    assert!(!json.status.success(), "blocked logs fail the command");
    let document: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let checks = document["checks"].as_array().unwrap();
    assert_eq!(checks[7]["name"], "logs", "logs order");
    assert_eq!(checks[7]["result"], "fail", "blocked logs");
    assert!(
        checks[7]["detail"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{} is not writable: ", logs.display())),
        "full log path: {}",
        checks[7]
    );
    assert_eq!(
        checks[7]["hint"], "check the permissions on the logs directory",
        "logs hint"
    );
    assert_eq!(
        checks[9]["name"], "ports",
        "checks continue after a logs failure"
    );
    let reading = invoke(false);
    let text = stdout(&reading);
    assert!(text.contains("✗ logs"), "logs failure: {text}");
    assert!(
        !text.contains(&test_home.display().to_string()),
        "reading home: {text}"
    );
    assert!(
        text.contains("    hint: check the permissions on the logs directory\n"),
        "logs fix: {text}"
    );
}

/// The private update entry point does nothing and prints nothing under CI.
#[test]
fn test_update_entry_point_is_silent_under_ci() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    let output = run(&["__update"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// A published build prints the kept answer's note, and a local build never
/// reads it.
#[cfg(unix)]
#[test]
fn test_update_note_preserves_command_output_and_excludes_noncommands() {
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
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

    // Published builds repeat the note without changing the result or exit
    let message = format!(
        "Ark Emulator {newest} is available, this is {version}; download it from https://github.com/dark-bio/emulator"
    );
    for case in [
        vec!["list"],
        vec!["button", "press", "emulator:0"],
        vec!["button", "press", "emulator:0", "--hold"],
        vec!["button", "press", "emulator:0", "--hold-for", "1"],
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
                if version.pre.as_str() == "local" {
                    assert_eq!(text, stderr(&baseline), "local build: {arguments:?}");
                } else if json {
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
        vec!["button", "press", "--hold-for", "-1"],
        vec!["button", "press", "--hold-for", "0"],
        vec!["button", "press", "--hold", "--hold-for", "1"],
        vec!["button", "press", "--release-after", "0"],
        vec!["button", "release", "--hold-for", "1"],
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

    // A local build's entry points leave even a stale answer alone
    if version.pre.as_str() == "local" {
        let stale = b"{\"asked\":\"2000-01-01T00:00:00Z\",\"newest\":\"99.0.0\"}";
        std::fs::write(cache.join("update.json"), stale).unwrap();
        for arguments in [vec!["list"], vec!["__update"], vec!["doctor", "--json"]] {
            let output = invoke(&arguments, None);
            assert!(!stderr(&output).contains("is available"), "{arguments:?}");
            if arguments[0] == "__update" {
                assert!(output.status.success(), "local detached lookup");
                assert!(
                    output.stdout.is_empty() && output.stderr.is_empty(),
                    "silent lookup"
                );
            }
            if arguments[0] == "doctor" {
                let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                let check = document["checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|check| check["name"] == "ark-emulator")
                    .unwrap();
                assert_eq!(check["result"], "skip", "local doctor lookup");
                assert_eq!(
                    check["detail"],
                    format!("{version}, not looked up for a local build"),
                    "local doctor reason"
                );
                assert_eq!(document["version"], version.to_string(), "doctor stamp");
            }
            assert_eq!(
                std::fs::read(cache.join("update.json")).unwrap(),
                stale,
                "{arguments:?}"
            );
        }
    }
}

/// Every way of asking for help prints the same page, under --json too.
#[test]
fn test_both_help_forms_answer_on_every_command() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    for command in std::iter::once("").chain(COMMANDS).chain(["button"]) {
        let mut arguments: Vec<_> = command.split_whitespace().collect();
        arguments.push("-h");
        let scan = run(&arguments);
        assert_eq!(scan.status.code(), Some(0), "{command} -h");
        assert!(scan.stderr.is_empty(), "{command} -h stderr");

        *arguments.last_mut().unwrap() = "--help";
        let contract = run(&arguments);
        assert_eq!(contract.status.code(), Some(0), "{command} --help");
        assert_eq!(stdout(&scan), stdout(&contract), "{command} help flags");
        assert_eq!(
            stdout(&contract).contains("  Requires  "),
            command != "button",
            "{command} contract"
        );

        let mut arguments = vec!["help"];
        arguments.extend(command.split_whitespace());
        let page = run(&arguments);
        assert!(page.status.success(), "{command} help command");
        assert_eq!(stdout(&contract), stdout(&page), "{command}");
        arguments.push("--json");
        let json = run(&arguments);
        assert!(json.status.success(), "{command} JSON help");
        assert_eq!(stdout(&json), stdout(&page), "{command} text under JSON");
    }
}

/// Nested commands retain global flags, reject incomplete input and open no UI.
#[test]
fn test_button_group_and_invalid_arguments() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    let group = run(&["button", "--help"]);
    assert!(group.status.success(), "button help");
    assert!(
        !stdout(&group).contains("Contract:"),
        "group has no contract"
    );
    assert!(
        stdout(&group).contains("Each subcommand has its own requirements and output"),
        "group footer"
    );
    assert_eq!(
        stdout(&group),
        stdout(&run(&["help", "button"])),
        "group help spelling"
    );
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
        assert!(stdout(&output).contains("cli_pressed"), "{arguments:?}");
    }
}

/// Each button spelling sends its hold and prints its result, read and in JSON.
#[cfg(unix)]
#[test]
fn test_button_press_modes_and_release_results() {
    use std::time::Duration;

    // Keep every command and listener apart from any running emulator
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    if std::env::var_os("ARK_EMULATOR_BUTTON_TEST").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_button_press_modes_and_release_results",
                "--nocapture",
            ])
            .env("ARK_EMULATOR_BUTTON_TEST", "1")
            .env(local::TEST_NAMESPACE, local::identity())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture: {}{}",
            stdout(&output),
            stderr(&output)
        );
        return;
    }

    // Each spelling takes its own route and keeps the result's fields
    for (arguments, path, pressed, seconds, reading) in [
        (
            vec!["button", "press"],
            "/v1/button/press/0",
            false,
            Some(0),
            "  Button  up\n",
        ),
        (
            vec!["button", "press", "--hold"],
            "/v1/button/press",
            true,
            None,
            "  Button  down\n",
        ),
        (
            vec!["button", "press", "--hold-for", "5"],
            "/v1/button/press/5",
            true,
            Some(5),
            "  Button  down, released in 5s\n",
        ),
        (
            vec!["button", "release"],
            "/v1/button/release",
            false,
            None,
            "  Button  up\n",
        ),
    ] {
        for json in [false, true] {
            let registry = local::Server::bind_test("registry-18180").unwrap();
            let id = local::identity();
            let control = local::Server::bind_test(&format!("c-{id}")).unwrap();
            let listing = serde_json::json!({
                "version": 1,
                "instances": [{
                    "port": 18181, "disk": "lab.ark", "disk_id": "0123456789abcdef",
                    "ready": true, "control": {"id": id},
                }],
            });
            let outcome = serde_json::json!({
                "pressed": pressed, "cli_pressed": pressed, "changed": true,
                "release_after_seconds": seconds,
            });

            // Confirm the request before replying, without running a release timer
            let worker = std::thread::spawn(move || {
                let request = registry
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), "GET", "registry method");
                request
                    .respond(tiny_http::Response::from_string(listing.to_string()))
                    .unwrap();

                let request = control
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), "GET", "snapshot method");
                assert_eq!(request.url(), "/v1/button", "snapshot route");
                let held = path == "/v1/button/release";
                let snapshot = serde_json::json!({
                    "generation": "7", "connected": true, "pressed": held, "cli_pressed": held,
                });
                request
                    .respond(tiny_http::Response::from_string(snapshot.to_string()))
                    .unwrap();

                let request = control
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method().as_str(), "POST", "button method");
                assert_eq!(request.url(), path, "button route");
                let generation = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("X-Ark-Generation"));
                assert_eq!(
                    generation.map(|header| header.value.as_str()),
                    Some("7"),
                    "connection generation"
                );
                assert!(request.body().is_empty(), "button request body");
                request
                    .respond(tiny_http::Response::from_string(outcome.to_string()))
                    .unwrap();
            });
            let mut arguments = arguments.clone();
            arguments.extend(["--timeout", "2"]);
            if json {
                arguments.push("--json");
            }
            let output = run(&arguments);
            worker.join().unwrap();

            // Both outputs report the acknowledged state without narration
            assert!(
                output.status.success(),
                "{arguments:?}: {}",
                stderr(&output)
            );
            assert!(
                output.stderr.is_empty(),
                "{arguments:?}: {}",
                stderr(&output)
            );
            if json {
                let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(
                    document,
                    serde_json::json!({
                        "locator": "emulator:18181", "pressed": pressed, "cli_pressed": pressed,
                        "changed": true, "release_after_seconds": seconds,
                    }),
                    "{arguments:?}"
                );
            } else {
                assert_eq!(stdout(&output), reading, "{arguments:?}");
            }
        }
    }
}

/// The hold options belong to press, and --hold-for takes whole seconds from 1.
#[test]
fn test_button_hold_option_validation_and_help() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    // Both help pages list the options only under the command that takes them
    for flag in ["-h", "--help"] {
        for (action, expected) in [("press", true), ("release", false)] {
            let page = stdout(&run(&["button", action, flag]));
            let (_, options) = page.split_once("\nOptions:\n").unwrap();
            let options = options.split("\n\n").next().unwrap();
            for option in ["--hold ", "--hold-for <SECONDS>"] {
                assert_eq!(
                    options.contains(option),
                    expected,
                    "{action} {flag}: {option}"
                );
            }
            assert!(
                !options.contains("--release-after"),
                "{action} {flag}: removed option"
            );
        }
    }

    // Both duration limits reach selection, using a port no emulator can hold
    for seconds in ["1", "4294967295"] {
        let output = run(&[
            "button",
            "press",
            "emulator:0",
            "--hold-for",
            seconds,
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(3), "{seconds}");
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["error"]["code"], "no-emulator", "{seconds}");
    }

    // Invalid durations fail as usage errors before discovery or hardware access
    for seconds in ["0", "-1", "1.5", "NaN", "inf", "4294967296", "seconds"] {
        let output = run(&["button", "press", "--hold-for", seconds, "--json"]);
        assert_eq!(output.status.code(), Some(2), "{seconds}");
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["error"]["code"], "usage", "{seconds}");
    }
    for arguments in [
        vec!["button", "release", "--hold"],
        vec!["button", "release", "--hold-for", "1"],
        vec!["button", "release", "--release-after", "1"],
    ] {
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
    }
}

#[test]
fn test_a_usage_error_takes_the_house_shape_on_both_outputs() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
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
    assert_eq!(
        stderr(&json).lines().count(),
        1,
        "error hints are not separate events"
    );
    assert_eq!(
        event["error"]["hints"],
        serde_json::json!(["see `ark-emulator list --help`"]),
        "error event hints"
    );
    assert_eq!(
        document["error"], event["error"],
        "the result and event carry the same error"
    );
}

/// A typo keeps clap's suggestion, and other mistakes point at the command's
/// help.
#[test]
fn test_usage_messages_and_recovery_hints() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    for (arguments, message, hint) in [
        (
            vec!["statr"],
            "unknown command 'statr'",
            "a similar command exists: `ark-emulator start`",
        ),
        (
            vec!["button", "pres"],
            "unknown command 'pres'",
            "a similar command exists: `ark-emulator button press`",
        ),
        (
            vec!["list", "--jsno"],
            "unknown option '--jsno'",
            "a similar option exists: `--json`",
        ),
        (
            vec!["start", "--env", "stagng"],
            "unknown environment 'stagng'",
            "a similar value exists: `staging`",
        ),
        (
            vec!["--log", "trce", "list"],
            "unknown value 'trce' for --log",
            "a similar value exists: `trace`",
        ),
        (
            vec!["completions", "bsh"],
            "unknown shell 'bsh'",
            "a similar value exists: `bash`",
        ),
        (
            vec!["help", "imgaes"],
            "unknown help topic 'imgaes'",
            "a similar topic exists: `ark-emulator help images`",
        ),
        (
            vec!["help", "statr"],
            "unknown help command 'statr'",
            "a similar command exists: `ark-emulator start`",
        ),
        (
            vec!["help", "button", "pres"],
            "unknown help command 'pres'",
            "a similar command exists: `ark-emulator button press`",
        ),
        (
            vec!["help", "bogus"],
            "unknown help topic 'bogus'",
            "see `ark-emulator help --help`",
        ),
        (
            vec!["help", "button", "bogus"],
            "unknown help command 'bogus'",
            "see `ark-emulator button --help`",
        ),
        (
            vec!["completions"],
            "missing the shell for completions",
            "see `ark-emulator completions --help`",
        ),
        (
            vec!["button"],
            "missing a command",
            "see `ark-emulator button --help`",
        ),
        (
            vec!["start", "--kernel", "kernel"],
            "missing the initramfs (--initrd)",
            "see `ark-emulator start --help`",
        ),
        (
            vec!["start", "--initrd", "initrd"],
            "missing the kernel image (--kernel)",
            "see `ark-emulator start --help`",
        ),
        (
            vec!["button", "press", "--hold-for"],
            "missing a value for --hold-for",
            "see `ark-emulator button press --help`",
        ),
        (
            vec!["button", "press", "--hold-for", "0"],
            "hold duration must be whole seconds from 1 to 4294967295",
            "see `ark-emulator button press --help`",
        ),
        (
            vec!["button", "press", "--hold", "--hold-for", "5"],
            "--hold cannot be combined with --hold-for <SECONDS>",
            "see `ark-emulator button press --help`",
        ),
        (
            vec!["button", "press", "--hold-for", "5", "--hold"],
            "--hold-for <SECONDS> cannot be combined with --hold",
            "see `ark-emulator button press --help`",
        ),
        (
            vec!["button", "press", "--release-after", "0"],
            "unknown option '--release-after'",
            "see `ark-emulator button press --help`",
        ),
        (
            vec!["list", "--bogus"],
            "unknown option '--bogus'",
            "see `ark-emulator list --help`",
        ),
        (
            vec!["button", "release", "one", "two"],
            "unexpected argument 'two'",
            "see `ark-emulator button release --help`",
        ),
        (
            vec!["--timeout", "17", "button", "release", "--bogus"],
            "unknown option '--bogus'",
            "see `ark-emulator button release --help`",
        ),
        (
            vec!["--timeout=17", "button", "-v", "release", "--bogus"],
            "unknown option '--bogus'",
            "see `ark-emulator button release --help`",
        ),
        (
            vec!["--headless", "list"],
            "boot options belong to `ark-emulator start` or to no command",
            "see `ark-emulator --help`",
        ),
        (
            vec!["list", "--headless"],
            "boot options belong to `ark-emulator start` or to no command",
            "see `ark-emulator --help`",
        ),
        (
            vec!["button", "release", "--image=lab.ark"],
            "boot options belong to `ark-emulator start` or to no command",
            "see `ark-emulator --help`",
        ),
    ] {
        // Clap finds both button actions near this spelling and keeps their order
        let hints = if arguments.last() == Some(&"pres") {
            vec![
                "a similar command exists: `ark-emulator button release`",
                hint,
            ]
        } else {
            vec![hint]
        };
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: usage stdout");
        let text = stderr(&output)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            text,
            format!("error[usage]: {message} hint: {}", hints.join(" hint: ")),
            "{arguments:?}"
        );

        // JSON carries the same failure on stdout and in its single stderr event
        let mut json_arguments = vec!["--json"];
        json_arguments.extend_from_slice(&arguments);
        let output = run(&json_arguments);
        assert_eq!(output.status.code(), Some(2), "{json_arguments:?}");
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            document["error"],
            serde_json::json!({"code": "usage", "message": message, "hints": hints}),
            "{json_arguments:?}"
        );
        assert_eq!(
            event["error"], document["error"],
            "{json_arguments:?}: event"
        );
    }
}

/// The manual holds every contract, every linked entry point and the errors
/// topic.
#[test]
fn test_the_manual_keeps_its_shape_and_its_links_in_a_pipe() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    let manual = run(&["help", "--all"]);
    assert_eq!(manual.status.code(), Some(0), "manual exit");
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
    assert_eq!(
        text.matches("\n  Requires  ").count(),
        COMMANDS.len() + 1,
        "contract rows"
    );
    assert!(text.contains("\nErrors\n"), "errors topic in manual");
    assert!(!text.contains("\n  $ "), "examples carry no prompt");
    for line in text.lines() {
        assert!(line.chars().count() <= 80, "{line}");
    }
}

/// Every topic prints as text under --json, the errors topic included.
#[test]
fn test_topics_print_text_on_both_outputs() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    for (topic, heading) in [
        ("agents", "Driving ark-emulator from an AI agent"),
        ("images", "Images"),
        ("registry", "The registry of running emulators"),
        ("output", "Output"),
        ("errors", "Errors"),
    ] {
        let reading = run(&["help", topic]);
        let json = run(&["help", topic, "--json"]);
        assert!(
            reading.status.success() && json.status.success(),
            "{topic}: status"
        );
        assert!(
            reading.stderr.is_empty() && json.stderr.is_empty(),
            "{topic}: stderr"
        );
        assert!(
            stdout(&reading).starts_with(&format!("{heading}\n\n")),
            "{topic}: heading"
        );
        assert_eq!(reading.stdout, json.stdout, "{topic}: text under JSON");
    }
}

#[test]
fn test_completions_are_shell_text_even_under_json() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    let script = run(&["--json", "completions", "zsh"]);
    assert_eq!(script.status.code(), Some(0));
    let text = stdout(&script);
    assert!(text.contains("ark-emulator"), "{text}");
    assert!(!text.trim_start().starts_with('{'), "{text}");
}

/// Help and completions report a closed output pipe as a local I/O failure.
#[test]
fn test_help_and_completions_fail_when_the_output_pipe_is_closed() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    for arguments in [
        vec!["help"],
        vec!["help", "--all"],
        vec!["completions", "zsh"],
        vec!["-h"],
        vec!["--help"],
        vec!["list", "-h"],
        vec!["list", "--help"],
        vec!["button", "press", "-h"],
        vec!["button", "press", "--help"],
    ] {
        for json in [false, true] {
            // Close the reader before spawning so even a short page must fail
            let (reader, writer) = std::io::pipe().unwrap();
            drop(reader);
            let mut command = Command::new(support::executable());
            if json {
                command.arg("--json");
            }
            let output = command
                .args(&arguments)
                .env("NO_COLOR", "1")
                .env("CI", "1")
                .stdout(writer)
                .output()
                .unwrap();

            // Both output modes retain the house error shape on stderr
            assert_eq!(output.status.code(), Some(1), "{arguments:?}, json={json}");
            if json {
                let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(event["event"], "error", "{arguments:?}");
                assert_eq!(event["error"]["code"], "io", "{arguments:?}");
            } else {
                assert!(stderr(&output).starts_with("error[io]: "), "{arguments:?}");
            }
        }
    }
}

/// Headless is a boot option in both modes, never a management option.
#[test]
fn test_headless_option_placement_and_help() {
    #[cfg(unix)]
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
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
    let _exclusive = local::PROCESS_TEST.lock().unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    std::fs::write(
        directory.path().join("bio.dark.emulator"),
        "not a directory",
    )
    .unwrap();
    for log in [false, true] {
        let mut command = Command::new(support::executable());
        command.args(["--headless", "--json"]);
        if log {
            command.args(["--log", "debug"]);
        }
        let output = command
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_DATA_HOME", directory.path())
            .env("CI", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "log={log}");
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
                .contains("could not create the data directory"),
            "log={log}, startup cause"
        );
        assert_eq!(
            events.iter().any(|event| event["event"] == "log"),
            log,
            "foreground logs need --log"
        );
    }
}
