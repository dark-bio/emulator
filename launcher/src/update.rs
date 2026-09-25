// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Lookups of the newest published Ark Emulator and the note that announces it.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use semver::Version;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::diagnostics::log;
use crate::output::Output;

/// Hidden sole argument that runs the detached lookup and nothing else.
pub(crate) const ENTRY_POINT: &str = "__update";

/// Largest kept answer read from disk, in bytes.
const CACHE_LIMIT: u64 = 4 * 1024;

/// The kept answer, stamped when its lookup started.
#[derive(Deserialize, Serialize)]
struct Answer {
    /// When the last lookup started, whether or not it succeeded.
    #[serde(with = "time::serde::rfc3339")]
    asked: OffsetDateTime,
    /// Newest release found, absent until a lookup succeeds.
    newest: Option<Version>,
}

impl Answer {
    /// Reads the kept answer. An unreadable file counts as no answer.
    fn read(directory: &Path) -> Option<Self> {
        // Read at most 4 KiB, far more than an answer ever takes
        let file = File::open(directory.join("update.json")).ok()?;
        let mut bytes = Vec::new();
        file.take(CACHE_LIMIT).read_to_end(&mut bytes).ok()?;

        // Typed fields reject malformed versions and timestamps
        serde_json::from_slice(&bytes).ok()
    }

    /// Reports whether the answer is absent, stamped in the future, or an hour old.
    fn stale(answer: Option<&Self>, now: OffsetDateTime) -> bool {
        answer.is_none_or(|answer| {
            answer.asked > now || now - answer.asked >= time::Duration::hours(1)
        })
    }

    /// Replaces the kept answer through a renamed temporary file.
    fn write(&self, directory: &Path) -> io::Result<()> {
        // Serialize the answer as one line ending in a newline, and make sure
        // the cache directory exists
        let mut bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        bytes.push(b'\n');
        fs::create_dir_all(directory)?;

        // Rename only a completely written file over the kept answer
        let temporary = directory.join(format!(".update-{}.tmp", std::process::id()));
        let result = fs::write(&temporary, bytes)
            .and_then(|()| fs::rename(&temporary, directory.join("update.json")));
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// Words the note while the kept version is newer than the running one.
    fn note(&self, running: &Version, hint: &str) -> Option<String> {
        let newest = self.newest.as_ref()?;
        newest
            .cmp_precedence(running)
            .is_gt()
            .then(|| format!("Ark Emulator {newest} is available, this is {running}; {hint}"))
    }
}

/// Reports whether a nonempty `CI` turns off lookups and notes alike.
pub(crate) fn disabled() -> bool {
    std::env::var_os("CI").is_some_and(|value| !value.is_empty())
}

/// Parses the version stamped into this executable by Cargo.
pub(crate) fn running() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version is semver")
}

/// Resolves the app's cache without creating a Tauri context or touching a display.
pub(crate) fn directory() -> Option<PathBuf> {
    let cache = dirs::cache_dir()?;
    let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
        .expect("the embedded app configuration is JSON");
    Some(
        cache.join(
            config["identifier"]
                .as_str()
                .expect("the app has an identifier"),
        ),
    )
}

/// Prints the kept note and starts a detached lookup when one is due.
pub(crate) fn start(output: &Output, now: OffsetDateTime) {
    // Under CI nothing is read, printed or looked up
    if disabled() {
        return;
    }
    let Some(directory) = directory() else {
        log!("[launcher] update cache directory could not be located");
        return;
    };

    // Print the note even when its answer needs refreshing
    let answer = Answer::read(&directory);
    if let Some(note) = answer
        .as_ref()
        .and_then(|answer| answer.note(&running(), &hint()))
    {
        output.event("note", note);
    }
    if !Answer::stale(answer.as_ref(), now) {
        return;
    }

    // Claim before asking, so a failing network asks only once an hour
    if let Err(error) = claim(&directory, now) {
        log!("[launcher] update claim could not be written: {}", error);
        return;
    }

    // Give the copy its own process group and none of the caller's streams
    let result = std::env::current_exe().and_then(|executable| {
        let mut command = Command::new(executable);
        command
            .arg(ENTRY_POINT)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{
                CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
            };
            command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        command.spawn().map(drop)
    });
    if let Err(error) = result {
        log!("[launcher] update process could not be started: {}", error);
    }
}

/// Runs the silent detached lookup, ending within 30 s whatever happens.
pub(crate) fn run() {
    // Under CI the copy does nothing
    if disabled() {
        return;
    }
    let started = Instant::now();
    let asked = OffsetDateTime::now_utc();

    // Skip the lookup if nothing can enforce its total lifetime
    if thread::Builder::new()
        .name("ark-emulator-update-watchdog".into())
        .spawn(move || {
            thread::sleep(Duration::from_secs(30).saturating_sub(started.elapsed()));
            std::process::exit(0);
        })
        .is_err()
    {
        return;
    }

    // Keep a successful answer, stamped with this copy's start time
    let _ = refresh(directory().as_deref(), asked, Duration::from_secs(20));
}

/// Stamps a new attempt, keeping the version last found.
fn claim(directory: &Path, now: OffsetDateTime) -> io::Result<()> {
    Answer {
        asked: now,
        newest: Answer::read(directory).and_then(|answer| answer.newest),
    }
    .write(directory)
}

/// Looks up and keeps the newest release. Failure leaves the kept answer untouched.
pub(crate) fn refresh(
    directory: Option<&Path>,
    asked: OffsetDateTime,
    timeout: Duration,
) -> Result<Version, &'static str> {
    // A failed lookup leaves the kept version and attempt time untouched
    let newest = lookup(timeout)?;
    let answer = Answer {
        asked,
        newest: Some(newest.clone()),
    };

    // Doctor can report success even without a writable cache; the copy stays silent
    if let Some(directory) = directory {
        let _ = answer.write(directory);
    }
    Ok(newest)
}

/// Fetches only a parsed version; failure reasons never contain response text.
fn lookup(timeout: Duration) -> Result<Version, &'static str> {
    // Bound each network wait; a HEAD request has no response body to read
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent("ark-emulator")
        .timeout_resolve(Some(timeout))
        .timeout_connect(Some(timeout))
        .timeout_send_request(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .build()
        .into();

    // Inspect the redirect without following it or reading its body
    let response = agent
        .head("https://github.com/dark-bio/emulator/releases/latest")
        .call()
        .map_err(|_| "GitHub could not be reached")?;
    if !response.status().is_redirection() {
        return Err("GitHub returned an unexpected status");
    }
    response
        .headers()
        .get("Location")
        .and_then(|location| location.to_str().ok())
        .ok_or("GitHub returned no release redirect")
        .and_then(release)
}

/// Accepts only a strict stable version at this repository's exact release URL.
fn release(location: &str) -> Result<Version, &'static str> {
    location
        .strip_prefix("https://github.com/dark-bio/emulator/releases/tag/v")
        .and_then(|tag| Version::parse(tag).ok())
        .filter(|version| version.pre.is_empty())
        .ok_or("GitHub returned an invalid release redirect")
}

/// Words the upgrade advice for the way this executable was installed.
pub(crate) fn hint() -> String {
    // Check custom and standard Homebrew prefixes without depending on PATH
    let executable = std::env::current_exe().ok();
    let prefix = std::env::var_os("HOMEBREW_PREFIX").filter(|prefix| !prefix.is_empty());
    let prefixes: Vec<&Path> = prefix
        .as_deref()
        .map(Path::new)
        .into_iter()
        .chain([Path::new("/opt/homebrew"), Path::new("/usr/local")])
        .collect();

    // An install the tool cannot place gets the repository link
    executable
        .as_deref()
        .and_then(|executable| upgrade(executable, &prefixes))
        .map(|command| format!("upgrade with `{command}`"))
        .unwrap_or_else(|| "download it from https://github.com/dark-bio/emulator".into())
}

/// Recognizes a cask by its app bundle and a matching Homebrew Caskroom directory.
fn upgrade(executable: &Path, prefixes: &[&Path]) -> Option<&'static str> {
    // Resolve links before checking which app bundle holds the executable
    let executable = executable.canonicalize().ok()?;
    if !executable.ancestors().skip(1).any(|path| {
        path.file_name()
            .is_some_and(|name| name == "Ark Emulator.app")
    }) {
        return None;
    }

    // The cask copies the app, so its executable path alone also matches a DMG install
    prefixes
        .iter()
        .any(|prefix| prefix.join("Caskroom/ark-emulator").is_dir())
        .then_some("brew update && brew upgrade --cask ark-emulator")
}

/// Tests of the kept answer, response parsing and install detection.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;
    use time::format_description::well_known::Rfc3339;

    /// A lookup is due after an hour and for a future stamp.
    #[test]
    fn test_cache_staleness_uses_the_claim_time() {
        // Use a fixed instant so the boundary never depends on test runtime
        let directory = TempDir::new().unwrap();
        let cache = directory.path().join("cache");
        let now = OffsetDateTime::parse("2026-09-25T12:00:00Z", &Rfc3339).unwrap();
        assert!(Answer::stale(Answer::read(&cache).as_ref(), now));

        // Keep each case through the production writer and read it back
        for (case, asked, stale) in [
            ("under an hour", "2026-09-25T11:00:00.001Z", false),
            ("one hour", "2026-09-25T11:00:00Z", true),
            ("future", "2026-09-25T12:00:00.001Z", true),
            ("now", "2026-09-25T12:00:00Z", false),
        ] {
            Answer {
                asked: OffsetDateTime::parse(asked, &Rfc3339).unwrap(),
                newest: Some(Version::parse("0.2.3").unwrap()),
            }
            .write(&cache)
            .unwrap();
            assert_eq!(
                Answer::stale(Answer::read(&cache).as_ref(), now),
                stale,
                "{case}"
            );
        }

        // Replacing an answer leaves one file holding the documented shape
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.join("update.json")).unwrap()).unwrap();
        assert_eq!(
            stored,
            json!({"asked":"2026-09-25T12:00:00Z", "newest":"0.2.3"})
        );
        assert_eq!(fs::read_dir(cache).unwrap().count(), 1);
    }

    /// Malformed and unreadable answers never postpone a fresh lookup.
    #[test]
    fn test_invalid_cache_answers_are_stale() {
        // Keep malformed fields close to the documented cache shape
        let directory = TempDir::new().unwrap();
        let now = OffsetDateTime::parse("2026-09-25T12:00:00Z", &Rfc3339).unwrap();
        let path = directory.path().join("update.json");
        for (case, bytes) in [
            ("broken JSON", b"{".to_vec()),
            (
                "invalid time",
                br#"{"asked":"today","newest":"0.2.3"}"#.to_vec(),
            ),
            (
                "invalid version",
                br#"{"asked":"2026-09-25T12:00:00Z","newest":"latest"}"#.to_vec(),
            ),
            ("missing time", br#"{"newest":"0.2.3"}"#.to_vec()),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(
                Answer::stale(Answer::read(directory.path()).as_ref(), now),
                "{case}"
            );
        }

        // A directory in place of the answer exercises an unreadable file portably
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(Answer::stale(Answer::read(directory.path()).as_ref(), now));
    }

    /// Claims keep the previous version and require a writable cache.
    #[test]
    fn test_claim_preserves_the_previous_answer() {
        // Claim an empty cache before any lookup has succeeded
        let directory = TempDir::new().unwrap();
        let now = OffsetDateTime::parse("2026-09-25T12:00:00Z", &Rfc3339).unwrap();
        claim(directory.path(), now).unwrap();
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("update.json")).unwrap())
                .unwrap();
        assert_eq!(
            stored,
            json!({"asked":"2026-09-25T12:00:00Z", "newest":null})
        );
        assert!(!Answer::stale(Answer::read(directory.path()).as_ref(), now));

        // Publish an expired answer through the same atomic writer used by the worker
        Answer {
            asked: OffsetDateTime::parse("2026-09-25T10:00:00Z", &Rfc3339).unwrap(),
            newest: Some(Version::parse("0.2.3").unwrap()),
        }
        .write(directory.path())
        .unwrap();

        // A new claim keeps the known version and records the new attempt time
        claim(directory.path(), now).unwrap();
        let claimed = Answer::read(directory.path()).unwrap();
        assert_eq!(
            claimed.asked.format(&Rfc3339).unwrap(),
            "2026-09-25T12:00:00Z"
        );
        assert_eq!(claimed.newest.unwrap().to_string(), "0.2.3");

        // An unwritable cache path cannot produce a claim or start a request
        let file = directory.path().join("file");
        fs::write(&file, []).unwrap();
        assert!(claim(&file, now).is_err());
    }

    /// Only a newer kept version produces the note, regardless of claim age.
    #[test]
    fn test_note_requires_a_newer_kept_version() {
        // The old timestamp deliberately leaves the notice independent of freshness
        let mut answer = Answer {
            asked: OffsetDateTime::parse("2026-01-01T00:00:00Z", &Rfc3339).unwrap(),
            newest: None,
        };
        let running = Version::parse("0.2.2").unwrap();
        let hint = "upgrade with `brew update && brew upgrade --cask ark-emulator`";
        assert!(answer.note(&running, hint).is_none());
        for newest in ["0.2.1", "0.2.2", "0.2.2+different-build"] {
            answer.newest = Some(Version::parse(newest).unwrap());
            assert!(answer.note(&running, hint).is_none(), "{newest}");
        }

        // Both known and unknown installations keep the promised sentence
        answer.newest = Some(Version::parse("0.2.3").unwrap());
        assert_eq!(
            answer.note(&running, hint).unwrap(),
            "Ark Emulator 0.2.3 is available, this is 0.2.2; upgrade with `brew update && brew upgrade --cask ark-emulator`"
        );
        assert_eq!(
            answer
                .note(
                    &running,
                    "download it from https://github.com/dark-bio/emulator"
                )
                .unwrap(),
            "Ark Emulator 0.2.3 is available, this is 0.2.2; download it from https://github.com/dark-bio/emulator"
        );

        // A prerelease build compares itself with the same stable release
        assert_eq!(
            answer
                .note(&Version::parse("0.2.3-dev.9").unwrap(), hint)
                .unwrap(),
            "Ark Emulator 0.2.3 is available, this is 0.2.3-dev.9; upgrade with `brew update && brew upgrade --cask ark-emulator`"
        );
        assert!(
            answer
                .note(&Version::parse("0.2.4-dev.1").unwrap(), hint)
                .is_none()
        );
    }

    /// Stable redirects must name this repository and a strict release version.
    #[test]
    fn test_release_redirect_rejects_foreign_and_nonrelease_locations() {
        // Captured from releases/latest on 2026-09-25 (HTTP/2 302)
        assert_eq!(
            release("https://github.com/dark-bio/emulator/releases/tag/v0.2.1")
                .unwrap()
                .to_string(),
            "0.2.1"
        );
        for location in [
            "https://example.com/dark-bio/emulator/releases/tag/v0.2.1",
            "https://github.com/dark-bio/cli/releases/tag/v0.2.1",
            "http://github.com/dark-bio/emulator/releases/tag/v0.2.1",
            "https://github.com/dark-bio/emulator/releases/tag/v0.2.2-dev.1",
            "https://github.com/dark-bio/emulator/releases/tag/v0.2",
            "https://github.com/dark-bio/emulator/releases/tag/v00.2.1",
            "https://github.com/dark-bio/emulator/releases/tag/v0.2.1/extra",
            "https://github.com/dark-bio/emulator/releases/tag/v0.2.1?next=bad",
            "https://github.com/dark-bio/emulator/releases/tag/v0.2.1\n",
            "garbage",
        ] {
            assert!(release(location).is_err(), "{location:?}");
        }
    }

    /// A copied app needs the cask directory before it can recommend Homebrew.
    #[test]
    fn test_upgrade_commands_follow_the_installation_layout() {
        // Build the layout Homebrew's app and command_wrapper artifacts install
        let directory = TempDir::new().unwrap();
        let executable = directory
            .path()
            .join("Applications/Ark Emulator.app/Contents/MacOS/ark-emulator");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, []).unwrap();
        let prefix = directory.path().join("homebrew");
        let cask = prefix.join("Caskroom/ark-emulator");
        fs::create_dir_all(&cask).unwrap();
        assert_eq!(
            upgrade(&executable, &[&prefix]),
            Some("brew update && brew upgrade --cask ark-emulator")
        );

        // The same app without its Caskroom is indistinguishable from a DMG install
        fs::remove_dir(&cask).unwrap();
        assert_eq!(upgrade(&executable, &[&prefix]), None);

        // A plain executable gets no command even when the cask also exists
        fs::create_dir(&cask).unwrap();
        let plain = directory.path().join("ark-emulator");
        fs::write(&plain, []).unwrap();
        assert_eq!(upgrade(&plain, &[&prefix]), None);
    }
}
