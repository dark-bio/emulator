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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::args::Global;
use crate::error::{Code, Error};
use crate::style::{self, Role, Theme, wrap};

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
    pub(crate) fn block(&self, document: &Value, rows: &[(&str, &str)]) -> Result<(), Error> {
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
            block(theme, &rows)
        })
    }

    /// Print several results as a table, one row each, or the whole document
    /// as JSON. A table too wide for the terminal falls back to one block per
    /// row, and no rows at all print as `none`.
    pub(crate) fn table(
        &self,
        document: &Value,
        rows: &[Value],
        columns: &[(&str, &str)],
    ) -> Result<(), Error> {
        self.result(document, |theme| table(theme, rows, columns))
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
        let mut stdout = io::stdout().lock();
        let written = (|| {
            if self.0.out.interactive && self.0.err.interactive && spacing.err_printed {
                writeln!(stdout)?;
            }
            writeln!(stdout, "{text}")?;
            stdout.flush()
        })();
        spacing.out_block = self.0.out.interactive;
        written.map_err(|err| Error::io(format!("could not write the result: {err}")))
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

    /// Write one stderr event, as `kind: message` or as one JSON object.
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
        if self.json() {
            let _ = writeln!(stderr, "{}", json!({"event": kind, "message": message}));
        } else {
            separate(&mut spacing, &mut stderr);
            let _ = writeln!(stderr, "{}", event(&self.0.err, kind, &message));
        }
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
            let theme = &self.0.err;
            let code = error.code.name();
            let line = format!(
                "{} {}",
                theme.paint(Role::Failure, format!("error[{code}]:")),
                theme.inline(&style::printable(&error.message))
            );
            let _ = writeln!(stderr, "{}", wrap(&line, theme.width, code.len() + 9));
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
        "warning" | "approve" => Role::Attention,
        "hint" => Role::Accent,
        _ => Role::Muted,
    };
    let prefix = theme.paint(role, format!("{kind}:"));
    let message = if kind == "step" {
        theme.paint(
            Role::Muted,
            format!("{} {message}", theme.glyph("\u{203a}", ">")),
        )
    } else {
        theme.inline(message)
    };
    wrap(&format!("{prefix} {message}"), theme.width, kind.len() + 2)
}

/// One field as a person reads it, with its unit, its mark and its color.
fn value(theme: &Theme, key: &str, value: &Value) -> String {
    match value {
        Value::Null => return theme.paint(Role::Muted, "-"),
        Value::Array(values) if values.is_empty() => return theme.paint(Role::Muted, "none"),
        // A device that is not ready is one to wait for, so its mark is the
        // attention mark the house tools put on a locked or unpaired device.
        Value::Bool(state) if key == "ready" => {
            let role = if *state {
                Role::Success
            } else {
                Role::Attention
            };
            return theme.mark(role, if *state { "yes" } else { "no" });
        }
        _ => {}
    }
    if key.ends_with("_bytes")
        && let Some(count) = value.as_u64()
    {
        return bytes(count);
    }
    let text = style::cell(&scalar(value));
    match key {
        // The day is what a reader scans for; the document keeps the instant.
        "expires" if text.len() >= 10 => text[..10].to_owned(),
        "environment" => theme.paint(
            match text.as_str() {
                "release" => Role::Accent,
                "staging" => Role::Staging,
                "develop" => Role::Develop,
                _ => Role::Default,
            },
            text,
        ),
        "locator" | "log" | "path" => theme.paint(Role::Accent, text),
        _ => text,
    }
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

/// Label and value lines, aligned on the widest label and indented two
/// spaces. A label wide enough to crowd the values stacks them instead.
fn block(theme: &Theme, rows: &[(String, String)]) -> String {
    let labels = rows
        .iter()
        .map(|(label, _)| console::measure_text_width(label))
        .max()
        .unwrap_or(0);
    let stacked = labels + 8 > theme.width;
    rows.iter()
        .map(|(label, value)| {
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
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One row per result, with muted uppercase headers and numbers aligned on
/// the right. A table that does not fit becomes one block per row, so nothing
/// is lost to the width.
fn table(theme: &Theme, rows: &[Value], columns: &[(&str, &str)]) -> String {
    if rows.is_empty() {
        return format!("  {}", theme.paint(Role::Muted, "none"));
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
            rows.iter().all(|row| {
                row.get(*key)
                    .is_none_or(|value| value.is_number() || value.is_null())
            })
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
                .max(label.len())
        })
        .collect();
    let total = |widths: &[usize]| 2 + widths.iter().sum::<usize>() + (columns.len() - 1) * 2;
    // A device's name is the one column whose text a reader can lose the tail
    // of and still know which emulator a row is. Everything else is a value
    // somebody may type back.
    if let Some(index) = columns.iter().position(|(_, key)| *key == "name") {
        widths[index] = widths[index]
            .saturating_sub(total(&widths).saturating_sub(theme.width))
            .max(columns[index].0.len());
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
                block(theme, &rows)
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    let line = |cells: &[String], header: bool| {
        let mut out = String::from("  ");
        for (index, cell) in cells.iter().enumerate() {
            let cell = theme.truncate(cell, widths[index]);
            let padding = " ".repeat(widths[index] - console::measure_text_width(&cell));
            let last = index + 1 == cells.len();
            let cell = if header {
                theme.paint(Role::Muted, cell)
            } else {
                cell
            };
            if numeric[index] && !header {
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

/// One line per check, the name marked by its result, the detail muted and
/// wrapped under its own column, and the hint on a line of its own beneath a
/// failure.
fn checklist(theme: &Theme, rows: &[Value]) -> String {
    let labels = rows
        .iter()
        .map(|row| console::measure_text_width(row["name"].as_str().unwrap_or("")))
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
                "fail" => Role::Failure,
                _ => Role::Muted,
            };
            let detail = style::cell(row["detail"].as_str().unwrap_or("-"));
            let detail = if result == "skip" {
                format!("skipped: {detail}")
            } else {
                detail
            };
            let name = theme.mark(role, name);
            let line = format!(
                "  {}{}  {}",
                name,
                " ".repeat(names.saturating_sub(console::measure_text_width(&name))),
                theme.paint(Role::Muted, &detail)
            );
            let mut line = wrap(&line, theme.width, names + 4);
            if let Some(hint) = row["hint"].as_str() {
                line.push('\n');
                line.push_str(&wrap(
                    &format!(
                        "    {} {}",
                        theme.paint(Role::Accent, "hint:"),
                        theme.inline(&style::printable(hint))
                    ),
                    theme.width,
                    6,
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
    use crate::style::Color;

    #[test]
    fn test_an_event_carries_its_prefix_and_paints_a_quoted_command() {
        let theme = Theme::fixed(80, Color::Basic, true);
        assert_eq!(
            event(&theme, "hint", "watch `ark-emulator list`"),
            "\x1b[1mhint:\x1b[0m watch \x1b[1mark-emulator list\x1b[0m"
        );
        assert_eq!(
            event(&theme, "step", "reserving a port"),
            "step: \u{203a} reserving a port"
        );
    }

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
            block(&theme, &rows),
            "  Locator  emulator:18181\n  Env      -\n  Ready    \u{2713} yes"
        );
    }

    #[test]
    fn test_a_table_aligns_numbers_right_and_text_left() {
        let theme = Theme::fixed(80, Color::Off, true);
        let rows = [
            json!({"port": 18181, "disk": "a.ark"}),
            json!({"port": 181, "disk": "longer.ark"}),
        ];
        let columns = [("PORT", "port"), ("DISK", "disk")];
        assert_eq!(
            table(&theme, &rows, &columns),
            "  PORT   DISK\n  18181  a.ark\n    181  longer.ark"
        );
    }

    #[test]
    fn test_a_table_falls_back_to_blocks_when_it_cannot_fit() {
        let rows = [json!({"port": 18181, "disk": "a-rather-long-name.ark"})];
        let columns = [("PORT", "port"), ("DISK", "disk")];

        let theme = Theme::fixed(24, Color::Off, true);
        let narrow = table(&theme, &rows, &columns);
        assert!(narrow.starts_with("  PORT  18181"), "{narrow}");
        for line in narrow.lines() {
            assert!(console::measure_text_width(line) <= theme.width, "{line}");
        }
        assert!(
            narrow
                .split_whitespace()
                .collect::<String>()
                .contains("a-rather-long-name.ark")
        );
    }

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
            "  ok qemu          11.1.1 on PATH\n  x acceleration   software emulation only\n    hint: add your user to the kvm group\n  - image          skipped: none chosen yet"
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
        assert!(lines.len() > 1);
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
            "ark\\u{1b}[2J\\nx"
        );
    }

    #[test]
    fn test_nothing_running_prints_as_none() {
        let theme = Theme::fixed(80, Color::Off, true);
        assert_eq!(table(&theme, &[], &[("PORT", "port")]), "  none");
    }

    #[test]
    fn test_sizes_read_in_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1 << 10), "1.0 KiB");
        assert_eq!(bytes(3 << 30), "3.0 GiB");
    }
}
