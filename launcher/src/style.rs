// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! What a terminal adds to a stream: the palette every Dark Bio tool paints
//! with, the glyphs and their ASCII twins, the width lines wrap at, and the
//! escaping that keeps text from a device or a file from steering the
//! terminal. None of it carries information, so a pipe loses the styling and
//! keeps every word.

use std::io::{self, IsTerminal as _};

use crate::platform;

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
        if self.unicode { unicode } else { ascii }
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

    /// A device's name arrives from the registry, so it can carry anything,
    /// and nothing it carries may reach the terminal as a control sequence.
    #[test]
    fn test_untrusted_text_cannot_steer_the_terminal() {
        assert_eq!(printable("ark\x1b[2J\r"), "ark\\u{1b}[2J\\r");
        assert_eq!(printable("two\nlines"), "two\nlines");
        assert_eq!(cell("two\nlines"), "two\\nlines");
    }
}
