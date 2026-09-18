// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! What a command prints, in the two shapes every Dark Bio tool prints in.
//!
//! stdout carries the result and nothing else, as a block or a table for
//! reading or as one JSON document. stderr carries everything a person reads
//! along the way: notes, warnings, hints, steps, diagnostic logs and errors,
//! one line each, or one JSON object per line.
//!
//! Color, glyphs and width are what a terminal adds on top, and each stream
//! decides for itself, so a pipe sees the same rows without the styling. None
//! of that styling carries information: every state also has a word.

use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::Global;

/// Semantic emphasis, the same seven roles every Dark Bio tool paints with.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Role {
    /// Content whose meaning needs no emphasis.
    Default,
    /// Section titles, help headings and table headers.
    Heading,
    /// Completed or verified states.
    Success,
    /// Warnings and states needing action.
    Attention,
    /// Errors and failed checks.
    Failure,
    /// Commands, identifiers and links the reader may act on.
    Accent,
    /// Labels and secondary context.
    Muted,
    /// The staging environment's label.
    Staging,
    /// The develop environment's label.
    Develop,
}

/// How much styling a stream can carry.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Color {
    /// No escape sequences at all, not even bold.
    Off,
    /// Bold emphasis without palette colors.
    Basic,
    /// The palette approximated on the 256 color cube.
    Ansi256,
    /// The palette exactly.
    True,
}

/// What one stream can do, resolved once for the whole run.
#[derive(Clone)]
pub(crate) struct Theme {
    /// Whether this stream is a terminal that may be styled.
    pub(crate) interactive: bool,

    /// Whether the terminal and the locale allow the Unicode glyphs.
    pub(crate) unicode: bool,

    /// How much styling depth this stream has.
    pub(crate) color: Color,

    /// Width in display cells, falling back to 80 columns.
    pub(crate) width: usize,
}

impl Theme {
    /// Resolve one stream's capabilities from the terminal and the
    /// environment. A pipe keeps the reading layouts and loses the styling.
    pub(crate) fn new(json: bool, stderr: bool) -> Self {
        let terminal = if stderr {
            console::Term::stderr()
        } else {
            console::Term::stdout()
        };
        let attended = if stderr {
            io::stderr().is_terminal()
        } else {
            io::stdout().is_terminal()
        };
        let term = std::env::var("TERM").unwrap_or_default();
        let interactive = !json && attended && term != "dumb";
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.is_empty());
        let unicode = interactive
            && locale.is_none_or(|locale| {
                let locale = locale.to_ascii_uppercase().replace('-', "");
                locale.contains("UTF8") || cfg!(windows)
            });
        let color = if !interactive
            || std::env::var_os("NO_COLOR").is_some()
            || std::env::var("CLICOLOR").is_ok_and(|value| value == "0")
        {
            Color::Off
        } else if native_console(&terminal)
            || std::env::var("COLORTERM")
                .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        {
            Color::True
        } else if term.contains("256color") {
            Color::Ansi256
        } else {
            Color::Basic
        };
        let width = terminal
            .size_checked()
            .map_or(80, |(_, width)| usize::from(width).max(20));
        Self {
            interactive,
            unicode,
            color,
            width,
        }
    }

    /// The escape sequence a role is painted with, degrading to bold and then
    /// to nothing as the stream's depth runs out.
    fn style(&self, role: Role) -> clap::builder::styling::Style {
        use clap::builder::styling::{Ansi256Color, RgbColor, Style};
        if self.color == Color::Off || role == Role::Default {
            return Style::new();
        }
        let rgb = match role {
            Role::Success => (148, 202, 110),
            Role::Attention => (232, 162, 74),
            Role::Failure => (235, 96, 112),
            Role::Accent => (137, 180, 250),
            Role::Muted => (124, 128, 152),
            Role::Staging => (147, 153, 178),
            Role::Develop => (108, 112, 134),
            Role::Heading => return Style::new().bold(),
            Role::Default => return Style::new(),
        };
        // The three quiet roles stay plain so that what they sit beside reads
        // as the louder of the two.
        let style = if matches!(role, Role::Muted | Role::Staging | Role::Develop) {
            Style::new()
        } else {
            Style::new().bold()
        };
        match self.color {
            Color::True => style.fg_color(Some(RgbColor(rgb.0, rgb.1, rgb.2).into())),
            Color::Ansi256 => {
                let cell = |channel: u8| ((u16::from(channel) * 5 + 127) / 255) as u8;
                style.fg_color(Some(
                    Ansi256Color(16 + 36 * cell(rgb.0) + 6 * cell(rgb.1) + cell(rgb.2)).into(),
                ))
            }
            _ => style,
        }
    }

    /// Wrap `text` in a role's style and its reset, or leave it as it is.
    pub(crate) fn paint(&self, role: Role, text: impl AsRef<str>) -> String {
        let style = self.style(role);
        format!("{style}{}{style:#}", text.as_ref())
    }

    /// Pick a glyph or its ASCII twin, without changing what the line says.
    pub(crate) fn glyph<'a>(&self, unicode: &'a str, ascii: &'a str) -> &'a str {
        if self.unicode {
            unicode
        } else {
            ascii
        }
    }

    /// Put a role's mark before a state, so the state survives without color.
    pub(crate) fn mark(&self, role: Role, text: &str) -> String {
        let icon = match role {
            Role::Success => self.glyph("\u{2713}", "ok"),
            Role::Failure => self.glyph("\u{2717}", "x"),
            Role::Attention => "!",
            _ => self.glyph("\u{00b7}", "-"),
        };
        self.paint(role, format!("{icon} {text}"))
    }

    /// Cut `text` to `width` cells, marking what was dropped.
    pub(crate) fn truncate(&self, text: &str, width: usize) -> String {
        if console::measure_text_width(text) <= width {
            return text.to_owned();
        }
        let tail = self.glyph("\u{2026}", "...");
        let tail = if width < console::measure_text_width(tail) {
            ""
        } else {
            tail
        };
        console::truncate_str(text, width, tail).into_owned()
    }

    /// The same palette, handed to clap for the help it generates.
    pub(crate) fn clap(&self) -> clap::builder::styling::Styles {
        clap::builder::styling::Styles::plain()
            .header(self.style(Role::Heading))
            .usage(self.style(Role::Heading))
            .literal(self.style(Role::Accent))
            .placeholder(self.style(Role::Muted))
            .error(self.style(Role::Failure))
            .valid(self.style(Role::Success))
            .invalid(self.style(Role::Attention))
    }

    /// Paint what backticks enclose as a command the reader may run. An
    /// unmatched backtick stays the character it is.
    pub(crate) fn inline(&self, text: &str) -> String {
        let mut painted = String::new();
        let mut rest = text;
        while let Some((before, after)) = rest.split_once('`') {
            let Some((code, tail)) = after.split_once('`') else {
                break;
            };
            painted.push_str(before);
            painted.push_str(&self.paint(Role::Accent, code));
            rest = tail;
        }
        painted.push_str(rest);
        painted
    }
}

/// Whether this stream is a Windows console, which carries the full palette
/// without anything in the environment saying so.
#[cfg(windows)]
fn native_console(terminal: &console::Term) -> bool {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    let mut mode = 0;
    // SAFETY: a handle this process owns in, a status code out. The call
    // reads nothing through the pointer beyond the mode it writes.
    unsafe { GetConsoleMode(terminal.as_raw_handle(), &mut mode) != 0 }
}

#[cfg(not(windows))]
fn native_console(_terminal: &console::Term) -> bool {
    false
}

/// A failure, in the shape both outputs render it from.
#[derive(Debug)]
pub(crate) struct Error {
    /// Exit class, one of the numbers the house tools share.
    pub(crate) exit: i32,

    /// Stable code, which is what a caller matches on.
    pub(crate) code: &'static str,

    /// What went wrong, in a sentence.
    pub(crate) message: String,

    /// What to do next, wherever the tool knows.
    pub(crate) hints: Vec<String>,
}

impl Error {
    /// A failure with no next step to name.
    pub(crate) fn new(exit: i32, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit,
            code,
            message: message.into(),
            hints: Vec::new(),
        }
    }

    /// The same failure, with one more line saying what to do about it.
    pub(crate) fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }

    /// The error object, which is what `--json` carries.
    fn json(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }
}

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
    /// value lines, or the whole document as JSON.
    ///
    /// Each row names the document key it shows, and a dot reaches into a
    /// nested object. The reading view may carry fewer fields than the
    /// document; the document always carries them all.
    pub(crate) fn block(&self, document: &Value, rows: &[(&str, &str)]) {
        self.result(document, |theme| {
            let rows: Vec<(String, String)> = rows
                .iter()
                .map(|(label, key)| {
                    (
                        (*label).to_owned(),
                        value(theme, key, pick(document, key).unwrap_or(&Value::Null)),
                    )
                })
                .collect();
            block(theme, &rows)
        });
    }

    /// Print several results as a table, one row each, or the whole document
    /// as JSON. A table too wide for the terminal falls back to one block per
    /// row, and no rows at all print as `none`.
    pub(crate) fn table(&self, document: &Value, rows: &[Value], columns: &[(&str, &str)]) {
        self.result(document, |theme| table(theme, rows, columns));
    }

    /// Claim the one result and write it. A second claim is ignored, so an
    /// error after a partial result cannot replace it.
    fn result(&self, document: &Value, render: impl FnOnce(&Theme) -> String) {
        if self.0.printed.swap(true, Ordering::SeqCst) {
            return;
        }
        let text = if self.json() {
            serde_json::to_string_pretty(document).expect("a result serializes")
        } else {
            render(&self.0.out)
        };
        let mut spacing = self.0.spacing.lock().expect("output not poisoned");
        let mut stdout = io::stdout().lock();
        if self.0.out.interactive && self.0.err.interactive && spacing.err_printed {
            let _ = writeln!(stdout);
        }
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
        spacing.out_block = self.0.out.interactive;
    }

    /// Whether a result has been claimed, which is what keeps a failure from
    /// replacing one.
    pub(crate) fn printed(&self) -> bool {
        self.0.printed.load(Ordering::SeqCst)
    }

    /// Give stdout up to something else for this run, such as a guest console.
    /// Nothing this layer would have written there is written at all.
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
        let message = message.as_ref();
        let mut spacing = self.0.spacing.lock().expect("output not poisoned");
        let mut stderr = io::stderr().lock();
        if self.json() {
            let _ = writeln!(stderr, "{}", json!({"event": kind, "message": message}));
        } else {
            separate(&mut spacing, &mut stderr);
            let _ = writeln!(stderr, "{}", event(&self.0.err, kind, message));
        }
        spacing.err_printed = true;
        let _ = stderr.flush();
    }

    /// Report a failure and the steps out of it. Under JSON the error object
    /// also lands on stdout when there is no result to replace.
    pub(crate) fn error(&self, error: &Error) {
        if self.json() {
            if !self.printed() {
                self.0.printed.store(true, Ordering::SeqCst);
                let mut stdout = io::stdout().lock();
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string_pretty(&error.json()).expect("an error serializes")
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
            let line = format!(
                "{} {}",
                theme.paint(Role::Failure, format!("error[{}]:", error.code)),
                theme.inline(&error.message)
            );
            let _ = writeln!(stderr, "{}", wrap(&line, theme.width, error.code.len() + 9));
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
            return Err(Error::new(1, "confirmation-required", refusal)
                .hint(format!("pass `{flag}` to confirm without being asked")));
        }
        {
            let mut spacing = self.0.spacing.lock().expect("output not poisoned");
            let mut stderr = io::stderr().lock();
            separate(&mut spacing, &mut stderr);
            let theme = &self.0.err;
            let line = if theme.interactive {
                format!(
                    "{} {} {}",
                    theme.paint(Role::Attention, "?"),
                    theme.inline(question),
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
            .map_err(|err| Error::new(1, "io", format!("could not read the answer: {err}")))?;
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

/// The value at `key`, where a dot reaches into a nested object.
fn pick<'a>(document: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(document, |value, part| value.get(part))
}

/// One field as a person reads it, with its unit, its mark and its color.
fn value(theme: &Theme, key: &str, value: &Value) -> String {
    let key = key.rsplit('.').next().unwrap_or(key);
    match value {
        Value::Null => return theme.paint(Role::Muted, "-"),
        Value::Array(values) if values.is_empty() => return theme.paint(Role::Muted, "none"),
        Value::Bool(state) if key == "ready" => {
            let role = if *state { Role::Success } else { Role::Default };
            return theme.mark(role, if *state { "yes" } else { "no" });
        }
        _ => {}
    }
    if key.ends_with("_bytes") {
        if let Some(count) = value.as_u64() {
            return bytes(count);
        }
    }
    let text = scalar(value);
    match key {
        "env" => theme.paint(
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

/// One row per result, with muted uppercase headers. A table that does not fit
/// becomes one block per row, so nothing is lost to the width.
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
            out.push_str(&if header {
                theme.paint(Role::Muted, cell)
            } else {
                cell
            });
            if index + 1 < cells.len() {
                out.push_str(&padding);
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

/// Wrap styled text at `width` cells, indenting every line after the first by
/// `indent`. Styling sequences take no width, and a word too long for a line
/// of its own is broken rather than dropped.
pub(crate) fn wrap(text: &str, width: usize, indent: usize) -> String {
    let width = width.max(1);
    let indent = indent.min(width - 1);
    let mut out = String::new();
    let mut column = 0;
    let place = |out: &mut String, column: &mut usize, word: &str| {
        // A word that would run past the margin starts the next line. One too
        // long for a line of its own does not, and the loop below breaks it.
        let size = console::measure_text_width(word.trim_end());
        if *column > indent && *column + size > width && size <= width - indent {
            out.push('\n');
            out.push_str(&" ".repeat(indent));
            *column = indent;
        }
        for (part, ansi) in console::AnsiCodeIterator::new(word) {
            if ansi {
                out.push_str(part);
                continue;
            }
            for character in part.chars() {
                // A message that carries several lines, such as a log tail,
                // keeps them, each under the same hanging indent.
                if character == '\n' {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent));
                    *column = indent;
                    continue;
                }
                let cell = console::measure_text_width(character.encode_utf8(&mut [0; 4]));
                if *column + cell > width {
                    // A space at the margin is where the line ends anyway.
                    if character.is_whitespace() {
                        continue;
                    }
                    out.push('\n');
                    out.push_str(&" ".repeat(indent));
                    *column = indent;
                }
                out.push(character);
                *column += cell;
            }
        }
    };
    let mut word = String::new();
    for (part, ansi) in console::AnsiCodeIterator::new(text) {
        if ansi {
            word.push_str(part);
            continue;
        }
        for character in part.chars() {
            word.push(character);
            if character.is_whitespace() {
                place(&mut out, &mut column, &word);
                word.clear();
            }
        }
    }
    place(&mut out, &mut column, &word);
    // A line that broke after a space would otherwise end in one.
    out.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
impl Theme {
    /// A stream with the capabilities a test wants to render against.
    pub(crate) fn fixed(width: usize, color: Color, unicode: bool) -> Self {
        Self {
            interactive: true,
            unicode,
            color,
            width,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_role_degrades_from_color_to_bold_to_plain() {
        let theme = Theme::fixed(80, Color::True, true);
        assert_eq!(
            theme.paint(Role::Success, "ready"),
            "\x1b[1m\x1b[38;2;148;202;110mready\x1b[0m"
        );
        assert_eq!(
            Theme::fixed(80, Color::Ansi256, true).paint(Role::Success, "ready"),
            "\x1b[1m\x1b[38;5;150mready\x1b[0m"
        );
        assert_eq!(
            Theme::fixed(80, Color::Basic, true).paint(Role::Success, "ready"),
            "\x1b[1mready\x1b[0m"
        );
        assert_eq!(
            Theme::fixed(80, Color::Off, true).paint(Role::Success, "ready"),
            "ready"
        );
    }

    #[test]
    fn test_a_state_keeps_its_word_without_color_or_glyphs() {
        let colored = Theme::fixed(80, Color::Off, true);
        assert_eq!(colored.mark(Role::Success, "yes"), "\u{2713} yes");
        let plain = Theme::fixed(80, Color::Off, false);
        assert_eq!(plain.mark(Role::Success, "yes"), "ok yes");
        assert_eq!(plain.mark(Role::Default, "no"), "- no");
    }

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
    fn test_wrapping_keeps_the_lines_a_message_already_had() {
        let wrapped = wrap("the emulator exited\n[qemu] it would not start", 80, 4);
        assert_eq!(
            wrapped,
            "the emulator exited\n    [qemu] it would not start"
        );
    }

    #[test]
    fn test_wrapping_keeps_every_line_within_the_width() {
        let theme = Theme::fixed(32, Color::True, true);
        let path = "/Users/someone/Library/Application Support/bio.dark.emulator/logs/18181.log";
        let wrapped = wrap(&theme.paint(Role::Accent, path), 32, 4);
        for line in wrapped.lines() {
            assert!(console::measure_text_width(line) <= 32, "{line}");
        }
        assert_eq!(
            console::strip_ansi_codes(&wrapped)
                .split_whitespace()
                .collect::<String>(),
            path.replace(' ', "")
        );
    }

    #[test]
    fn test_a_block_aligns_labels_and_spells_absent_values() {
        let theme = Theme::fixed(80, Color::Off, true);
        let document = json!({"locator": "emulator:18181", "env": null, "ready": true});
        let rows = [
            (
                "Locator".to_owned(),
                value(&theme, "locator", &document["locator"]),
            ),
            ("Env".to_owned(), value(&theme, "env", &document["env"])),
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
    fn test_a_table_falls_back_to_blocks_when_it_cannot_fit() {
        let rows = [json!({"port": 18181, "disk": "a-rather-long-name.ark"})];
        let columns = [("PORT", "port"), ("DISK", "disk")];

        let wide = table(&Theme::fixed(80, Color::Off, true), &rows, &columns);
        assert_eq!(wide, "  PORT   DISK\n  18181  a-rather-long-name.ark");

        let theme = Theme::fixed(24, Color::Off, true);
        let narrow = table(&theme, &rows, &columns);
        assert!(narrow.starts_with("  PORT  18181"), "{narrow}");
        for line in narrow.lines() {
            assert!(console::measure_text_width(line) <= theme.width, "{line}");
        }
        assert!(narrow
            .split_whitespace()
            .collect::<String>()
            .contains("a-rather-long-name.ark"));
    }

    #[test]
    fn test_nothing_running_prints_as_none() {
        let theme = Theme::fixed(80, Color::Off, true);
        assert_eq!(table(&theme, &[], &[("PORT", "port")]), "  none");
    }

    #[test]
    fn test_a_nested_field_is_reached_by_its_path() {
        let document = json!({"firmware": {"version": "v0.11.5"}, "qemu": null});
        assert_eq!(pick(&document, "firmware.version"), Some(&json!("v0.11.5")));
        assert_eq!(pick(&document, "qemu.version"), None);
        assert_eq!(pick(&document, "missing"), None);
    }

    #[test]
    fn test_sizes_read_in_binary_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1 << 10), "1.0 KiB");
        assert_eq!(bytes(3 << 30), "3.0 GiB");
    }
}
