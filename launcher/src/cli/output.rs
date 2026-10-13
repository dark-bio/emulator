// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! What a command prints, in the two shapes every Dark Bio tool prints in.
//!
//! stdout carries the result and nothing else, as a block, a table or a
//! checklist for reading or as one JSON document. stderr carries everything a
//! person reads along the way: notes, warnings, hints, steps, diagnostic logs
//! and errors, one line each, or one JSON object per line.
//!
//! Every value that came from outside the tool, a device's name, a path, a
//! line of a log, goes through the style module's escaping first, so nothing
//! a device says can steer the terminal or pass for a line of the tool's own.

use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::args::Global;
use super::style::{self, Role, Theme, wrap};
use crate::error::{Code, Error};

/// The two streams of one run, shared by everything that prints.
#[derive(Clone)]
pub(crate) struct Output(Arc<State>);

/// What the streams agreed on, and how much of the screen is already used.
struct State {
    /// Whether stdout is one JSON document and stderr JSON Lines.
    json: bool,

    /// Whether optional events are suppressed.
    quiet: bool,

    /// Whether steps are narrated.
    verbose: bool,

    /// Whether a question fails instead of being asked.
    no_input: bool,

    /// What stdout can carry.
    out: Theme,

    /// What stderr can carry.
    err: Theme,

    /// Whether a result has already been claimed.
    printed: AtomicBool,

    /// Blank lines owed between the two streams.
    spacing: Mutex<Spacing>,
}

/// What each stream last put on screen, which is what decides the blank lines
/// between them. The two are read together as one page.
#[derive(Default)]
struct Spacing {
    /// Whether stderr has printed anything yet.
    err_printed: bool,

    /// Whether a result block is the last thing on stdout.
    out_block: bool,
}

impl Output {
    /// Resolve both streams from the options this run was given.
    pub(crate) fn new(global: &Global) -> Self {
        Self(Arc::new(State {
            json: global.json,
            quiet: global.quiet,
            verbose: global.verbose,
            no_input: global.no_input,
            out: Theme::new(global.json, false),
            err: Theme::new(global.json, true),
            printed: AtomicBool::new(false),
            spacing: Mutex::new(Spacing::default()),
        }))
    }

    /// Whether this run answers in JSON.
    pub(crate) fn json(&self) -> bool {
        self.0.json
    }

    /// Print a command's one result: the named rows as a block of label and
    /// value lines, or the whole document as JSON. The reading view may carry
    /// fewer fields than the document; the document always carries them all.
    pub(crate) fn block(
        &self,
        document: &Value,
        title: Option<&str>,
        rows: &[(&str, &str)],
    ) -> Result<(), Error> {
        self.result(document, |theme| {
            let rows: Vec<(String, String)> = rows
                .iter()
                .map(|(label, key)| {
                    (
                        (*label).to_owned(),
                        value(theme, key, document.get(*key).unwrap_or(&Value::Null)),
                    )
                })
                .collect();
            block(theme, title, &rows)
        })
    }

    /// Print several results as a table, one row each, or the whole document
    /// as JSON. A table too wide for the terminal falls back to one block per
    /// row, and with no rows the `empty` sentence prints in its place.
    pub(crate) fn table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
        empty: &str,
    ) -> Result<(), Error> {
        self.result(document, |theme| table(theme, rows, columns, empty))
    }

    /// Print a doctor's checks as a checklist, one row each with its mark,
    /// detail and hint, or the whole document as JSON.
    pub(crate) fn checklist(&self, document: &Value, rows: &[Value]) -> Result<(), Error> {
        self.result(document, |theme| checklist(theme, rows))
    }

    /// Claim the one result and write it. A second claim is ignored, so an
    /// error after a partial result cannot replace it. A write that fails is
    /// the command's failure, since a result nobody received is no result.
    fn result(&self, document: &Value, render: impl FnOnce(&Theme) -> String) -> Result<(), Error> {
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let text = if self.json() {
            serde_json::to_string_pretty(document).expect("a result serializes")
        } else {
            render(&self.0.out)
        };
        let mut spacing = self.0.spacing.lock().expect("output not poisoned");
        write_result(
            &mut spacing,
            &mut io::stdout().lock(),
            &mut io::stderr().lock(),
            &text,
            self.json(),
        )
        .map_err(|err| Error::io(format!("could not write the result: {err}")))
    }

    /// Whether a result has been claimed, which is what keeps a failure from
    /// replacing one.
    pub(crate) fn printed(&self) -> bool {
        self.0.printed.load(Ordering::SeqCst)
    }

    /// Give stdout up to something else for this run, such as a guest console
    /// or a completion script. Nothing this layer would have written there is
    /// written at all.
    pub(crate) fn release_stdout(&self) {
        self.0.printed.store(true, Ordering::SeqCst);
    }

    /// Write one stderr event, as a reading line or as one JSON object.
    pub(crate) fn event(&self, kind: &str, message: impl AsRef<str>) {
        if self.0.quiet && matches!(kind, "progress" | "note" | "warning" | "step") {
            return;
        }
        if kind == "step" && !self.0.verbose {
            return;
        }
        let message = style::printable(message.as_ref());
        let mut spacing = self.0.spacing.lock().expect("output not poisoned");
        let mut stderr = io::stderr().lock();
        emit(
            &mut spacing,
            &mut stderr,
            self.json(),
            &json!({"event": kind, "message": message}),
            &event(&self.0.err, kind, &message),
        );
    }

    /// Renders a tracing event with its level, target and fields in JSON output.
    pub(crate) fn log(&self, level: &str, target: &str, fields: &serde_json::Map<String, Value>) {
        let message = fields.get("message").and_then(Value::as_str).unwrap_or("");
        if !self.json() {
            self.event("log", format!("[{target}] {message}"));
            return;
        }

        // Escape external strings before they can form terminal control sequences
        let fields: serde_json::Map<String, Value> = fields
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::String(value) => Value::String(style::printable(value)),
                    value => value.clone(),
                };
                (key.clone(), value)
            })
            .collect();
        let mut spacing = self.0.spacing.lock().expect("output not poisoned");
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "{}",
            json!({
                "event": "log", "message": style::printable(message),
                "level": level.to_ascii_lowercase(), "target": target, "fields": fields,
            })
        );
        spacing.err_printed = true;
        let _ = stderr.flush();
    }

    /// Report a failure and the steps out of it. Under JSON the error object
    /// also lands on stdout, as the one member of an `error` envelope, when
    /// there is no result to replace.
    pub(crate) fn error(&self, error: &Error) {
        if self.json() {
            if !self.printed() {
                self.0.printed.store(true, Ordering::SeqCst);
                let mut stdout = io::stdout().lock();
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string_pretty(&json!({"error": error.json()}))
                        .expect("an error serializes")
                );
                let _ = stdout.flush();
            }
            let mut spacing = self.0.spacing.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            let _ = writeln!(
                stderr,
                "{}",
                json!({"event": "error", "error": error.json()})
            );
            spacing.err_printed = true;
            let _ = stderr.flush();
        } else {
            let mut spacing = self.0.spacing.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            separate(&mut spacing, &mut stderr);
            let code = error.code.name();
            let line = event(
                &self.0.err,
                &format!("error[{code}]"),
                &style::printable(&error.message),
            );
            let _ = writeln!(stderr, "{line}");
            spacing.err_printed = true;
            let _ = stderr.flush();
        }
        for hint in &error.hints {
            self.event("hint", hint);
        }
    }

    /// Ask a yes or no question. Only a terminal is ever asked, and anywhere
    /// else the question is the failure that names the flag answering it.
    pub(crate) fn confirm(&self, question: &str, refusal: &str, flag: &str) -> Result<bool, Error> {
        if self.json() || self.0.no_input || !io::stdin().is_terminal() {
            return Err(Error::new(Code::ConfirmationRequired, refusal)
                .hint(format!("pass `{flag}` to confirm without being asked")));
        }
        {
            let mut spacing = self.0.spacing.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            separate(&mut spacing, &mut stderr);
            let theme = &self.0.err;
            let question = style::printable(question);
            let line = if theme.interactive {
                format!(
                    "{} {} {}",
                    theme.paint(Role::Attention, "?"),
                    theme.inline(&question),
                    theme.paint(Role::Muted, "(y/N)")
                )
            } else {
                format!("{question} (y/N)")
            };
            let _ = write!(stderr, "{} ", wrap(&line, theme.width, 2));
            spacing.err_printed = true;
            let _ = stderr.flush();
        }
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|err| Error::io(format!("could not read the answer: {err}")))?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }
}

/// Write a result alone to stdout, with the page's blank lines on stderr.
fn write_result(
    spacing: &mut Spacing,
    stdout: &mut impl io::Write,
    stderr: &mut impl io::Write,
    text: &str,
    json: bool,
) -> io::Result<()> {
    if !json && spacing.err_printed {
        writeln!(stderr)?;
        stderr.flush()?;
    }
    writeln!(stdout, "{text}")?;
    stdout.flush()?;
    spacing.out_block = !json;
    Ok(())
}

/// Write one event, after the blank line a result is owed.
fn emit(
    spacing: &mut Spacing,
    stderr: &mut impl io::Write,
    json: bool,
    document: &Value,
    line: &str,
) {
    if json {
        let _ = writeln!(stderr, "{document}");
    } else {
        separate(spacing, stderr);
        let _ = writeln!(stderr, "{line}");
    }
    spacing.err_printed = true;
    let _ = stderr.flush();
}

/// Put the blank line between a result block and the next stderr line, once.
fn separate(spacing: &mut Spacing, stderr: &mut impl io::Write) {
    if spacing.out_block {
        let _ = writeln!(stderr);
        spacing.out_block = false;
    }
}

/// One stderr event, painted by its kind and wrapped under its own prefix.
fn event(theme: &Theme, kind: &str, message: &str) -> String {
    let role = match kind {
        "error" => Role::Failure,
        kind if kind.starts_with("error[") => Role::Failure,
        "warning" | "approve" => Role::Attention,
        "hint" => Role::Accent,
        _ => Role::Muted,
    };
    if kind == "step" {
        // Return to Muted after each quoted command's Accent
        let message = theme
            .inline(message)
            .replace("\x1b[0m", &format!("\x1b[0m{}", theme.style(Role::Muted)));
        return wrap(
            &format!("  {}", theme.paint(Role::Muted, message)),
            theme.width,
            2,
        );
    }
    let prefix = match kind {
        "error" | "warning" | "hint" | "note" | "log" => theme.paint(role, format!("{kind}:")),
        kind if kind.starts_with("error[") => theme.paint(role, format!("{kind}:")),
        "progress" => theme.glyph("›", ">").to_owned(),
        "approve" => theme.paint(Role::Attention, "!"),
        _ => theme.paint(Role::Muted, theme.glyph("·", "-")),
    };
    wrap(
        &format!("{prefix} {}", theme.inline(message)),
        theme.width,
        console::measure_text_width(&prefix) + 1,
    )
}

/// One field as a person reads it, with its unit, its mark and its color.
fn value(theme: &Theme, key: &str, value: &Value) -> String {
    match value {
        Value::Null => return theme.paint(Role::Muted, "-"),
        Value::Array(values) if values.is_empty() => return theme.paint(Role::Muted, "none"),
        Value::Bool(state) => return if *state { "yes" } else { "no" }.to_owned(),
        _ => {}
    }
    if key.ends_with("_bytes")
        && let Some(count) = value.as_u64()
    {
        return bytes(count);
    }
    if key.ends_with("_seconds") && value.is_number() {
        return format!("{value}s");
    }
    let text = style::cell(&scalar(value));
    match key {
        // The day is what a reader scans for; the document keeps the instant.
        "expires" if text.len() >= 10 => text[..10].to_owned(),
        "locator" => theme.paint(Role::Accent, text),
        "path" => style::cell(&home_path(Path::new(&scalar(value)))),
        _ => text,
    }
}

/// A local path as a person reads it, with the home directory as `~` on
/// macOS and Linux. JSON keeps the full path.
pub(crate) fn home_path(path: &Path) -> String {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(home)
    {
        return if relative.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", relative.display())
        };
    }
    path.display().to_string()
}

/// A scalar as plain text, with the words the house uses for absence and for
/// the two truth values.
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".to_owned(),
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        Value::String(text) => text.clone(),
        Value::Array(values) => values.iter().map(scalar).collect::<Vec<_>>().join(", "),
        value => value.to_string(),
    }
}

/// A byte count in binary units, which is how every size in this tool reads.
pub(crate) fn bytes(count: u64) -> String {
    for (unit, divisor) in [("GiB", 1_u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if count >= divisor {
            return format!("{:.1} {unit}", count as f64 / divisor as f64);
        }
    }
    format!("{count} B")
}

/// Label and value lines under an optional title, aligned on the widest
/// label and indented two spaces. A label wide enough to crowd the values
/// stacks them instead, an empty row separates two groups, and a row without a
/// label prints its value alone.
fn block(theme: &Theme, title: Option<&str>, rows: &[(String, String)]) -> String {
    // The title sits at column 0, above its rows
    let mut lines = Vec::new();
    if let Some(title) = title {
        lines.push(wrap(
            &theme.paint(Role::Heading, style::cell(title)),
            theme.width,
            0,
        ));
    }

    // Align the rows, stacking values when the labels leave too little room
    let labels = rows
        .iter()
        .map(|(label, _)| console::measure_text_width(label))
        .max()
        .unwrap_or(0);
    let stacked = labels + 8 > theme.width;
    lines.extend(rows.iter().map(|(label, value)| {
        if label.is_empty() && value.is_empty() {
            return String::new();
        }
        if label.is_empty() {
            return wrap(&format!("  {value}"), theme.width, 2);
        }
        if stacked {
            return [theme.paint(Role::Muted, label), value.clone()]
                .iter()
                .map(|line| wrap(&format!("  {line}"), theme.width, 2))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let padding = " ".repeat(labels - console::measure_text_width(label));
        let line = format!("  {}{padding}  {value}", theme.paint(Role::Muted, label));
        wrap(&line, theme.width, labels + 4)
    }));
    lines.join("\n")
}

/// One row per result, with muted uppercase headers and numbers aligned on
/// the right. A table that does not fit becomes one block per row, so nothing
/// is lost to the width.
fn table(theme: &Theme, rows: &[Value], columns: &[(&str, &str)], empty: &str) -> String {
    if rows.is_empty() {
        return wrap(&style::cell(empty), theme.width, 0);
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|(_, key)| value(theme, key, row.get(*key).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    let numeric: Vec<bool> = columns
        .iter()
        .map(|(_, key)| {
            key.ends_with("_bytes")
                || key.ends_with("_seconds")
                || rows.iter().any(|row| row[*key].is_number())
        })
        .collect();
    let mut widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(index, (label, _))| {
            cells
                .iter()
                .map(|row| console::measure_text_width(&row[index]))
                .max()
                .unwrap_or(0)
                .max(console::measure_text_width(label))
        })
        .collect();
    let total =
        |widths: &[usize]| widths.iter().sum::<usize>() + columns.len().saturating_sub(1) * 2;
    // A device's name is the one column whose text a reader can lose the tail
    // of and still know which emulator a row is. Everything else is a value
    // somebody may type back.
    let flexible = columns.iter().position(|(_, key)| *key == "name");
    if let Some(index) = flexible {
        widths[index] = widths[index]
            .saturating_sub(total(&widths).saturating_sub(theme.width))
            .max(console::measure_text_width(columns[index].0).max(8));
    }
    if total(&widths) > theme.width {
        return cells
            .iter()
            .map(|row| {
                let rows: Vec<(String, String)> = columns
                    .iter()
                    .zip(row)
                    .map(|((label, _), cell)| ((*label).to_owned(), cell.clone()))
                    .collect();
                block(theme, None, &rows)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let line = |cells: &[String], header: bool| {
        let mut out = String::new();
        for (index, cell) in cells.iter().enumerate() {
            let cell = if Some(index) == flexible {
                theme.truncate(cell, widths[index])
            } else {
                cell.clone()
            };
            let padding = " ".repeat(widths[index] - console::measure_text_width(&cell));
            let last = index + 1 == cells.len();
            let cell = if header {
                theme.paint(Role::Muted, cell)
            } else {
                cell
            };
            if numeric[index] {
                out.push_str(&padding);
                out.push_str(&cell);
            } else {
                out.push_str(&cell);
                if !last {
                    out.push_str(&padding);
                }
            }
            if !last {
                out.push_str("  ");
            }
        }
        out
    };
    let headers: Vec<String> = columns
        .iter()
        .map(|(label, _)| (*label).to_owned())
        .collect();
    let mut lines = vec![line(&headers, true)];
    lines.extend(cells.iter().map(|row| line(row, false)));
    lines.join("\n")
}

/// One line per check, its mark in the result's color, its name muted like a
/// label and its detail plain, wrapped under its own column. A hint follows on
/// a line of its own beneath a warning or a failure.
fn checklist(theme: &Theme, rows: &[Value]) -> String {
    let labels = rows
        .iter()
        .map(|row| console::measure_text_width(&style::cell(row["name"].as_str().unwrap_or("-"))))
        .max()
        .unwrap_or(0);
    // The mark is two cells as a glyph and three as its ASCII twin.
    let names = labels + if theme.unicode { 2 } else { 3 };
    rows.iter()
        .map(|row| {
            let name = row["name"].as_str().unwrap_or("-");
            let result = row["result"].as_str().unwrap_or("-");
            let role = match result {
                "ok" => Role::Success,
                "warn" => Role::Attention,
                "fail" => Role::Failure,
                _ => Role::Muted,
            };
            let detail = style::cell(row["detail"].as_str().unwrap_or("-"));
            let detail = if result == "skip" {
                format!("skipped: {detail}")
            } else {
                detail
            };
            let name = theme.mark(role, &theme.paint(Role::Muted, style::cell(name)));
            let line = format!(
                "  {}{}  {}",
                name,
                " ".repeat(names.saturating_sub(console::measure_text_width(&name))),
                detail
            );
            let mut line = wrap(&line, theme.width, names + 4);
            if let Some(hint) = row["hint"].as_str() {
                line.push('\n');
                line.push_str(&wrap(
                    &format!(
                        "    {} {}",
                        theme.paint(Role::Accent, "hint:"),
                        theme.inline(&style::cell(hint))
                    ),
                    theme.width,
                    10,
                ));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::style::Color;

    /// A diagnostic keeps its label and quoted command, and a step prints
    /// indented with neither label nor mark.
    #[test]
    fn test_an_event_carries_its_prefix_and_paints_a_quoted_command() {
        let theme = Theme::fixed(80, Color::Basic, true);
        assert_eq!(
            event(&theme, "hint", "watch `ark-emulator list`"),
            "\x1b[1mhint:\x1b[0m watch `\x1b[1mark-emulator list\x1b[0m`",
            "quoted hint"
        );
        assert_eq!(
            event(&theme, "step", "reserving a port"),
            "  reserving a port",
            "step without label or mark"
        );
    }

    /// A block aligns its labels and keeps booleans plain beside absent values.
    #[test]
    fn test_a_block_aligns_labels_and_spells_absent_values() {
        let theme = Theme::fixed(80, Color::Off, true);
        let document = json!({"locator": "emulator:18181", "environment": null, "ready": true});
        let rows = [
            (
                "Locator".to_owned(),
                value(&theme, "locator", &document["locator"]),
            ),
            (
                "Env".to_owned(),
                value(&theme, "environment", &document["environment"]),
            ),
            (
                "Ready".to_owned(),
                value(&theme, "ready", &document["ready"]),
            ),
        ];
        assert_eq!(
            block(&theme, None, &rows),
            "  Locator  emulator:18181\n  Env      -\n  Ready    yes",
            "untitled block"
        );
    }

    /// A table starts at column 0, with numbers and their headers aligned right.
    #[test]
    fn test_a_table_aligns_numbers_right_and_text_left() {
        let theme = Theme::fixed(80, Color::Off, true);
        let rows = [
            json!({"port": 18181, "disk": "a.ark"}),
            json!({"port": 181, "disk": "longer.ark"}),
        ];
        let columns = [("PORT", "port"), ("DISK", "disk")];
        assert_eq!(
            table(&theme, &rows, &columns, "No emulators running"),
            " PORT  DISK\n18181  a.ark\n  181  longer.ark",
            "numbers and their headers align right"
        );
    }

    /// A table that cannot fit keeps every value in a block per row.
    #[test]
    fn test_a_table_falls_back_to_blocks_when_it_cannot_fit() {
        let rows = [json!({"port": 18181, "disk": "a-rather-long-name.ark"})];
        let columns = [("PORT", "port"), ("DISK", "disk")];

        let theme = Theme::fixed(24, Color::Off, true);
        let narrow = table(&theme, &rows, &columns, "No emulators running");
        assert!(narrow.starts_with("  PORT  18181"), "{narrow}");
        for line in narrow.lines() {
            assert!(console::measure_text_width(line) <= theme.width, "{line}");
        }
        assert!(
            narrow
                .split_whitespace()
                .collect::<String>()
                .contains("a-rather-long-name.ark"),
            "the image name stays complete"
        );
    }

    /// A checklist without glyphs keeps its ASCII marks, skip details and hints.
    #[test]
    fn test_a_checklist_marks_each_result_and_carries_the_hint() {
        let theme = Theme::fixed(80, Color::Off, false);
        let rows = [
            json!({"name": "qemu", "result": "ok", "detail": "11.1.1 on PATH", "hint": null}),
            json!({"name": "acceleration", "result": "fail", "detail": "software emulation only", "hint": "add your user to the kvm group"}),
            json!({"name": "image", "result": "skip", "detail": "none chosen yet", "hint": null}),
        ];
        assert_eq!(
            checklist(&theme, &rows),
            "  ok qemu          11.1.1 on PATH\n  x acceleration   software emulation only\n    hint: add your user to the kvm group\n  - image          skipped: none chosen yet",
            "ASCII checklist"
        );
    }

    /// A warning takes the attention mark and keeps its upgrade hint.
    #[test]
    fn test_a_checklist_warning_uses_attention_and_carries_its_hint() {
        let rows = [json!({
            "name": "update", "result": "warn",
            "detail": "Ark Emulator 0.2.3 is available, this is 0.2.2",
            "hint": "download it from https://github.com/dark-bio/emulator",
        })];
        assert_eq!(
            checklist(&Theme::fixed(80, Color::Off, false), &rows),
            "  ! update   Ark Emulator 0.2.3 is available, this is 0.2.2\n    hint: download it from https://github.com/dark-bio/emulator",
            "plain warning"
        );
        assert_eq!(
            checklist(&Theme::fixed(80, Color::True, true), &rows),
            "  \x1b[1m\x1b[38;2;232;162;74m!\x1b[0m \x1b[38;2;124;128;152mupdate\x1b[0m  Ark Emulator 0.2.3 is available, this is 0.2.2\n    \x1b[1m\x1b[38;2;137;180;250mhint:\x1b[0m download it from https://github.com/dark-bio/emulator",
            "colored mark, muted name and plain detail"
        );
    }

    /// A detail longer than the line continues under its own column, not
    /// under the mark.
    #[test]
    fn test_a_long_detail_wraps_under_the_detail_column() {
        let theme = Theme::fixed(40, Color::Off, false);
        let rows = [json!({
            "name": "data",
            "result": "ok",
            "detail": "/Users/someone/Library/Application Support/bio.dark.emulator",
            "hint": null,
        })];
        let text = checklist(&theme, &rows);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() > 1, "the long path wraps");
        for line in &lines[1..] {
            assert!(line.starts_with(&" ".repeat(11)), "{line:?}");
        }
    }

    /// A name from the registry is printed as text, however it is spelled.
    #[test]
    fn test_a_value_from_a_device_is_escaped() {
        let theme = Theme::fixed(80, Color::Off, true);
        assert_eq!(
            value(&theme, "name", &json!("ark\x1b[2J\nx")),
            "ark\\u{1b}[2J\\nx",
            "device controls stay escaped"
        );
    }

    /// A table with no rows prints its sentence at column 0.
    #[test]
    fn test_nothing_running_prints_a_sentence() {
        let theme = Theme::fixed(80, Color::Off, true);
        assert_eq!(
            table(&theme, &[], &[("PORT", "port")], "No emulators running"),
            "No emulators running",
            "empty list"
        );
        assert_eq!(
            table(&theme, &[], &[], "No results"),
            "No results",
            "caller supplies the sentence"
        );
    }

    /// Sizes read in binary units.
    #[test]
    fn test_sizes_read_in_binary_units() {
        assert_eq!(bytes(0), "0 B", "zero");
        assert_eq!(bytes(1023), "1023 B", "bytes");
        assert_eq!(bytes(1 << 10), "1.0 KiB", "KiB");
        assert_eq!(bytes(3 << 30), "3.0 GiB", "GiB");
    }

    /// A title sits at column 0 above its indented groups and unlabeled rows.
    #[test]
    fn test_block_titles_and_groups() {
        let rows = [
            ("Serial".into(), "emulator-1".into()),
            (String::new(), String::new()),
            ("Ready".into(), "yes".into()),
            (String::new(), "Another fact".into()),
        ];
        assert_eq!(
            block(
                &Theme::fixed(80, Color::Basic, true),
                Some("Lab Emulator"),
                &rows
            ),
            "\x1b[1mLab Emulator\x1b[0m\n  Serial  emulator-1\n\n  Ready   yes\n  Another fact",
            "title and row groups"
        );
        assert_eq!(
            block(
                &Theme::fixed(12, Color::Off, true),
                Some("Lab Emulator"),
                &rows[..1]
            ),
            "Lab Emulator\n  Serial\n  emulator-1",
            "narrow blocks stack labels and values"
        );
    }

    /// Environments, booleans and paths print plain, and locators in Accent.
    #[test]
    fn test_values_keep_only_locator_emphasis() {
        let theme = Theme::fixed(80, Color::True, true);
        for environment in ["release", "staging", "develop", "unknown"] {
            assert_eq!(
                block(
                    &theme,
                    None,
                    &[(
                        "Environment".into(),
                        value(&theme, "environment", &json!(environment))
                    )]
                ),
                format!("  \x1b[38;2;124;128;152mEnvironment\x1b[0m  {environment}"),
                "{environment}"
            );
        }
        for key in [
            "ready",
            "created",
            "started",
            "pressed",
            "cli_pressed",
            "changed",
            "wiped",
            "locator",
        ] {
            let rows = [json!({key: true}), json!({key: false})];
            assert_eq!(
                table(&theme, &rows, &[("VALUE", key)], "No values"),
                "\x1b[38;2;124;128;152mVALUE\x1b[0m\nyes\nno",
                "{key}"
            );
            assert_eq!(
                block(
                    &theme,
                    None,
                    &[("Value".into(), value(&theme, key, &json!(false)))]
                ),
                "  \x1b[38;2;124;128;152mValue\x1b[0m  no",
                "{key}"
            );
        }
        for key in ["path", "log"] {
            assert_eq!(
                block(
                    &theme,
                    None,
                    &[("File".into(), value(&theme, key, &json!("lab.ark")))]
                ),
                "  \x1b[38;2;124;128;152mFile\x1b[0m  lab.ark",
                "{key}"
            );
        }
        assert_eq!(
            table(
                &theme,
                &[json!({"locator":"emulator:18181"})],
                &[("LOCATOR", "locator")],
                "No emulators running"
            ),
            "\x1b[38;2;124;128;152mLOCATOR\x1b[0m\n\x1b[1m\x1b[38;2;137;180;250memulator:18181\x1b[0m",
            "locator"
        );
    }

    /// Only the name truncates, never below 8 cells or its header.
    #[test]
    fn test_tables_shrink_only_names_before_falling_back() {
        let rows = [json!({"name":"A very long emulator name", "serial":"ark-123"})];
        let columns = [("NAME", "name"), ("SERIAL", "serial")];
        for (unicode, name) in [(true, "A very …"), (false, "A ver...")] {
            let theme = Theme::fixed(17, Color::Off, unicode);
            assert_eq!(
                table(&theme, &rows, &columns, "No emulators running"),
                format!("NAME      SERIAL\n{name}  ark-123"),
                "unicode={unicode}"
            );
        }
        for (width, header) in [(16, "NAME"), (17, "LONG NAME")] {
            let theme = Theme::fixed(width, Color::Off, true);
            let rendered = table(
                &theme,
                &rows,
                &[(header, "name"), ("SERIAL", "serial")],
                "No emulators running",
            );
            assert!(
                rendered.starts_with("  "),
                "width={width}, header={header}: {rendered}"
            );
            assert!(
                !rendered.contains('…'),
                "width={width}, header={header}: {rendered}"
            );
            assert!(
                rendered
                    .lines()
                    .all(|line| console::measure_text_width(line) <= width),
                "width={width}, header={header}: {rendered}"
            );
            assert!(
                rendered
                    .split_whitespace()
                    .collect::<String>()
                    .contains("Averylongemulatorname"),
                "width={width}, header={header}: {rendered}"
            );
        }
    }

    /// A diagnostic's label takes its role, and a long message hangs under
    /// itself.
    #[test]
    fn test_events_style_labels_and_wrap_messages() {
        let theme = Theme::fixed(80, Color::True, true);
        for (kind, prefix) in [
            (
                "error[usage]",
                "\x1b[1m\x1b[38;2;235;96;112merror[usage]:\x1b[0m",
            ),
            ("warning", "\x1b[1m\x1b[38;2;232;162;74mwarning:\x1b[0m"),
            ("hint", "\x1b[1m\x1b[38;2;137;180;250mhint:\x1b[0m"),
            ("note", "\x1b[38;2;124;128;152mnote:\x1b[0m"),
            ("log", "\x1b[38;2;124;128;152mlog:\x1b[0m"),
        ] {
            assert_eq!(
                event(&theme, kind, "run `ark-emulator list`"),
                format!("{prefix} run `\x1b[1m\x1b[38;2;137;180;250mark-emulator list\x1b[0m`"),
                "{kind}"
            );
        }
        assert_eq!(
            event(&theme, "step", "run `ark-emulator list` next"),
            "  \x1b[38;2;124;128;152mrun `\x1b[1m\x1b[38;2;137;180;250mark-emulator list\x1b[0m\x1b[38;2;124;128;152m` next\x1b[0m",
            "step resumes muted text"
        );

        let narrow = Theme::fixed(32, Color::Off, true);
        assert_eq!(
            event(
                &narrow,
                "warning",
                "the emulator needs a newer build to boot"
            ),
            "warning: the emulator needs a\n         newer build to boot",
            "warning indent"
        );
        assert_eq!(
            event(
                &narrow,
                "error[usage]",
                "the emulator needs a newer build to boot"
            ),
            "error[usage]: the emulator needs\n              a newer build to\n              boot",
            "error indent"
        );
        assert_eq!(
            event(&narrow, "step", "the emulator needs a newer build to boot"),
            "  the emulator needs a newer\n  build to boot",
            "step indent"
        );
    }

    /// Checklist details and hints wrap under their columns and escape controls.
    #[test]
    fn test_checklist_colors_and_wrapping() {
        let theme = Theme::fixed(80, Color::True, true);
        for (result, mark, detail) in [
            ("ok", "\x1b[1m\x1b[38;2;148;202;110m✓\x1b[0m", "detail"),
            ("warn", "\x1b[1m\x1b[38;2;232;162;74m!\x1b[0m", "detail"),
            ("fail", "\x1b[1m\x1b[38;2;235;96;112m✗\x1b[0m", "detail"),
            ("skip", "\x1b[38;2;124;128;152m·\x1b[0m", "skipped: detail"),
        ] {
            let rows = [json!({"name":"image", "result":result, "detail":"detail"})];
            assert_eq!(
                checklist(&theme, &rows),
                format!("  {mark} \x1b[38;2;124;128;152mimage\x1b[0m  {detail}"),
                "{result}"
            );
        }
        let rows = [
            json!({"name":"data", "result":"fail", "detail":"a path with a long explanation for the reader", "hint":"run `ark-emulator doctor` for details"}),
        ];
        assert_eq!(
            checklist(&Theme::fixed(40, Color::Off, true), &rows),
            "  ✗ data  a path with a long explanation\n          for the reader\n    hint: run `ark-emulator doctor` for\n          details",
            "details and hints hang under their text"
        );
        let rows = [
            json!({"name":"bad\nname", "result":"ok", "detail":"bad\nvalue", "hint":"bad\u{001b}[2Jhint"}),
        ];
        assert_eq!(
            checklist(&Theme::fixed(80, Color::Off, true), &rows),
            "  ✓ bad\\nname  bad\\nvalue\n    hint: bad\\u{1b}[2Jhint",
            "untrusted text cannot add rows or terminal controls"
        );
    }

    /// The blank lines around a reading result go to stderr, in a pipe too.
    #[test]
    fn test_composition_keeps_stdout_clean_and_nothing_trailing() {
        for interactive in [false, true] {
            let theme = Theme {
                interactive,
                ..Theme::fixed(80, Color::Off, true)
            };
            for result in [
                block(&theme, None, &[("Ready".into(), "yes".into())]),
                table(&theme, &[], &[("NAME", "name")], "No emulators running"),
                checklist(
                    &theme,
                    &[json!({"name":"qemu", "result":"ok", "detail":"on PATH"})],
                ),
            ] {
                for (before, after) in [(false, false), (true, false), (false, true), (true, true)]
                {
                    let mut spacing = Spacing::default();
                    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                    if before {
                        emit(
                            &mut spacing,
                            &mut stderr,
                            false,
                            &Value::Null,
                            &event(&theme, "note", "before"),
                        );
                    }
                    write_result(&mut spacing, &mut stdout, &mut stderr, &result, false).unwrap();
                    if after {
                        emit(
                            &mut spacing,
                            &mut stderr,
                            false,
                            &Value::Null,
                            &event(&theme, "error[io]", "after"),
                        );
                        emit(
                            &mut spacing,
                            &mut stderr,
                            false,
                            &Value::Null,
                            &event(&theme, "hint", "last"),
                        );
                    }
                    assert_eq!(
                        String::from_utf8(stdout).unwrap(),
                        format!("{result}\n"),
                        "interactive={interactive}, before={before}, after={after}"
                    );
                    assert_eq!(
                        String::from_utf8(stderr).unwrap(),
                        format!(
                            "{}{}",
                            if before { "note: before\n\n" } else { "" },
                            if after {
                                "\nerror[io]: after\nhint: last\n"
                            } else {
                                ""
                            }
                        ),
                        "interactive={interactive}, before={before}, after={after}"
                    );
                }
            }
        }
    }

    /// JSON results and events carry whole values and no blank lines.
    #[test]
    fn test_json_composition_keeps_one_document_and_line_events() {
        let mut spacing = Spacing::default();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        emit(
            &mut spacing,
            &mut stderr,
            true,
            &json!({"event":"step", "message":"run `ark-emulator list`"}),
            "unused",
        );
        write_result(
            &mut spacing,
            &mut stdout,
            &mut stderr,
            "{\n  \"emulators\": []\n}",
            true,
        )
        .unwrap();
        emit(
            &mut spacing,
            &mut stderr,
            true,
            &json!({"event":"hint", "message":"after"}),
            "unused",
        );
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "{\n  \"emulators\": []\n}\n",
            "empty list document"
        );
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "{\"event\":\"step\",\"message\":\"run `ark-emulator list`\"}\n{\"event\":\"hint\",\"message\":\"after\"}\n",
            "JSON lines carry label-free messages"
        );
    }

    /// A wipe's path shows the home directory as `~`, and JSON keeps it whole.
    #[test]
    fn test_home_paths_in_reading_results() {
        let home = dirs::home_dir().expect("a home directory");
        let path = home.join("arks/lab.ark");
        let document = json!({"path":path.display().to_string()});
        let expected = if cfg!(any(target_os = "macos", target_os = "linux")) {
            "~/arks/lab.ark".to_owned()
        } else {
            path.display().to_string()
        };
        let theme = Theme::fixed(200, Color::Off, true);
        assert_eq!(
            block(
                &theme,
                None,
                &[("Path".into(), value(&theme, "path", &document["path"]))]
            ),
            format!("  Path  {expected}"),
            "reading path"
        );
        assert_eq!(document["path"], path.display().to_string(), "JSON path");
        for path in [
            Path::new("arks/lab.ark").to_path_buf(),
            home.with_file_name("another-home").join("lab.ark"),
            Path::new(&format!("{}-other/lab.ark", home.display())).to_path_buf(),
        ] {
            assert_eq!(
                home_path(&path),
                path.display().to_string(),
                "outside or relative: {}",
                path.display()
            );
        }
    }
}
