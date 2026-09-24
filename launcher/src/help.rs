// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The manual, which is the only thing an agent plans from.
//!
//! Every command carries its whole contract: what has to be true first, how
//! long to expect it to take, what it prints and what it can exit with, then
//! two examples. `-h` is the scan of that page, `--help` and
//! `ark-emulator help <command>` are the contract, and the topics below are
//! the reference a reader arrives at from either.
//!
//! The tree built here is also the one a run is parsed with, so the pages and
//! the arguments cannot drift apart. Help prints text on stdout whatever the
//! run asked for, since a caller that wanted JSON still has to read this.

use clap::CommandFactory as _;

use crate::args::Cli;
use crate::error::{Code, Error};
use crate::style::{self, Color, Role, Theme};

/// The topics this build carries, in the order the manual prints them.
const TOPICS: [&str; 4] = ["agents", "output", "images", "registry"];

/// The column the contract's values start in, one past the widest label and
/// its colon.
const COLUMN: usize = 10;

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
    let globals: Vec<clap::Arg> = command
        .get_arguments()
        .filter(|argument| argument.is_global_set())
        .cloned()
        .collect();
    decorate(&mut command, "", theme, &globals);
    command.build();
    compact(&mut command, theme);
    command
}

/// The tree a run is parsed with. It is the help tree, so that `-h` and
/// `--help` after a command print the page `help <command>` prints.
pub(crate) fn parser() -> clap::Command {
    command(&theme())
}

/// Print a command's page, a topic, or the whole manual. `long` is the
/// contract rather than the scan, which is what `--help` asks for and `-h`
/// does not.
pub(crate) fn run(path: &[String], all: bool, long: bool) -> Result<(), Error> {
    let theme = theme();
    let mut root = command(&theme);
    if all {
        let mut pages = Vec::new();
        collect(&mut root, &mut pages);
        pages.extend(TOPICS.map(|name| markdown(&theme, topic(name).expect("a listed topic"))));
        let rule = theme.paint(Role::Muted, "-".repeat(theme.width.min(80)));
        return print(&pages.join(&format!("\n\n{rule}\n\n")));
    }
    if let [name] = path
        && let Some(text) = topic(name)
    {
        return print(&markdown(&theme, text));
    }
    let mut command = &mut root;
    for name in path {
        command = command.find_subcommand_mut(name).ok_or_else(|| {
            Error::new(Code::Usage, format!("no command or topic named {name:?}"))
                .hint("`ark-emulator help` lists the commands and the topics")
        })?;
    }
    let printed = if long {
        command.print_long_help()
    } else {
        command.print_help()
    };
    printed.map_err(|err| Error::io(format!("could not print the help: {err}")))
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
///
/// The options every command takes are listed on the root page for all of
/// them, so each command below the root gets its own hidden copy of them
/// before the build; clap then leaves the copy alone instead of propagating
/// the root's visible one.
fn decorate(command: &mut clap::Command, parent: &str, theme: &Theme, globals: &[clap::Arg]) {
    *command = command
        .clone()
        .styles(theme.clap())
        .term_width(theme.width)
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
        // The boot options belong to the bare run and to `start`, and clap
        // would otherwise offer them on one line together with a command.
        *command = command
            .clone()
            .override_usage("ark-emulator [OPTIONS]\n       ark-emulator <COMMAND>")
            .arg(help.help("Print help; `help --all` prints the manual"));
    } else {
        for global in globals {
            *command = command.clone().arg(global.clone().hide(true));
        }
        *command = command
            .clone()
            .arg(help.help("Print help (see more with '--help')"));
    }
    let path = if parent.is_empty() {
        command.get_name().to_owned()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let contract = contract(path.strip_prefix("ark-emulator").unwrap_or(&path).trim());
    let footer = footer(theme, &contract);
    let closing = "Output is formatted for reading; --json keeps complete, exact values.
AI agents: read `ark-emulator help agents` first.
Topics: agents, output, images, registry.";
    let closing = closing
        .lines()
        .map(|line| style::wrap(&theme.inline(line), theme.width, 0))
        .collect::<Vec<_>>()
        .join("\n");

    let mut decorated = command.clone();
    if parent.is_empty() {
        decorated = decorated
            .after_help(format!("{closing}\n"))
            .after_long_help(format!("{footer}\n\n{closing}\n"));
    } else if command.has_subcommands() {
        decorated = decorated.after_long_help(format!(
            "Each subcommand has its own contract.\nRead `{} <command> --help` for that command's behavior.\n", path
        ));
    } else {
        decorated = decorated.after_long_help(format!("{footer}\n"));
    }
    *command = decorated;
    for child in command.get_subcommands_mut() {
        decorate(child, &path, theme, globals);
    }
}

/// Clap lays a long help's options out over two lines each. Render the short
/// layout once and keep it, so the long page differs only by its footer.
fn compact(command: &mut clap::Command, theme: &Theme) {
    let rendered = command
        .clone()
        .after_help(None)
        .after_long_help(None)
        .render_help()
        .ansi()
        .to_string();
    let scan = rendered
        .lines()
        .map(|line| style::wrap(&theme.inline(line), theme.width, hanging(line)))
        .collect::<Vec<_>>()
        .join("\n");
    *command = command
        .clone()
        .help_template(format!("{}{{after-help}}", scan.trim_end()));
    for child in command.get_subcommands_mut() {
        compact(child, theme);
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

/// What a command promises, by the words it is typed with. The bare run's
/// contract sits under the empty path, since the root is a command too.
fn contract(path: &str) -> [(&'static str, &'static str); 5] {
    let (requires, time, prints, exits, examples) = match path {
        "start" => (
            "a free loopback port from 18181 up, or --port; a source build needs --kernel and --initrd",
            "about 10 s with hardware acceleration, minutes without; --timeout bounds the whole wait; a separate process opens the device window unless --headless is set; an already running image keeps its current mode",
            "locator, image, created, started, environment, ready, expires; JSON adds port, path, name, serial and log",
            "0 ready; 1 image, firmware or QEMU problem; 2 usage; 3 registry unreachable; 7 not ready in time, still booting; 130/143 interrupted, still booting",
            "ark-emulator start\nark-emulator start --headless --image ~/arks/dev.ark --json",
        ),
        "list" => (
            "nothing; no registry means no emulators",
            "immediate",
            "locator, image, ready, environment, name, serial, expires; JSON adds port and log",
            "0 done; 2 usage; 3 registry answered but could not be read; 130/143 interrupted",
            "ark-emulator list\nark-emulator list --json",
        ),
        "stop" => (
            "a running emulator, named as ark -d names it, or the only one running; --all stops every one",
            "about a second to deliver, then seconds for the device to go; --timeout bounds the whole wait",
            "stopped, the locators that went, which is the partial result on a timeout",
            "0 done; 2 usage; 3 none matches, or several do; 7 one did not go in time; 130/143 interrupted",
            "ark-emulator stop\nark-emulator stop emulator:18181 --json",
        ),
        "button press" => (
            "a running emulator with connected hardware and button control; locator, serial, name or image selects it, or the only one running",
            "normally under a second; --timeout bounds each reply wait; success means written to hardware or already held; the CLI hold persists until button release, disconnect or shutdown",
            "locator, pressed (physical state), cli_pressed (CLI hold), changed (whether this command changed the CLI hold)",
            "0 held; 1 output failure; 2 usage; 3 selection, registry, control or hardware unavailable; 7 reply timed out, state unknown; 130/143 interrupted, state unknown; no automatic retry",
            "ark-emulator button press\nark-emulator button press emulator:18181 --json",
        ),
        "button release" => (
            "a running emulator with connected hardware and button control; locator, serial, name or image selects it, or the only one running",
            "normally under a second; --timeout bounds each reply wait; success means written to hardware or already released; the button remains pressed while the window holds it",
            "locator, pressed (physical state), cli_pressed (CLI hold), changed (whether this command changed the CLI hold)",
            "0 CLI hold released; 1 output failure; 2 usage; 3 selection, registry, control or hardware unavailable; 7 reply timed out, state unknown; 130/143 interrupted, state unknown; no automatic retry",
            "ark-emulator button release\nark-emulator button release emulator:18181 --json",
        ),
        "wipe" => (
            "an existing image no emulator holds; confirmation at a terminal, or --yes",
            "a second",
            "path, wiped",
            "0 done; 1 confirmation or file problem; 2 usage; 3 held by an emulator; 130/143 interrupted",
            "ark-emulator wipe\nark-emulator wipe ~/arks/dev.ark --yes",
        ),
        "doctor" => (
            "nothing; a check that cannot run is skipped",
            "seconds; the release lookup waits up to --timeout for each network reply",
            "checks: result (ok, warn, fail or skip), name, detail, hint; JSON adds version, firmware, qemu, accel, arch, data_dir, settings, disk, logs_dir, registry",
            "0 done; 1 a check failed on this computer; 2 usage; 3 the registry could not be read; 130/143 interrupted",
            "ark-emulator doctor\nark-emulator doctor --json",
        ),
        "completions" => (
            "nothing",
            "immediate",
            "the shell's completion script on stdout, even under --json",
            "0 done; 2 usage",
            "ark-emulator completions zsh\nark-emulator completions bash",
        ),
        "help" => (
            "nothing",
            "immediate",
            "help text on stdout, even under --json",
            "0 done; 2 no such command or topic",
            "ark-emulator help agents\nark-emulator help start",
        ),
        _ => (
            "nothing on a packaged build; a source build needs --kernel and --initrd and a QEMU on PATH",
            "runs in the foreground; --headless opens no window and never prompts; the device accepts clients after about 10 s with hardware acceleration, minutes without; --timeout does not limit this run",
            "nothing on stdout; the launcher's log on stderr; a source build adds the guest console on stdout",
            "0 stopped; 1 startup or QEMU failure; 2 usage; 130/143 interrupted, device stopped",
            "ark-emulator\nark-emulator --headless --image ~/arks/dev.ark",
        ),
    };
    [
        ("Requires", requires),
        ("Time", time),
        ("Prints", prints),
        ("Exit", exits),
        ("Examples", examples),
    ]
}

/// The contract as it prints: four labeled lines aligned on one column, then
/// the examples as bare commands, since a pasted prompt breaks in a shell.
fn footer(theme: &Theme, contract: &[(&str, &str); 5]) -> String {
    let mut lines: Vec<String> = contract[..4]
        .iter()
        .map(|(label, text)| {
            let label = format!("{label}:");
            style::wrap(
                &format!(
                    "{}{}{}",
                    theme.paint(Role::Muted, &label),
                    " ".repeat(COLUMN - label.len()),
                    theme.inline(text)
                ),
                theme.width,
                COLUMN,
            )
        })
        .collect();
    lines.push(format!("\n{}", theme.paint(Role::Heading, "Examples:")));
    lines.extend(contract[4].1.lines().map(|line| {
        style::wrap(
            &format!("  {}", theme.paint(Role::Accent, line)),
            theme.width,
            2,
        )
    }));
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
            lines.push(style::wrap(&theme.inline(&text), theme.width, hanging));
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
            lines.push(style::wrap(&code, theme.width, 2));
            continue;
        }
        if indent >= 4 && !content.is_empty() {
            flush(&mut paragraph, &mut lines);
            let base = *block.get_or_insert(indent);
            let code = &line[base.min(indent)..];
            let pad = if bullet { 4 } else { 2 };
            let code = format!("{}{}", " ".repeat(pad), theme.paint(Role::Accent, code));
            lines.push(style::wrap(&code, theme.width, pad));
            continue;
        }
        block = None;
        if content.is_empty() {
            flush(&mut paragraph, &mut lines);
            lines.push(String::new());
        } else if line.starts_with('#') {
            flush(&mut paragraph, &mut lines);
            bullet = false;
            lines.push(theme.paint(Role::Heading, content.trim_start_matches('#').trim_start()));
        } else if line.starts_with("- ") {
            flush(&mut paragraph, &mut lines);
            bullet = true;
            paragraph = Some((format!("  {line}"), 4));
        } else if let Some((text, _)) = &mut paragraph {
            text.push(' ');
            text.push_str(content);
        } else {
            bullet = false;
            paragraph = Some((line.to_owned(), 0));
        }
    }
    flush(&mut paragraph, &mut lines);
    lines.join("\n").trim_end().to_owned()
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
        "output" => include_str!("help/output.md"),
        "images" => include_str!("help/images.md"),
        "registry" => include_str!("help/registry.md"),
        _ => return None,
    })
}

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

    /// The whole manual, as `help --all` hands it to a reader or a model.
    fn manual() -> String {
        let theme = Theme::fixed(80, Color::Off, false);
        let mut root = command(&theme);
        let mut pages = Vec::new();
        collect(&mut root, &mut pages);
        pages.extend(TOPICS.map(|name| markdown(&theme, topic(name).unwrap())));
        pages.join("\n\n")
    }

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
            for label in ["Requires:", "Time:", "Prints:", "Exit:", "Examples:"] {
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
        assert!(!page.contains("Requires:"));
        assert!(page.contains("Each subcommand has its own contract"));
        for name in ["press", "release"] {
            let page = group
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            for label in ["Requires:", "Time:", "Prints:", "Exit:", "Examples:"] {
                assert!(page.contains(label), "{name}: {label}");
            }
            assert_eq!(
                page.matches(&format!("\n  ark-emulator button {name}"))
                    .count(),
                2
            );
            assert!(!page.contains("--no-input"));
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

    #[test]
    fn test_the_bare_run_states_its_contract_on_the_root_page() {
        let theme = Theme::fixed(80, Color::Off, false);
        let page = command(&theme).render_long_help().to_string();
        for label in ["Requires:", "Time:", "Prints:", "Exit:", "Examples:"] {
            assert!(page.contains(label), "{label}");
        }
        assert!(page.contains("Topics: agents, output, images, registry."));
    }

    #[test]
    fn test_the_root_scan_fits_on_a_screen() {
        let theme = Theme::fixed(80, Color::Off, false);
        let page = command(&theme).render_help().to_string();
        assert!(page.lines().count() <= 42, "{}", page.lines().count());
        for command in COMMANDS {
            assert!(page.contains(command), "{command}");
        }
    }

    /// The two help flags after a command are what an agent tries first, and
    /// they print the same page `help <command>` does, the scan for `-h` and
    /// the contract for `--help`.
    #[test]
    fn test_help_flags_after_a_command_print_its_page() {
        for (arguments, contract) in [
            (["ark-emulator", "wipe", "-h"], false),
            (["ark-emulator", "wipe", "--help"], true),
        ] {
            let error = parser().try_get_matches_from(arguments).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "{arguments:?}"
            );
            let page = error.render().to_string();
            assert!(page.contains("Reset a stopped image"), "{arguments:?}");
            assert_eq!(page.contains("Requires:"), contract, "{arguments:?}");
        }
    }

    /// The options every command takes are listed on the root page and on no
    /// other, so the manual says each one once.
    #[test]
    fn test_global_options_are_listed_once() {
        let theme = Theme::fixed(80, Color::Off, false);
        let mut root = command(&theme);
        assert!(root.render_help().to_string().contains("--no-input"));
        let page = root
            .find_subcommand_mut("list")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(!page.contains("--no-input"), "{page}");
    }

    #[test]
    fn test_no_page_runs_past_the_width() {
        for width in [80, 120] {
            let theme = Theme::fixed(width, Color::Off, false);
            let mut root = command(&theme);
            let mut pages = Vec::new();
            collect(&mut root, &mut pages);
            pages.extend(TOPICS.map(|name| markdown(&theme, topic(name).unwrap())));
            for line in pages.join("\n").lines() {
                assert!(console::measure_text_width(line) <= width, "{line}");
            }
        }
    }

    /// The command line tools, the example apps and the emulator are of no use
    /// on their own, so the manual has to lead to the other two.
    #[test]
    fn test_the_manual_names_the_cli_and_the_example_apps() {
        // Wrapped, so a phrase is looked for across line breaks.
        let manual = manual().split_whitespace().collect::<Vec<_>>().join(" ");
        for link in [
            "https://github.com/dark-bio/cli",
            "https://github.com/dark-bio/examples",
            "ark help agents",
        ] {
            assert!(manual.contains(link), "{link}");
        }
    }

    /// The output topic is checked against the codes the tool defines, not
    /// against a second list of them.
    #[test]
    fn test_the_output_topic_names_every_error_code() {
        let output = topic("output").unwrap();
        for code in Code::ALL {
            assert!(
                output.contains(&format!("- {}:", code.name())),
                "{}",
                code.name()
            );
        }
    }

    #[test]
    fn test_a_topic_renders_its_headings_bullets_and_code() {
        let theme = Theme::fixed(80, Color::Basic, true);
        assert_eq!(
            markdown(
                &theme,
                "# Stopping\n\nRun `ark-emulator list` first.\n\n- One emulator at a time.\n\n    ark-emulator stop 18181\n"
            ),
            "\x1b[1mStopping\x1b[0m\n\nRun \x1b[1mark-emulator list\x1b[0m first.\n\n  - One emulator at a time.\n\n    \x1b[1mark-emulator stop 18181\x1b[0m"
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
            "one two three four five six seven eight\nnine ten\n\n  - a bullet that runs on and on and on"
        );
    }
}
