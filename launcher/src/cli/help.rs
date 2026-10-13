// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The manual, which is the only thing an agent plans from.
//!
//! Every command page carries its contract, with what the command requires,
//! how long it takes and its JSON keys, then two examples. `-h`, `--help` and `ark-emulator help <command>` print the same
//! page, and the topics are the reference a reader arrives at from it.
//!
//! The tree built here is also the one a run is parsed with, so the pages and
//! the arguments cannot drift apart. Help prints text on stdout whatever the
//! run asked for, since a caller that wanted JSON still has to read this.

use clap::CommandFactory as _;

use super::args::Cli;
use super::style::{self, Color, Role, Theme};
use crate::error::{Code, Error};

/// The topics this build carries, in the order the manual prints them.
const TOPICS: [&str; 5] = ["agents", "images", "registry", "output", "errors"];

/// The widest a help page or topic gets, in cells, even on a wider terminal.
const WIDTH: usize = 80;

/// The options every command takes, in the root page's order.
const GLOBAL: [&str; 6] = ["json", "no_input", "timeout", "quiet", "verbose", "log"];

/// The boot options, for a run with no command or for `start`, in page order.
const BOOT: [&str; 8] = [
    "headless", "image", "env", "memory", "arch", "kernel", "initrd", "port",
];

/// What a help page is styled with. It is written for reading even when the
/// run answers in JSON, so the theme never follows that flag.
fn theme() -> Theme {
    Theme::new(false, false)
}

/// The command tree with the palette and every contract attached. Nothing
/// touches an argument once the tree is built, since clap indexes them at the
/// build and a moved argument would answer to another's flag.
fn command(theme: &Theme) -> clap::Command {
    let mut command = Cli::command();
    decorate(&mut command, "", theme);
    command.build();
    compact(&mut command, theme, true);
    command
}

/// The tree a run is parsed with. It is the help tree, so that `-h` and
/// `--help` after a command print the page `help <command>` prints.
pub(crate) fn parser() -> clap::Command {
    command(&theme())
}

/// Prints a command's page, a topic, or the whole manual.
pub(crate) fn run(path: &[String], all: bool) -> Result<(), Error> {
    let theme = theme();
    let mut root = command(&theme);
    if all {
        return print(&manual(&mut root, &theme));
    }
    if let [name] = path
        && let Some(text) = topic(name)
    {
        return print(&markdown(&theme, text));
    }
    let mut command = &mut root;
    for (index, name) in path.iter().enumerate() {
        if command.find_subcommand(name).is_none() {
            return Err(unknown_page(command, path, index));
        }
        command = command.find_subcommand_mut(name).expect("a known command");
    }
    print(command.render_long_help().ansi().to_string().trim_end())
}

/// The usage error for an unknown page, with clap's near match or the
/// parent's help as its hint.
fn unknown_page(command: &clap::Command, path: &[String], index: usize) -> Error {
    let name = &path[index];
    let suggestions = command
        .clone()
        .try_get_matches_from([command.get_name(), name])
        .err()
        .map(|error| {
            super::usage::suggestions(&error, clap::error::ContextKind::SuggestedSubcommand)
        })
        .unwrap_or_default();
    if index == 0 && suggestions.is_empty() {
        let error = Error::new(Code::Usage, format!("unknown help topic '{name}'"));
        return if let Some(suggested) = super::usage::similar(name, TOPICS) {
            error.hint(format!(
                "a similar topic exists: `ark-emulator help {suggested}`"
            ))
        } else {
            error.hint("see `ark-emulator help --help`")
        };
    }

    // Command suggestions use the path to the page's parent
    let parent = std::iter::once("ark-emulator")
        .chain(path[..index].iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let mut error = Error::new(Code::Usage, format!("unknown help command '{name}'"));
    for suggested in suggestions {
        error = error.hint(format!("a similar command exists: `{parent} {suggested}`"));
    }
    if error.hints.is_empty() {
        error = error.hint(format!("see `{parent} --help`"));
    }
    error
}

/// Write a page to stdout, reporting a reader that has gone away rather than
/// panicking on it.
fn print(text: &str) -> Result<(), Error> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .map_err(|err| Error::io(format!("could not print the help: {err}")))
}

/// Attach the palette, the contract lines and the examples to one command and
/// everything under it. clap keeps owning the syntax and the argument help.
fn decorate(command: &mut clap::Command, parent: &str, theme: &Theme) {
    *command = command
        .clone()
        .styles(theme.clap())
        .term_width(theme.width.min(WIDTH))
        .color(if theme.color == Color::Off {
            clap::ColorChoice::Never
        } else {
            clap::ColorChoice::Always
        });
    // The help flag is defined here rather than left to clap, so that the
    // root's line can say where the manual is; clap would add a second one
    // beside it otherwise, so the tree disables its own.
    let help = clap::Arg::new("help")
        .short('h')
        .long("help")
        .action(clap::ArgAction::Help);
    if parent.is_empty() {
        // Boot options apply to a run with no command and to `start`
        *command = command
            .clone()
            .override_usage("ark-emulator [OPTIONS]\n       ark-emulator <COMMAND>")
            .arg(help.help("Print help; `ark-emulator help --all` is the manual"));
    } else {
        *command = command.clone().arg(help.help("Print help"));
    }
    let path = if parent.is_empty() {
        command.get_name().to_owned()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let key = path.strip_prefix("ark-emulator ").unwrap_or(&path);
    let contract = CONTRACTS
        .iter()
        .find(|contract| contract.path == key)
        .unwrap_or_else(|| panic!("missing help contract for {key}"));
    let closing = format!(
        "Output is formatted for reading; --json keeps complete, exact values.
AI agents: read `ark-emulator help agents` first.
Topics: {}.",
        TOPICS.join(", ")
    );
    let closing = closing
        .lines()
        .map(|line| style::wrap(&theme.inline(line), theme.width.min(WIDTH), 0))
        .collect::<Vec<_>>()
        .join("\n");

    // -h, --help and help <command> all carry the paragraph and the contract
    let paragraph = style::wrap(&theme.inline(contract.paragraph), theme.width.min(WIDTH), 0);
    let help = if parent.is_empty() {
        let footer = footer(
            theme,
            contract.footer.as_ref().expect("the root's contract"),
            "Contract with no command:",
        );
        format!("{footer}\n\n{}\n\n{closing}", exit_codes(theme))
    } else if let Some(fields) = &contract.footer {
        footer(theme, fields, "Contract:")
    } else {
        format!(
            "Each subcommand has its own requirements and output.\nRead `{} COMMAND --help` for its contract.",
            path
        )
    };
    let help = help
        .lines()
        .map(|line| style::wrap(line, theme.width.min(WIDTH), hanging(line)))
        .collect::<Vec<_>>()
        .join("\n");
    *command = command
        .clone()
        .before_help(paragraph.clone())
        .before_long_help(paragraph)
        .after_help(format!("{help}\n"))
        .after_long_help(format!("{help}\n"));
    for child in command.get_subcommands_mut() {
        decorate(child, &path, theme);
    }
}

/// Lays out every page the same for each way of asking for help. Global
/// options hide only on a display copy, so the parser keeps them.
fn compact(command: &mut clap::Command, theme: &Theme, root: bool) {
    let mut display = command
        .clone()
        .before_help(None)
        .before_long_help(None)
        .after_help(None)
        .after_long_help(None);
    if root {
        for (order, name) in GLOBAL.into_iter().chain(["version", "help"]).enumerate() {
            display = display.mut_arg(name, |arg| arg.display_order(order));
        }
        for (order, name) in BOOT.into_iter().enumerate() {
            display = display.mut_arg(name, |arg| arg.display_order(order + GLOBAL.len() + 2));
        }
    } else {
        for name in GLOBAL {
            display = display.mut_arg(name, |arg| arg.hide(true));
        }
    }

    // Group the root commands and wrap every row under its own description
    let rendered = display.render_help().ansi().to_string();
    let scan = rendered
        .lines()
        .map(|line| {
            let plain = console::strip_ansi_codes(line);
            let before = if root && plain.starts_with("  doctor ") {
                "\n".to_owned()
            } else if root && plain.split_whitespace().next() == Some("--headless") {
                format!(
                    "\n{}\n",
                    style::wrap(
                        &theme.paint(
                            Role::Heading,
                            "Boot options, with no command or with start:"
                        ),
                        theme.width.min(WIDTH),
                        0,
                    )
                )
            } else {
                String::new()
            };
            format!(
                "{before}{}",
                style::wrap(&theme.inline(line), theme.width.min(WIDTH), hanging(line))
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (description, scan) = scan.split_once("\n\n").unwrap_or((&scan, ""));
    *command = command.clone().help_template(format!(
        "{description}\n\n{{before-help}}{}{{after-help}}",
        scan.trim_end()
    ));

    // Subcommands use the same layout for their own paragraphs and footers
    for child in command.get_subcommands_mut() {
        compact(child, theme, false);
    }
}

/// The column a rendered help line's text starts in, so that wrapping it
/// keeps clap's layout. A row puts its text after a run of two or more
/// spaces; a paragraph has none and wraps to the margin.
fn hanging(line: &str) -> usize {
    let plain = console::strip_ansi_codes(line);
    let body = plain.trim_start();
    let indent = plain.len() - body.len();
    match body.find("  ") {
        Some(gap) => {
            let rest = &body[gap..];
            indent + gap + (rest.len() - rest.trim_start().len())
        }
        None => indent,
    }
}

/// A command's paragraph and optional footer, selected by its path.
struct Contract {
    /// Command words after `ark-emulator`, or `ark-emulator` for the root.
    path: &'static str,
    /// Explanation between the one-line description and usage.
    paragraph: &'static str,
    /// The contract of a run with no command or of a leaf, absent on a group.
    footer: Option<ContractFooter>,
}

/// A command's prerequisites, duration, outcome and examples.
struct ContractFooter {
    /// Conditions the command needs before it can work.
    requires: &'static str,
    /// Expected duration and the waits the timeout bounds.
    time: &'static str,
    /// Every key of the JSON document, or what prints in its place.
    json: &'static str,
    /// Two invocations printed as bare commands.
    examples: &'static str,
}

/// Command paragraphs and contract fields in command-tree order.
const CONTRACTS: &[Contract] = &[
    Contract {
        path: "ark-emulator",
        paragraph: "ark-emulator boots the real Ark firmware in QEMU on this computer. The \
            whole device is one plain disk image, so real data belongs on hardware. \
            `ark`, from https://github.com/dark-bio/cli, talks to an emulated Ark as \
            to hardware, and the owner approves its requests on their phone in Ark \
            Companion. ark-emulator itself needs no approval.",
        footer: Some(ContractFooter {
            requires: "a display unless --headless; a source build needs --kernel, --initrd \
                and QEMU on PATH",
            time: "until the window closes, `ark-emulator stop`, Ctrl-C or SIGTERM; the \
                firmware boots in about 10s with hardware acceleration; --timeout plays \
                no part",
            json: "none; stdout carries the guest console with --kernel and --initrd, and \
                stderr narrates the boot",
            examples: "ark-emulator\nark-emulator --headless --image ~/arks/dev.ark",
        }),
    },
    Contract {
        path: "start",
        paragraph: "The emulator runs in a process of its own, in its window unless \
            --headless, until `ark-emulator stop`. Its locator is what `ark --device` \
            takes. An image that is already booted is reported, never booted twice. A \
            start that runs past --timeout fails with timeout while the emulator keeps \
            booting. Which image a start boots is in `ark-emulator help images`.",
        footer: Some(ContractFooter {
            requires: "a free port from 18181 to 18280, or --port; a display unless \
                --headless; a source build needs --kernel and --initrd",
            time: "about 10s with hardware acceleration, minutes without; --timeout \
                bounds the wait, after which the emulator keeps booting",
            json: "locator, port, image, ready, environment, name, serial, expires, log, \
                path, created, started",
            examples: "ark-emulator start\nark-emulator start --headless --image ~/arks/dev.ark --json",
        }),
    },
    Contract {
        path: "list",
        paragraph: "It reads the registry this computer keeps and connects to no emulator. \
            The name, serial, environment and expiry are what each emulator's firmware \
            reported, absent until it has and never verified. `ark devices` shows the \
            same emulators beside hardware Arks.",
        footer: Some(ContractFooter {
            requires: "nothing",
            time: "immediate",
            json: "emulators, each with locator, port, image, ready, environment, name, \
                serial, expires and log",
            examples: "ark-emulator list\nark-emulator list --json",
        }),
    },
    Contract {
        path: "stop",
        paragraph: "It waits until the emulator's launcher and QEMU have exited. The guest \
            gets no warning, like hardware that loses power, so stop an emulator only \
            while no `ark` command runs against it. With several running, name one as \
            `ark --device` names it, or pass --all, which stops them in port order. A \
            stop that runs past --timeout fails with timeout while the emulator may \
            still be shutting down.",
        footer: Some(ContractFooter {
            requires: "a running emulator; --all needs none",
            time: "seconds; --timeout bounds the wait for the emulators to exit",
            json: "stopped, the locators that stopped, kept when a later one fails",
            examples: "ark-emulator stop\nark-emulator stop emulator:18181 --json",
        }),
    },
    Contract {
        path: "button",
        paragraph: "The window and the command line hold the button independently, and it \
            is down while either holds it. While `ark pair` waits for the button, a \
            press confirms the pairing at once. At any other time, a hold of 5s arms a \
            factory reset, which erases the device's data and pairing once the button \
            is let go, and keeps its enrollment.",
        footer: None,
    },
    Contract {
        path: "button press",
        paragraph: "A plain press is released before the command returns, which is what \
            `ark pair` waits for. --hold keeps the button down until \
            `ark-emulator button release`, and --hold-for releases it after that many \
            seconds. A hold outlives the command and ends if the emulated hardware \
            disconnects. A press while the command line holds the button sends no \
            second press, and replaces or cancels the timer. A reply that does not \
            come fails with timeout, and the button's state is then unknown.",
        footer: Some(ContractFooter {
            requires: "a booted emulator",
            time: "under a second; --timeout bounds each reply; --hold-for returns once \
                the release is scheduled",
            json: "locator, pressed, cli_pressed, changed, release_after_seconds, which is \
                0 for a press and null under --hold",
            examples: "ark-emulator button press\nark-emulator button press emulator:18181 --hold-for 5 --json",
        }),
    },
    Contract {
        path: "button release",
        paragraph: "It also cancels the release --hold-for scheduled. The button stays \
            down while the window holds it. Releasing a button the command line does \
            not hold succeeds without a change, so it clears a press whose outcome is \
            unknown.",
        footer: Some(ContractFooter {
            requires: "a booted emulator",
            time: "under a second; --timeout bounds each reply",
            json: "locator, pressed, cli_pressed, changed, release_after_seconds, which \
                is null",
            examples: "ark-emulator button release\nark-emulator button release emulator:18181 --json",
        }),
    },
    Contract {
        path: "wipe",
        paragraph: "It replaces the image with an empty one at the same path, so the \
            device needs `ark enroll` and `ark pair` again. Without PATH it wipes the \
            image a start would boot. An image an emulator holds fails with disk-busy, \
            so stop that emulator first.",
        footer: Some(ContractFooter {
            requires: "an image no emulator holds; confirmation at a terminal, or --yes",
            time: "a second",
            json: "path, wiped",
            examples: "ark-emulator wipe\nark-emulator wipe ~/arks/dev.ark --yes",
        }),
    },
    Contract {
        path: "doctor",
        paragraph: "It runs every check in order and prints the whole report, closed by a \
            count of problems and warnings. It covers this build's version against the \
            newest release, QEMU, the bundled firmware, hardware acceleration, the data \
            directory, the settings, the remembered image, the log directory, the \
            registry and a free port. It applies no fixes, and exits with the first \
            failed check's code.",
        footer: Some(ContractFooter {
            requires: "nothing; a check that cannot run is skipped",
            time: "seconds; --timeout bounds the release lookup",
            json: "version, and checks, each with name, result, detail and hint",
            examples: "ark-emulator doctor\nark-emulator doctor --json",
        }),
    },
    Contract {
        path: "completions",
        paragraph: "It prints the script for the shell on stdout, even under --json, and \
            never installs it.",
        footer: Some(ContractFooter {
            requires: "nothing",
            time: "immediate",
            json: "none; the script prints as text",
            examples: "ark-emulator completions zsh\nark-emulator completions bash",
        }),
    },
    Contract {
        path: "help",
        paragraph: "`ark-emulator help --all` prints the whole manual, starting with the \
            root page and the agents topic. The topics are agents, images, registry, \
            output and errors. Help prints text even under --json.",
        footer: Some(ContractFooter {
            requires: "nothing",
            time: "immediate",
            json: "none; help prints as text",
            examples: "ark-emulator help agents\nark-emulator help start",
        }),
    },
];

/// The contract with its values aligned in one column, then the examples as
/// commands a reader can paste.
fn footer(theme: &Theme, fields: &ContractFooter, heading: &str) -> String {
    let rows = [
        ("Requires", fields.requires),
        ("Time", fields.time),
        ("JSON", fields.json),
    ];
    let column = rows.iter().map(|(label, _)| label.len() + 4).max().unwrap();
    let mut lines = vec![theme.paint(Role::Heading, heading)];
    lines.extend(rows.into_iter().map(|(label, text)| {
        style::wrap(
            &format!(
                "  {}{}{}",
                theme.paint(Role::Muted, label),
                " ".repeat(column - label.len() - 2),
                theme.inline(text)
            ),
            theme.width.min(WIDTH),
            column,
        )
    }));

    // Examples have their own heading and command color
    lines.push(format!("\n{}", theme.paint(Role::Heading, "Examples:")));
    lines.extend(fields.examples.lines().map(|line| {
        style::wrap(
            &format!("  {}", theme.paint(Role::Accent, line)),
            theme.width.min(WIDTH),
            2,
        )
    }));
    lines.join("\n")
}

/// The exit codes in one column, muted numbers beside plain meanings.
fn exit_codes(theme: &Theme) -> String {
    let mut lines = vec![theme.paint(Role::Heading, "Exit codes:")];
    for (code, meaning) in [
        ("0", "done"),
        ("1", "a local file, a confirmation or a failed launch"),
        ("2", "usage"),
        ("3", "an emulator in the way or out of reach"),
        ("7", "a wait that ran out"),
        ("130", "Ctrl-C, or Ctrl-Break on Windows"),
        ("143", "SIGTERM, or a closed console"),
    ] {
        lines.push(style::wrap(
            &format!(
                "  {}  {meaning}",
                theme.paint(Role::Muted, format!("{code:>3}"))
            ),
            theme.width.min(WIDTH),
            7,
        ));
    }
    lines.join("\n")
}

/// Render a topic: headings, paragraphs, bullets with their continuation
/// lines, and code either fenced or indented by four spaces. The lines of a
/// paragraph are joined before they are wrapped, so the source's own line
/// breaks never show.
fn markdown(theme: &Theme, text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut paragraph: Option<(String, usize)> = None;
    let mut fenced = false;
    let mut bullet = false;
    let mut block = None;
    let flush = |paragraph: &mut Option<(String, usize)>, lines: &mut Vec<String>| {
        if let Some((text, hanging)) = paragraph.take() {
            lines.push(style::wrap(
                &theme.inline(&text),
                theme.width.min(WIDTH),
                hanging,
            ));
        }
    };
    for line in text.lines() {
        if line.starts_with("```") {
            flush(&mut paragraph, &mut lines);
            fenced = !fenced;
            continue;
        }
        let content = line.trim_start();
        let indent = line.len() - content.len();
        if fenced {
            flush(&mut paragraph, &mut lines);
            let code = format!("  {}", theme.paint(Role::Accent, line));
            lines.push(style::wrap(&code, theme.width.min(WIDTH), 2));
            continue;
        }
        if indent >= 4 && !content.is_empty() {
            flush(&mut paragraph, &mut lines);
            let base = *block.get_or_insert(indent);
            let code = &line[base.min(indent)..];
            let pad = if bullet { 4 } else { 2 };
            let code = format!("{}{}", " ".repeat(pad), theme.paint(Role::Accent, code));
            lines.push(style::wrap(&code, theme.width.min(WIDTH), pad));
            continue;
        }
        block = None;
        if content.is_empty() {
            flush(&mut paragraph, &mut lines);
            lines.push(String::new());
        } else if line.starts_with('#') {
            flush(&mut paragraph, &mut lines);
            bullet = false;
            lines.push(style::wrap(
                &theme.paint(Role::Heading, content.trim_start_matches('#').trim_start()),
                theme.width.min(WIDTH),
                0,
            ));
        } else if line.starts_with("- ") {
            flush(&mut paragraph, &mut lines);
            bullet = true;
            paragraph = Some((format!("  {line}"), 4));
        } else if let Some((text, _)) = &mut paragraph {
            text.push(' ');
            text.push_str(content);
        } else if bullet && indent > 0 {
            paragraph = Some((format!("    {content}"), 4));
        } else {
            bullet = false;
            paragraph = Some((line.to_owned(), 0));
        }
    }
    flush(&mut paragraph, &mut lines);
    lines.join("\n").trim_end().to_owned()
}

/// Renders the root, agents topic, command tree and remaining topics, divided
/// by the muted rule.
fn manual(root: &mut clap::Command, theme: &Theme) -> String {
    let mut pages = Vec::new();
    collect(root, &mut pages);
    pages.insert(
        1,
        markdown(theme, topic(TOPICS[0]).expect("the agents topic")),
    );
    pages.extend(
        TOPICS[1..]
            .iter()
            .map(|name| markdown(theme, topic(name).expect("a listed topic"))),
    );
    let rule = theme.paint(Role::Muted, "-".repeat(theme.width.min(WIDTH)));
    pages.join(&format!("\n\n{rule}\n\n"))
}

/// Every command's page, in the order the tree holds them.
fn collect(command: &mut clap::Command, pages: &mut Vec<String>) {
    pages.push(
        command
            .render_long_help()
            .ansi()
            .to_string()
            .trim_end()
            .to_owned(),
    );
    for child in command.get_subcommands_mut() {
        collect(child, pages);
    }
}

/// A topic by its public name, compiled into the binary.
fn topic(name: &str) -> Option<&'static str> {
    Some(match name {
        "agents" => include_str!("help/agents.md"),
        "images" => include_str!("help/images.md"),
        "registry" => include_str!("help/registry.md"),
        "output" => include_str!("help/output.md"),
        "errors" => include_str!("help/errors.md"),
        _ => return None,
    })
}

/// Tests of the rendered pages, the parser and the topics.
#[cfg(test)]
mod tests {
    use super::*;

    /// Every command the tool has, in the order the root lists them.
    const COMMANDS: [&str; 8] = [
        "start",
        "list",
        "stop",
        "button",
        "wipe",
        "doctor",
        "completions",
        "help",
    ];

    /// Every leaf includes the contract rows and two examples.
    #[test]
    fn test_every_command_states_its_whole_contract() {
        let theme = Theme::fixed(80, Color::Off, false);
        let mut root = command(&theme);
        for name in COMMANDS {
            if name == "button" {
                continue;
            }
            let page = root
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            for label in [
                "Contract:",
                "  Requires  ",
                "  Time      ",
                "  JSON      ",
                "Examples:",
            ] {
                assert!(page.contains(label), "{name}: {label}");
            }
            assert_eq!(page.matches("\n  ark-emulator ").count(), 2, "{name}");
            assert!(!page.contains("$ "), "{name}");
        }
    }

    /// Group help describes its leaves, and each button leaf carries a contract.
    #[test]
    fn test_button_help_has_nested_contracts() {
        let theme = Theme::fixed(80, Color::Off, false);
        let mut root = command(&theme);
        let group = root.find_subcommand_mut("button").unwrap();
        let page = group.render_long_help().to_string();
        assert!(!page.contains("Contract:"), "groups carry no contract");
        assert!(
            page.contains("Each subcommand has its own requirements and output"),
            "group footer"
        );
        for name in ["press", "release"] {
            let page = group
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            for label in [
                "Contract:",
                "  Requires  ",
                "  Time      ",
                "  JSON      ",
                "Examples:",
            ] {
                assert!(page.contains(label), "{name}: {label}");
            }
            assert_eq!(
                page.matches(&format!("\n  ark-emulator button {name}"))
                    .count(),
                2,
                "{name} examples"
            );
            assert!(!page.contains("--no-input"), "{name} global options");
        }
    }

    /// A one-liner is what the root lists, so it has to fit beside its name.
    #[test]
    fn test_every_one_liner_is_short_and_imperative() {
        let theme = Theme::fixed(80, Color::Off, false);
        let root = command(&theme);
        for command in root.get_subcommands() {
            let line = command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(line.len() < 60, "{}: {line:?}", command.get_name());
            assert!(!line.ends_with('.'), "{}: {line:?}", command.get_name());
        }
    }

    /// The root page states the contract of a run with no command and names
    /// every topic.
    #[test]
    fn test_a_run_with_no_command_states_its_contract_on_the_root_page() {
        let theme = Theme::fixed(80, Color::Off, false);
        let page = command(&theme).render_long_help().to_string();
        for label in [
            "Contract with no command:",
            "  Requires  ",
            "  Time      ",
            "  JSON      ",
            "Examples:",
            "Exit codes:",
        ] {
            assert!(page.contains(label), "{label}");
        }
        assert!(
            page.contains("Topics: agents, images, registry, output, errors."),
            "topic list"
        );
    }

    /// The root page lists every command and quotes the manual's command.
    #[test]
    fn test_the_root_scan_lists_every_command() {
        let theme = Theme::fixed(80, Color::Off, false);
        let page = command(&theme).render_help().to_string();
        assert!(
            page.contains("`ark-emulator help --all`"),
            "quoted help command"
        );
        for command in COMMANDS {
            assert!(page.contains(command), "{command}");
        }
    }

    /// Every way of asking for help puts a paragraph after the one-liner.
    #[test]
    fn test_paragraphs_follow_descriptions_on_the_right_pages() {
        /// Checks the preamble of a command and every command below it.
        fn check(command: &mut clap::Command, width: usize) {
            // Separate each preamble from usage and check the paragraph's
            // spacing independently of its wording
            let long = command.render_long_help().to_string();
            let short = command.render_help().to_string();
            let name = command.get_name();
            let (preamble, _) = long.split_once("\n\nUsage:").expect(name);
            let (description, paragraph) = preamble.split_once("\n\n").expect(name);
            assert!(!description.trim().is_empty(), "{name}");
            assert!(!paragraph.trim().is_empty(), "{name}");
            assert_eq!(paragraph, paragraph.trim(), "{name}");
            assert!(!paragraph.contains("\n\n"), "{name}");
            for line in paragraph.lines() {
                assert!(console::measure_text_width(line) <= width, "{name}: {line}");
            }
            assert_eq!(short, long, "{name}");

            // Visit the actual tree so a new command also needs a paragraph
            for child in command.get_subcommands_mut() {
                check(child, width);
            }
        }

        for width in [40, 80, 120] {
            let theme = Theme::fixed(width, Color::Off, false);
            check(&mut command(&theme), width.min(80));
        }
    }

    /// The manual puts the agents topic before commands, visits nested
    /// commands in tree order and ends with the other topics.
    #[test]
    fn test_the_manual_orders_commands_and_prints_every_topic() {
        let theme = Theme::fixed(80, Color::Off, false);
        let text = manual(&mut command(&theme), &theme);
        let separator = format!("\n\n{}\n\n", "-".repeat(80));
        let pages: Vec<_> = text.split(&separator).collect();

        // The order is part of the public manual, including the button group
        let paths = [
            "ark-emulator",
            "start",
            "list",
            "stop",
            "button",
            "button press",
            "button release",
            "wipe",
            "doctor",
            "completions",
            "help",
        ];
        assert_eq!(pages.len(), paths.len() + TOPICS.len(), "manual pages");
        for (index, path) in paths.into_iter().enumerate() {
            let page = pages[if index == 0 { 0 } else { index + 1 }];
            let usage = page
                .lines()
                .find_map(|line| line.strip_prefix("Usage: "))
                .unwrap();
            let path = if index == 0 {
                path.to_owned()
            } else {
                format!("ark-emulator {path}")
            };
            assert_eq!(
                usage.split(['[', '<']).next().unwrap().trim(),
                path,
                "{index}"
            );
        }

        // Each advertised topic has a nonempty page at its expected position
        for (name, index) in [
            ("agents", 1),
            ("images", 12),
            ("registry", 13),
            ("output", 14),
            ("errors", 15),
        ] {
            let page = markdown(&theme, topic(name).unwrap());
            assert!(!page.trim().is_empty(), "{name}");
            assert_eq!(pages[index], page, "{name}");
        }
        for name in TOPICS {
            let page = markdown(&theme, topic(name).expect(name));
            assert!(pages.contains(&page.as_str()), "{name}");
        }
    }

    /// The two help flags after a command are what an agent tries first, and
    /// they print the same whole contract.
    #[test]
    fn test_help_flags_after_a_command_print_its_page() {
        for arguments in [
            ["ark-emulator", "wipe", "-h"],
            ["ark-emulator", "wipe", "--help"],
        ] {
            let error = parser().try_get_matches_from(arguments).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "{arguments:?}"
            );
            let page = error.render().to_string();
            assert!(page.contains("Reset a stopped image"), "{arguments:?}");
            assert!(page.contains("Contract:"), "{arguments:?}");
        }
    }

    /// The options every command takes are listed on the root page and on no
    /// other, so the manual says each one once.
    #[test]
    fn test_global_options_are_listed_once() {
        let theme = Theme::fixed(80, Color::Off, false);
        let mut root = command(&theme);
        let mut pages = Vec::new();
        collect(&mut root, &mut pages);
        for (index, page) in pages.iter().enumerate() {
            let options = page.split_once("\nOptions:\n").expect("options section").1;
            let options = options.split("\n\n").next().unwrap();
            for flag in [
                "--json",
                "--no-input",
                "--timeout",
                "--quiet",
                "--verbose",
                "--log",
            ] {
                assert_eq!(
                    options.matches(flag).count(),
                    usize::from(index == 0),
                    "page {index}: {flag}"
                );
            }
            assert_eq!(
                options.matches("-h, --help").count(),
                1,
                "page {index}: help"
            );
        }
    }

    /// A global option means the same before, between and after commands.
    #[test]
    fn test_global_options_parse_at_every_level() {
        use clap::FromArgMatches as _;
        use serde_json::json;

        for (flags, key, expected) in [
            (vec!["--json"], "json", json!(true)),
            (vec!["--no-input"], "no_input", json!(true)),
            (vec!["--timeout", "17"], "timeout", json!(17)),
            (vec!["--quiet"], "quiet", json!(true)),
            (vec!["-q"], "quiet", json!(true)),
            (vec!["--verbose"], "verbose", json!(true)),
            (vec!["-v"], "verbose", json!(true)),
            (vec!["--log", "trace"], "log", json!("trace")),
        ] {
            for path in [vec!["list"], vec!["button", "release"], vec!["start"]] {
                for level in 0..=path.len() {
                    let mut arguments = vec!["ark-emulator"];
                    arguments.extend_from_slice(&path[..level]);
                    arguments.extend_from_slice(&flags);
                    arguments.extend_from_slice(&path[level..]);
                    let matches = command(&Theme::fixed(80, Color::Off, false))
                        .try_get_matches_from(&arguments)
                        .unwrap_or_else(|error| panic!("{arguments:?}: {error}"));
                    let cli = Cli::from_arg_matches(&matches).unwrap();
                    let actual = json!({
                        "json": cli.global.json, "no_input": cli.global.no_input,
                        "timeout": cli.global.timeout, "quiet": cli.global.quiet,
                        "verbose": cli.global.verbose,
                        "log": cli.global.log.map(|log| match log {
                            super::super::args::Log::Debug => "debug",
                            super::super::args::Log::Trace => "trace",
                        }),
                    });
                    let mut wanted = json!({
                        "json": false, "no_input": false, "timeout": 120,
                        "quiet": false, "verbose": false, "log": null,
                    });
                    wanted[key] = expected.clone();
                    assert_eq!(actual, wanted, "{arguments:?}");
                    assert!(
                        !cli.version && !cli.boot.named(),
                        "{arguments:?}: root options"
                    );
                }
            }
        }
    }

    /// The root page's sections, command groups and options come in reading
    /// order.
    #[test]
    fn test_root_sections_and_option_order() {
        let page = command(&Theme::fixed(80, Color::Off, false))
            .render_help()
            .to_string();
        let mut rest = page.as_str();
        for section in [
            "Usage: ark-emulator [OPTIONS]\n       ark-emulator <COMMAND>",
            "Commands:",
            "Options:",
            "Boot options, with no command or with start:",
            "Contract with no command:",
            "Examples:",
            "Exit codes:",
            "Output is formatted for reading; --json keeps complete, exact values.",
            "AI agents: read `ark-emulator help agents` first.",
            "Topics: agents, images, registry, output, errors.",
        ] {
            rest = rest
                .split_once(section)
                .unwrap_or_else(|| panic!("missing or misplaced {section}: {page}"))
                .1;
        }
        let commands = page
            .split_once("Commands:\n")
            .unwrap()
            .1
            .split_once("\nOptions:")
            .unwrap()
            .0;
        let names: Vec<_> = commands
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(
            names,
            [
                "start",
                "list",
                "stop",
                "button",
                "wipe",
                "doctor",
                "completions",
                "help"
            ],
            "root commands"
        );
        assert!(commands.contains("\n\n  doctor "), "command groups");

        // The shared options precede boot options, each shown once
        let mut options = page.split_once("Options:\n").unwrap().1;
        for flag in [
            "--json",
            "--no-input",
            "--timeout",
            "-q, --quiet",
            "-v, --verbose",
            "--log",
            "-V, --version",
            "-h, --help",
        ] {
            options = options
                .split_once(flag)
                .unwrap_or_else(|| panic!("misplaced {flag}: {page}"))
                .1;
        }
        let mut boot = page
            .split_once("Boot options, with no command or with start:\n")
            .unwrap()
            .1;
        for flag in [
            "--headless",
            "--image",
            "--env",
            "--memory",
            "--arch",
            "--kernel",
            "--initrd",
            "--port",
        ] {
            boot = boot
                .split_once(flag)
                .unwrap_or_else(|| panic!("misplaced {flag}: {page}"))
                .1;
        }
    }

    /// A contract names the document's keys, with no exit or approval row.
    #[test]
    fn test_json_contracts_name_every_result_key() {
        let mut root = command(&Theme::fixed(80, Color::Off, false));
        for (path, keys) in [
            (
                "start",
                "locator port image path created started environment name serial ready expires log",
            ),
            (
                "list",
                "emulators locator port image ready environment name serial expires log",
            ),
            ("stop", "stopped"),
            (
                "button press",
                "locator pressed cli_pressed changed release_after_seconds",
            ),
            (
                "button release",
                "locator pressed cli_pressed changed release_after_seconds",
            ),
            ("wipe", "path wiped"),
            ("doctor", "version checks name result detail hint"),
        ] {
            let mut command = &mut root;
            for name in path.split_whitespace() {
                command = command.find_subcommand_mut(name).unwrap();
            }
            let page = command.render_help().to_string();
            let json = page
                .split_once("  JSON      ")
                .unwrap()
                .1
                .split_once("\n\nExamples:")
                .unwrap()
                .0;
            for key in keys.split_whitespace() {
                assert!(
                    json.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                        .any(|word| word == key),
                    "{path}: {key}"
                );
            }
            for removed in ["Prints", "Exit", "Approval"] {
                assert!(
                    !page.contains(&format!("\n  {removed}")),
                    "{path}: {removed}"
                );
            }
        }
    }

    /// Headings, contract labels and the exit codes take their own roles.
    #[test]
    fn test_contract_styles_and_exit_column() {
        let theme = Theme::fixed(200, Color::True, true);
        let page = command(&theme).render_help().ansi().to_string();
        assert!(
            page.contains("\x1b[1mContract with no command:\x1b[0m"),
            "contract heading"
        );
        for label in ["Requires", "Time", "JSON"] {
            assert!(
                page.contains(&format!("\x1b[38;2;124;128;152m{label}\x1b[0m")),
                "{label}"
            );
        }
        for code in ["  0", "  1", "  2", "  3", "  7", "130", "143"] {
            assert!(
                page.contains(&format!("\n  \x1b[38;2;124;128;152m{code}\x1b[0m  ")),
                "exit {code}"
            );
        }
        let plain = console::strip_ansi_codes(&page);
        let exits = plain
            .split_once("Exit codes:\n")
            .unwrap()
            .1
            .split_once("\n\n")
            .unwrap()
            .0;
        assert_eq!(
            exits,
            "    0  done\n    1  a local file, a confirmation or a failed launch\n    2  usage\n    3  an emulator in the way or out of reach\n    7  a wait that ran out\n  130  Ctrl-C, or Ctrl-Break on Windows\n  143  SIGTERM, or a closed console",
            "exit meanings and alignment"
        );
        let bare = command(&Theme::fixed(200, Color::Off, false))
            .render_help()
            .to_string();
        assert_eq!(plain, bare, "color preserves layout");
        assert!(
            page.contains("`\x1b[1m\x1b[38;2;137;180;250mark-emulator help agents\x1b[0m`"),
            "quoted command"
        );
    }

    /// Command and topic pages fit both narrow and wide terminals.
    #[test]
    fn test_no_page_runs_past_the_width() {
        for (width, color) in [
            (40, Color::Off),
            (80, Color::Off),
            (200, Color::Off),
            (40, Color::True),
            (200, Color::True),
        ] {
            let theme = Theme::fixed(width, color, true);
            let mut root = command(&theme);
            let mut pages = Vec::new();
            collect(&mut root, &mut pages);
            pages.extend(TOPICS.map(|name| markdown(&theme, topic(name).unwrap())));
            for line in pages.join("\n").lines() {
                assert!(
                    console::measure_text_width(line) <= width.min(80),
                    "{width}: {line}"
                );
            }
        }
    }

    /// The command line tools, the example apps and the emulator are of no use
    /// on their own, so the manual has to lead to the other two.
    #[test]
    fn test_the_manual_names_the_cli_and_the_example_apps() {
        // Wrapped, so a phrase is looked for across line breaks.
        let theme = Theme::fixed(80, Color::Off, false);
        let manual = manual(&mut command(&theme), &theme)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for link in [
            "https://github.com/dark-bio/cli",
            "https://github.com/dark-bio/examples",
            "ark help agents",
        ] {
            assert!(manual.contains(link), "{link}");
        }
    }

    /// The errors topic names every error code the tool defines.
    #[test]
    fn test_the_errors_topic_names_every_error_code() {
        let errors = markdown(
            &Theme::fixed(80, Color::Off, false),
            topic("errors").unwrap(),
        );
        for code in Code::ALL {
            assert!(
                errors.contains(&format!("`{}`", code.name())),
                "{}",
                code.name()
            );
        }
    }

    /// A topic styles its headings, bullets and code, and keeps the backticks of
    /// a quoted command.
    #[test]
    fn test_a_topic_renders_its_headings_bullets_and_code() {
        let theme = Theme::fixed(80, Color::Basic, true);
        assert_eq!(
            markdown(
                &theme,
                "# Stopping\n\nRun `ark-emulator list` first.\n\n- One emulator at a time.\n\n    ark-emulator stop 18181\n\n  It stops at once.\n"
            ),
            "\x1b[1mStopping\x1b[0m\n\nRun `\x1b[1mark-emulator list\x1b[0m` first.\n\n  - One emulator at a time.\n\n    \x1b[1mark-emulator stop 18181\x1b[0m\n\n    It stops at once.",
            "headings, inline commands and examples keep their reading layout"
        );
    }

    /// A paragraph is wrapped as a whole, so where its source broke its lines
    /// does not show, and a bullet's continuation lines hang under its text.
    #[test]
    fn test_a_paragraph_is_joined_before_it_is_wrapped() {
        let theme = Theme::fixed(40, Color::Off, false);
        let text = "one two\nthree four five six seven eight nine ten\n\n- a bullet that runs\n  on and on and on\n";
        assert_eq!(
            markdown(&theme, text),
            "one two three four five six seven eight\nnine ten\n\n  - a bullet that runs on and on and on",
            "paragraph and bullet wrapping"
        );
    }
}
