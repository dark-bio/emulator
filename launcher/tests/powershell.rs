// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Exercises the shipped script with both Windows PowerShell and PowerShell 7.

#![cfg(windows)]

use std::fs;
use std::path::Path;
use std::process::Command;

/// The script preserves arguments, waits for output, and returns the application's exit code.
#[test]
fn test_powershell_waits_and_preserves_arguments_and_exit_codes() {
    let install = tempfile::Builder::new()
        .prefix("Ark Emulator café ")
        .tempdir()
        .unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_ark-emulator"),
        install.path().join("ark-emulator.exe"),
    )
    .unwrap();
    let script = install.path().join("ark-emulator.ps1");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/packaging/windows/ark-emulator.ps1"),
        &script,
    )
    .unwrap();

    for shell in ["powershell.exe", "pwsh.exe"] {
        let invoke = |arguments: &[&str]| {
            Command::new(shell)
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&script)
                .args(arguments)
                .env("NO_COLOR", "1")
                .output()
                .unwrap()
        };
        let help = invoke(&["help", "start"]);
        assert_eq!(help.status.code(), Some(0), "{shell}");
        assert!(String::from_utf8_lossy(&help.stdout).contains("Requires:"));

        for argument in [
            "",
            "two words",
            "café",
            "a\"b",
            "tail\\",
            "space and slash\\",
        ] {
            let output = invoke(&["--json", "list", argument]);
            assert_eq!(output.status.code(), Some(2), "{shell}: {argument}");
            let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(document["error"]["code"], "usage");
            assert!(
                document["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(argument),
                "{shell}: {argument}: {document}"
            );
            let stderr = String::from_utf8(output.stderr).unwrap();
            let event: serde_json::Value =
                serde_json::from_str(stderr.lines().next().unwrap()).unwrap();
            assert_eq!(event["event"], "error");
            assert_eq!(event["error"], document["error"]);
        }
    }
}
