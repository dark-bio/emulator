//! What the launcher knows about itself, kept ready for a crash report.
//!
//! A packaged build has nowhere to print: Windows release builds link as GUI
//! apps and a macOS `.app` or Linux AppImage started from a file manager has no
//! visible stderr. So everything that would have been a diagnostic `eprintln!`
//! goes through [`log!`] instead, which still writes to stderr and also keeps
//! the line in a bounded ring buffer. QEMU's own stderr is teed in here too.
//!
//! Alongside the log sits a small ordered set of facts about this run (guest
//! architecture, which QEMU was picked, the paths in play). They are recorded
//! as startup progresses rather than gathered at the end, so a failure halfway
//! through still reports what was known by then.
//!
//! [`report`] joins the two with an error chain into the text the user sees in
//! the error window and can copy to us. Nothing is ever transmitted from here.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

/// Product name, matching `productName` in `tauri.conf.json`.
const PRODUCT: &str = "Ark Emulator";

/// How many recent log lines a report carries. Enough to cover a whole startup
/// including QEMU's own complaints, short enough to paste into an email.
const LOG_CAPACITY: usize = 200;

/// Log ring and recorded facts, behind one lock because every writer touches
/// them from a different thread (startup, the QEMU stderr reader, the wait
/// thread) and none of it is hot.
static STATE: Mutex<State> = Mutex::new(State {
    log: VecDeque::new(),
    facts: Vec::new(),
});

struct State {
    log: VecDeque<String>,
    facts: Vec<(&'static str, String)>,
}

/// Print a line to stderr and keep it for the crash report. Takes the same
/// arguments as [`eprintln!`], which it replaces throughout the launcher.
macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        eprintln!("{line}");
        $crate::diagnostics::push(line);
    }};
}
pub(crate) use log;

/// Add an already-formatted line to the ring, dropping the oldest once full.
/// Called by [`log!`]; use that instead.
pub(crate) fn push(line: String) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
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
