// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Keeps tracing events and launcher facts ready for a crash report.
//!
//! Enabled launcher events enter a bounded ring buffer and, once opened, the log file.
//! The output layer renders enabled diagnostics on stderr, including QEMU's
//! captured stderr. Windowed launches retain the same events for error reports.
//!
//! Alongside the log sits a small ordered set of facts about this run (guest
//! architecture, which QEMU was picked, the paths in play). They are recorded
//! as startup progresses rather than gathered at the end, so a failure halfway
//! through still reports what was known by then.
//!
//! [`report`] joins the two with an error chain into the text the user sees in
//! the error window and can copy to us. Nothing is ever transmitted from here.
//!
//! Every launcher also keeps its lines in a file under the data directory,
//! named by the port it holds, so that a second process can read what a
//! running emulator has been up to. The port is the only identity a reader has
//! before the registry answers. The file holds the launcher's own lines and
//! QEMU's complaints; the guest never writes to it.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

use crate::cli::args::Log;
use crate::cli::output::Output;

/// Product name, matching `productName` in `tauri.conf.json`.
const PRODUCT: &str = "Ark Emulator";

/// How many recent log lines a report carries. Enough to cover a whole startup
/// including QEMU's own complaints, short enough to paste into an email.
const LOG_CAPACITY: usize = 200;

/// Name of the directory the log files live in, under the data directory.
const LOGS: &str = "logs";

/// Log ring, log file and recorded facts, behind one lock because every writer
/// touches them from a different thread (startup, the QEMU stderr reader, the
/// wait thread) and none of it is hot.
static STATE: Mutex<State> = Mutex::new(State {
    log: VecDeque::new(),
    facts: Vec::new(),
    file: None,
    output: None,
});

/// The ring, the file, the facts and where lines go besides them.
struct State {
    /// The most recent lines, oldest first.
    log: VecDeque<String>,
    /// What this run has found out about itself, in the order it did.
    facts: Vec<(&'static str, String)>,
    /// This launcher's own log file, once it has one.
    file: Option<File>,
    /// Optional terminal output, independent of crash-log retention.
    output: Option<Output>,
}

/// Installs launcher diagnostics before any runtime threads start.
///
/// # Panics
/// Panics if a global tracing subscriber has already been installed.
pub(crate) fn init(output: &Output, level: Option<Log>, command: bool) {
    // Commands keep diagnostics off stderr unless the caller requests them
    if let Ok(mut state) = STATE.lock() {
        state.output = (!command || level.is_some()).then(|| output.clone());
    }
    let level = if level == Some(Log::Trace) {
        LevelFilter::TRACE
    } else {
        LevelFilter::DEBUG
    };

    // Dependency events can contain data outside the launcher's diagnostic contract
    tracing_subscriber::registry()
        .with(Targets::new().with_target("ark_emulator", level))
        .with(Diagnostics)
        .init();
}

/// The directory every launcher writes its log file into.
pub(crate) fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(LOGS)
}

/// Where the launcher holding `port` writes its log.
pub(crate) fn log_path(data_dir: &Path, port: u16) -> PathBuf {
    logs_dir(data_dir).join(format!("{port}.log"))
}

/// Start writing this launcher's lines to its own log file as well, replacing
/// what an earlier launcher on the same port left there.
pub(crate) fn log_to(data_dir: &Path, port: u16) -> Result<()> {
    let dir = logs_dir(data_dir);
    fs::create_dir_all(&dir)
        .with_context(|| format!("could not create the log directory {}", dir.display()))?;
    let path = log_path(data_dir, port);
    let file =
        File::create(&path).with_context(|| format!("could not write {}", path.display()))?;
    if let Ok(mut state) = STATE.lock() {
        state.file = Some(file);
    }
    Ok(())
}

/// Subscriber layer retaining events and forwarding their metadata to the output layer.
struct Diagnostics;

/// One stored diagnostic, retaining its metadata when a parent relays the log.
#[derive(Deserialize, Serialize)]
pub(crate) struct LogRecord {
    /// Tracing severity in lowercase.
    pub(crate) level: String,
    /// Module or named source that emitted the event.
    pub(crate) target: String,
    /// Event fields, including its formatted message.
    pub(crate) fields: Map<String, Value>,
}

impl<S: Subscriber> Layer<S> for Diagnostics {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let metadata = event.metadata();
        let message = fields
            .0
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        let line = format!("[{}] {}", metadata.target(), message)
            .replace('\r', "\\r")
            .replace('\n', "\\n");
        let record = LogRecord {
            level: metadata.level().as_str().to_ascii_lowercase(),
            target: metadata.target().to_owned(),
            fields: fields.0,
        };

        // A failed file write leaves the event available in the crash report
        let output = {
            let Ok(mut state) = STATE.lock() else {
                return;
            };
            if let Some(file) = state.file.as_mut() {
                let _ = writeln!(file, "{}", serde_json::to_string(&record).unwrap());
            }
            if state.log.len() == LOG_CAPACITY {
                state.log.pop_front();
            }
            state.log.push_back(line);
            state.output.clone()
        };

        // Terminal I/O happens after releasing the crash-log lock
        if let Some(output) = output {
            output.log(&record.level, &record.target, &record.fields);
        }
    }
}

/// Fields captured from positional tracing events.
#[derive(Default)]
struct Fields(
    /// JSON fields retained alongside the event's level and target.
    Map<String, Value>,
);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_owned(), Value::String(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().to_owned(), Value::String(value.to_owned()));
    }
}

/// Note a fact about this run for the report. Recording the same key twice
/// overwrites it in place, so a value that gets refined later does not appear
/// under two different answers.
pub(crate) fn record(key: &'static str, value: impl Into<String>) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    let value = value.into();
    match state.facts.iter_mut().find(|(k, _)| *k == key) {
        Some((_, existing)) => *existing = value,
        None => state.facts.push((key, value)),
    }
}

/// Note a path-valued fact. Lossy because a report is text and an unprintable
/// path is still worth seeing.
pub(crate) fn record_path(key: &'static str, path: &Path) {
    record(key, path.display().to_string());
}

/// The full copyable report: what failed, why, what this build and host are,
/// and the recent log. `title` says which kind of failure this was, since
/// dying during startup and dying an hour in read very differently.
///
/// The paths included here contain the user's home directory. That is inherent
/// to a report they choose to send us, and it is the fact most likely to
/// explain the failure, so it stays. Nothing beyond the fields recorded through
/// [`record`] is collected.
pub(crate) fn report(title: &str, err: &anyhow::Error) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{PRODUCT} {}: {title}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out);
    let _ = writeln!(out, "Error: {err}");

    let mut causes = err.chain().skip(1).peekable();
    if causes.peek().is_some() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Caused by:");
        for (i, cause) in causes.enumerate() {
            let _ = writeln!(out, "  {i}: {cause}");
        }
    }

    let state = STATE.lock();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Host: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if let Ok(state) = state.as_deref() {
        for (key, value) in &state.facts {
            let _ = writeln!(out, "{key}: {value}");
        }
        if !state.log.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "Recent log:");
            for line in &state.log {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
    out
}
