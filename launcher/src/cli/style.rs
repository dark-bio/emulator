// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The palette every Dark Bio tool paints with, the glyphs and their ASCII
//! twins, the width lines wrap at, and the escaping that keeps text from a
//! device or a file from steering the terminal. Color carries no information,
//! so a pipe loses it and keeps every word, and the glyphs follow the locale
//! on a terminal and in a pipe alike.

use std::io::{self, IsTerminal as _};

use crate::platform;

/// The semantic roles every Dark Bio command line tool paints with.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Role {
    /// Content whose meaning needs no emphasis.
    Default,
    /// Help headings, a result's title and a check group.
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

    /// Whether the locale allows the Unicode glyphs, in a pipe too.
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
        let unicode = !json
            && locale.is_none_or(|locale| {
                let locale = locale.to_ascii_uppercase().replace('-', "");
                locale.contains("UTF8") || cfg!(windows)
            });
        let color = if !interactive
            || std::env::var_os("NO_COLOR").is_some()
            || std::env::var("CLICOLOR").is_ok_and(|value| value == "0")
        {
            Color::Off
        } else if platform::native_console(&terminal)
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
    pub(crate) fn style(&self, role: Role) -> clap::builder::styling::Style {
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
            Role::Heading => return Style::new().bold(),
            Role::Default => return Style::new(),
        };
        // Muted stays regular weight, and every other color is bold
        let style = if role == Role::Muted {
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
        if self.unicode { unicode } else { ascii }
    }

    /// Put a state's mark before its word, with the mark alone in the role's
    /// color.
    pub(crate) fn mark(&self, role: Role, text: &str) -> String {
        let icon = match role {
            Role::Success => self.glyph("\u{2713}", "ok"),
            Role::Failure => self.glyph("\u{2717}", "x"),
            Role::Attention => "!",
            _ => self.glyph("\u{00b7}", "-"),
        };
        format!("{} {text}", self.paint(role, icon))
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
            painted.push('`');
            painted.push_str(&self.paint(Role::Accent, code));
            painted.push('`');
            rest = tail;
        }
        painted.push_str(rest);
        painted
    }
}

/// Text from outside the tool, such as a device's name or a file's path, with
/// every control character spelled out, so it can neither steer the terminal
/// nor pass for a line of the tool's own. Line breaks stay, since a message
/// that carries several lines, such as a log tail, is meant to.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character.is_control() && character != '\n' {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

/// The same for one cell of a table or a block, where a line break would
/// start a new row.
pub(crate) fn cell(text: &str) -> String {
    printable(text).replace('\n', "\\n")
}

/// A duration in the house format, `14s`, `1m 02s` or `1h 05m`.
pub(crate) fn duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
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

    /// A role degrades from color to bold to plain with the stream's depth.
    #[test]
    fn test_a_role_degrades_from_color_to_bold_to_plain() {
        let theme = Theme::fixed(80, Color::True, true);
        assert_eq!(
            theme.paint(Role::Success, "ready"),
            "\x1b[1m\x1b[38;2;148;202;110mready\x1b[0m",
            "true color"
        );
        assert_eq!(
            Theme::fixed(80, Color::Ansi256, true).paint(Role::Success, "ready"),
            "\x1b[1m\x1b[38;5;150mready\x1b[0m",
            "256 colors"
        );
        assert_eq!(
            Theme::fixed(80, Color::Basic, true).paint(Role::Success, "ready"),
            "\x1b[1mready\x1b[0m",
            "bold only"
        );
        assert_eq!(
            Theme::fixed(80, Color::Off, true).paint(Role::Success, "ready"),
            "ready",
            "plain text"
        );
    }

    /// A mark and its word read the same without color, in either glyph set.
    #[test]
    fn test_a_state_keeps_its_word_without_color_or_glyphs() {
        let colored = Theme::fixed(80, Color::Off, true);
        assert_eq!(
            colored.mark(Role::Success, "done"),
            "✓ done",
            "Unicode mark"
        );
        let plain = Theme::fixed(80, Color::Off, false);
        assert_eq!(plain.mark(Role::Success, "done"), "ok done", "ASCII mark");
        assert_eq!(
            plain.mark(Role::Muted, "skipped"),
            "- skipped",
            "neutral mark"
        );
    }

    /// Only a state's mark carries its role's color and weight.
    #[test]
    fn test_state_words_stay_plain() {
        let theme = Theme::fixed(80, Color::True, true);
        for (role, expected) in [
            (Role::Success, "\x1b[1m\x1b[38;2;148;202;110m✓\x1b[0m done"),
            (Role::Attention, "\x1b[1m\x1b[38;2;232;162;74m!\x1b[0m done"),
            (Role::Failure, "\x1b[1m\x1b[38;2;235;96;112m✗\x1b[0m done"),
            (Role::Muted, "\x1b[38;2;124;128;152m·\x1b[0m done"),
        ] {
            assert_eq!(theme.mark(role, "done"), expected, "{expected:?}");
        }
    }

    /// Quoted commands keep their backticks, with and without color.
    #[test]
    fn test_inline_commands_keep_their_backticks() {
        for (color, expected) in [
            (
                Color::Off,
                "run `ark-emulator list`, then `ark-emulator stop`; keep `",
            ),
            (
                Color::Basic,
                "run `\x1b[1mark-emulator list\x1b[0m`, then `\x1b[1mark-emulator stop\x1b[0m`; keep `",
            ),
            (
                Color::True,
                "run `\x1b[1m\x1b[38;2;137;180;250mark-emulator list\x1b[0m`, then `\x1b[1m\x1b[38;2;137;180;250mark-emulator stop\x1b[0m`; keep `",
            ),
        ] {
            assert_eq!(
                Theme::fixed(80, color, true)
                    .inline("run `ark-emulator list`, then `ark-emulator stop`; keep `"),
                expected,
                "{expected:?}"
            );
        }
    }

    /// The locale alone picks the glyphs in a pipe, whatever the terminal
    /// settings say.
    #[test]
    fn test_piped_glyphs_follow_the_locale() {
        // Each case runs in a child process, since the locale is process-wide
        if let Ok(expected) = std::env::var("EMULATOR_TEST_GLYPH") {
            for stderr in [false, true] {
                let theme = Theme::new(false, stderr);
                assert_eq!(
                    theme.mark(Role::Success, "done"),
                    format!("{expected} done"),
                    "stderr={stderr}"
                );
                assert_eq!(
                    Theme::new(true, stderr).mark(Role::Success, "done"),
                    "ok done",
                    "JSON styling, stderr={stderr}"
                );
            }
            return;
        }

        for (all, ctype, lang, unicode) in [
            (None, None, None, true),
            (Some("en_US.UTF-8"), Some("C"), Some("C"), true),
            (Some("C"), Some("en_US.UTF-8"), Some("en_US.UTF-8"), false),
            (Some(""), Some("C.UTF8"), Some("C"), true),
            (None, Some("C"), Some("en_US.UTF-8"), false),
            (None, None, Some("en_US.UTF-8"), true),
            (None, None, Some("POSIX"), false),
        ] {
            for term in ["dumb", "xterm-256color"] {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "cli::style::tests::test_piped_glyphs_follow_the_locale",
                    ])
                    .env(
                        "EMULATOR_TEST_GLYPH",
                        if unicode || cfg!(windows) {
                            "✓"
                        } else {
                            "ok"
                        },
                    )
                    .env("TERM", term)
                    .env("COLORTERM", "truecolor")
                    .env("CLICOLOR_FORCE", "1")
                    .env_remove("NO_COLOR")
                    .env_remove("CLICOLOR");
                for (key, value) in [("LC_ALL", all), ("LC_CTYPE", ctype), ("LANG", lang)] {
                    if let Some(value) = value {
                        command.env(key, value);
                    } else {
                        command.env_remove(key);
                    }
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{all:?}/{ctype:?}/{lang:?}, {term}: {output:?}"
                );
            }
        }
    }

    /// A message keeps its own line breaks under the hanging indent.
    #[test]
    fn test_wrapping_keeps_the_lines_a_message_already_had() {
        let wrapped = wrap("the emulator exited\n[qemu] it would not start", 80, 4);
        assert_eq!(
            wrapped, "the emulator exited\n    [qemu] it would not start",
            "existing line break"
        );
    }

    /// Long paths wrap without losing text or exceeding the width.
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
            path.replace(' ', ""),
            "the path survives wrapping"
        );
    }

    /// A device's name arrives from the registry, so it can carry anything,
    /// and nothing it carries may reach the terminal as a control sequence.
    #[test]
    fn test_untrusted_text_cannot_steer_the_terminal() {
        assert_eq!(
            printable("ark\x1b[2J\r"),
            "ark\\u{1b}[2J\\r",
            "terminal controls"
        );
        assert_eq!(printable("two\nlines"), "two\nlines", "message line break");
        assert_eq!(cell("two\nlines"), "two\\nlines", "cell line break");
    }
}
