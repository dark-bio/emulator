//! What the launcher remembers between runs.
//!
//! A single TOML file in the app's data directory, written on first run and
//! rewritten whenever a value changes. Nothing ships with the app, so a
//! portable copy carried to a new machine starts from the same blank slate an
//! installer would. It holds the handful of choices that would otherwise have
//! to be retyped as flags on every launch: which disk image to boot, how much
//! RAM to give the guest, and which cloud environment a newly created disk
//! gets bound to.
//!
//! Every value is optional, and absent means "no preference" rather than a
//! default written out eagerly. A flag still wins over the file for one run
//! without rewriting it, so the two are readable independently: the file says
//! what was chosen, the command line says what this one launch is doing
//! differently.
//!
//! TOML rather than JSON because this is a file a developer is expected to
//! open and edit by hand, and it can carry comments.
//!
//! Versioned from the start, and strict about it in both directions. A file
//! this build cannot make sense of is an error naming its path rather than
//! defaults quietly written over what someone typed: a settings file only ever
//! becomes unreadable by being hand-edited or by a newer build writing it, and
//! both of those deserve to be pointed at rather than discarded.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// Schema version this build writes and understands.
const VERSION: u32 = 1;

/// Name of the settings file within the data directory.
const FILE: &str = "settings.toml";

/// Guest RAM in MiB when neither a flag nor the settings file names one.
pub(crate) const DEFAULT_MEMORY: u32 = 8192;

/// Floor on guest RAM. Below this the guest cannot get far enough to say what
/// went wrong, so a typo in the settings panel would look like a hang.
pub(crate) const MIN_MEMORY: u32 = 512;

/// Cloud environment a newly created disk is bound to when neither a flag nor
/// the settings file names one.
pub(crate) const DEFAULT_ENV: &str = "release";

/// The environments a disk can be bound to. Shared with the `--env` parser and
/// with the settings panel, so all three agree on the list.
pub(crate) const ENVS: [&str; 3] = ["develop", "staging", "release"];

/// The file's on-disk shape. Separate from [`Settings`] so that the path a
/// value came from never becomes a value itself.
#[derive(Deserialize, Serialize)]
struct Stored {
    /// Schema version of this file, checked against [`VERSION`] on load.
    version: u32,

    /// Disk image to boot when no `--disk` is given. Absent until something
    /// has been chosen, which is how a first run knows to ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk: Option<PathBuf>,

    /// Guest RAM in MiB to use when no `--memory` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<u32>,

    /// Environment to bind a newly created disk to when no `--env` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    env: Option<String>,
}

/// The launcher's persisted preferences, together with where they live.
pub(crate) struct Settings {
    path: PathBuf,
    stored: Stored,
}

impl Settings {
    /// Read the settings out of `dir`, creating them at the current version if
    /// there are none yet. `dir` is expected to exist already.
    pub(crate) fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE);
        let body = match fs::read_to_string(&path) {
            Ok(body) => body,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let settings = Self {
                    path,
                    stored: Stored {
                        version: VERSION,
                        disk: None,
                        memory: None,
                        env: None,
                    },
                };
                settings.save()?;
                return Ok(settings);
            }
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };

        let stored: Stored =
            toml::from_str(&body).with_context(|| format!("could not parse {}", path.display()))?;
        if stored.version > VERSION {
            bail!(
                "{} was written by a newer version of the emulator (settings version {}, \
                 this build understands {VERSION}); update the emulator, or move that file \
                 aside to start over",
                path.display(),
                stored.version
            );
        }

        let settings = Self { path, stored };
        if settings.stored.version < VERSION {
            // Nothing to migrate yet: version 1 is the first shape there has
            // ever been. Older files are simply rewritten at the current
            // version, which is where a real migration would go.
            settings.save()?;
        }
        Ok(settings)
    }

    /// Where these settings are stored, for diagnostics and error messages.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The remembered disk image, if one has ever been chosen. The file it
    /// names is not guaranteed to still exist.
    pub(crate) fn disk(&self) -> Option<&Path> {
        self.stored.disk.as_deref()
    }

    /// The remembered guest RAM in MiB, if one has ever been chosen.
    pub(crate) fn memory(&self) -> Option<u32> {
        self.stored.memory
    }

    /// The remembered environment for newly created disks, if one has ever
    /// been chosen.
    pub(crate) fn env(&self) -> Option<&str> {
        self.stored.env.as_deref()
    }

    /// Write all three values out at once, so the panel's save is one file
    /// write rather than three.
    ///
    /// `None` for `disk` forgets the remembered image, so that the next launch
    /// asks again rather than booting straight through.
    pub(crate) fn apply(&mut self, disk: Option<&Path>, memory: u32, env: &str) -> Result<()> {
        self.stored.disk = disk.map(Path::to_path_buf);
        self.stored.memory = Some(memory);
        self.stored.env = Some(env.to_owned());
        self.save()
    }

    /// Write the file out, through a temporary that is renamed over the
    /// target. A half-written file would be a parse error on the next launch,
    /// and parse errors here are fatal by design.
    ///
    /// The temporary is named after this process. Several emulators share one
    /// data directory, and a common name would have two saving at once rename
    /// each other's half-written file into place.
    fn save(&self) -> Result<()> {
        // Serializing a PathBuf fails on a path that is not valid UTF-8, which
        // is reachable on Linux. Better to say so than to store a lossy string
        // that would then name a different file.
        let body = toml::to_string(&self.stored)
            .with_context(|| format!("could not serialize {}", self.path.display()))?;
        let tmp = self
            .path
            .with_extension(format!("toml.{}.tmp", std::process::id()));
        fs::write(&tmp, body).with_context(|| format!("could not write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_creates_file_when_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let settings = Settings::load(dir).unwrap();
        assert!(settings.disk().is_none());
        assert!(settings.memory().is_none());
        assert!(settings.env().is_none());

        let body = fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(body.contains("version = 1"));
        assert!(!body.contains("disk"));
        assert!(!body.contains("memory"));
        assert!(!body.contains("env"));
    }

    #[test]
    fn test_values_survive_a_reload() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        Settings::load(dir)
            .unwrap()
            .apply(Some(Path::new("/tmp/ark.img")), 2048, "develop")
            .unwrap();

        let settings = Settings::load(dir).unwrap();
        assert_eq!(settings.disk(), Some(Path::new("/tmp/ark.img")));
        assert_eq!(settings.memory(), Some(2048));
        assert_eq!(settings.env(), Some("develop"));
    }

    #[test]
    fn test_a_forgotten_disk_leaves_the_rest() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut settings = Settings::load(dir).unwrap();
        settings
            .apply(Some(Path::new("/tmp/ark.img")), 2048, "develop")
            .unwrap();
        settings.apply(None, 2048, "develop").unwrap();

        let settings = Settings::load(dir).unwrap();
        assert!(settings.disk().is_none());
        assert_eq!(settings.memory(), Some(2048));
        assert_eq!(settings.env(), Some("develop"));
    }

    #[test]
    fn test_reads_a_file_that_only_names_a_disk() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join(FILE), "version = 1\ndisk = \"/tmp/ark.img\"\n").unwrap();

        let settings = Settings::load(dir).unwrap();
        assert_eq!(settings.disk(), Some(Path::new("/tmp/ark.img")));
        assert!(settings.memory().is_none());
        assert!(settings.env().is_none());
    }

    #[test]
    fn test_rejects_a_newer_version() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join(FILE), "version = 999\n").unwrap();

        let Err(err) = Settings::load(dir) else {
            panic!("a future version was accepted");
        };
        let err = err.to_string();
        assert!(err.contains("newer version"), "{err}");
        // The file a newer build wrote is left exactly as it was.
        assert_eq!(
            fs::read_to_string(dir.join(FILE)).unwrap(),
            "version = 999\n"
        );
    }

    #[test]
    fn test_rejects_a_malformed_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join(FILE), "not toml at all").unwrap();

        let Err(err) = Settings::load(dir) else {
            panic!("a malformed file was accepted");
        };
        let err = err.to_string();
        assert!(err.contains("could not parse"), "{err}");
    }
}
