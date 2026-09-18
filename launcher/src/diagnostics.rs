// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! What the launcher knows about itself, kept ready for a crash report.
//!
//! A packaged build has nowhere to print: Windows release builds link as GUI
//! apps and a macOS `.app` or Linux AppImage started from a file manager has no
//! visible stderr. So every diagnostic goes through [`log!`] instead, which
//! keeps the line in a bounded ring buffer and hands it to whichever [`Sink`]
//! this run chose. QEMU's own stderr is teed in here too.
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
    sink: Sink::Stderr,
});

struct State {
    log: VecDeque<String>,
    facts: Vec<(&'static str, String)>,
    file: Option<File>,
    sink: Sink,
}

/// Where a log line goes besides the ring and the log file.
pub(crate) enum Sink {
    /// Straight to stderr, which is what a run with a window does.
    Stderr,

    /// Through a command's output layer, as `log` events.
    Events(crate::output::Output),

    /// Nowhere, which is a command that was not asked for diagnostics.
    Quiet,
}

/// Choose where this run echoes its log lines. The ring and the log file get
/// them whatever is chosen, so a crash report is never short of them.
pub(crate) fn log_sink(sink: Sink) {
    if let Ok(mut state) = STATE.lock() {
        state.sink = sink;
    }
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

/// Record one diagnostic line and hand it to this run's sink. Takes the same
/// arguments as [`eprintln!`], which it replaces throughout the launcher.
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::diagnostics::push(format!($($arg)*));
    }};
}
pub(crate) use log;

/// Add an already-formatted line to the ring, the log file and the sink,
/// dropping the oldest ring entry once full. Called by [`log!`]; use that
/// instead.
pub(crate) fn push(line: String) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    if let Some(file) = state.file.as_mut() {
        // A log file that cannot be written is not a reason to stop logging,
        // and the line is still in the ring.
        let _ = writeln!(file, "{line}");
    }
    match &state.sink {
        Sink::Stderr => eprintln!("{line}"),
        Sink::Events(output) => output.event("log", &line),
        Sink::Quiet => {}
    }
    if state.log.len() == LOG_CAPACITY {
        state.log.pop_front();
    }
    state.log.push_back(line);
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
