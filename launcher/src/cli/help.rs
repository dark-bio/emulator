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

use super::args::Cli;
use super::style::{self, Color, Role, Theme};
use crate::error::{Code, Error};

/// The topics this build carries, in the order the manual prints them.
const TOPICS: [&str; 4] = ["agents", "images", "registry", "output"];

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
        return print(&manual(&mut root, &theme));
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
            .arg(help.help("Print help; `ark-emulator help --all` prints the manual"));
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
        .map(|line| style::wrap(&theme.inline(line), theme.width, 0))
        .collect::<Vec<_>>()
        .join("\n");

    // Clap selects the paragraph with the help action, and the template puts
    // it between the description and usage
    let paragraph = style::wrap(&theme.inline(contract.paragraph), theme.width, 0);
    let mut decorated = command.clone().before_long_help(paragraph.clone());
    if parent.is_empty() {
        let footer = footer(
            theme,
            contract.footer.as_ref().expect("the bare run's footer"),
        );
        decorated = decorated
            .before_help(paragraph)
            .after_help(format!("{closing}\n"))
            .after_long_help(format!("{footer}\n\n{closing}\n"));
    } else if let Some(fields) = &contract.footer {
        decorated = decorated.after_long_help(format!("{}\n", footer(theme, fields)));
    } else {
        decorated = decorated.after_long_help(format!(
            "Each subcommand has its own requirements and output.\nRead {} COMMAND --help for its contract.\n", path
        ));
    }
    *command = decorated;
    for child in command.get_subcommands_mut() {
        decorate(child, &path, theme, globals);
    }
}

/// Renders clap's compact option layout with the paragraph and footer selected
/// by the help action.
fn compact(command: &mut clap::Command, theme: &Theme) {
    // Render the scan alone so the template can insert the paragraph after
    // the description and the footer after the arguments and options
    let rendered = command
        .clone()
        .before_help(None)
        .before_long_help(None)
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
    let (description, scan) = scan.split_once("\n\n").unwrap_or((&scan, ""));
    *command = command.clone().help_template(format!(
        "{description}\n\n{{before-help}}{}{{after-help}}",
        scan.trim_end()
    ));

    // Subcommands use the same layout for their own paragraphs and footers
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

/// A command's paragraph and optional footer, selected by its path.
struct Contract {
    /// Command words after `ark-emulator`, or `ark-emulator` for the root.
    path: &'static str,
    /// Explanation between the one-line description and usage.
    paragraph: &'static str,
    /// Contract fields for the bare run and leaves, absent on group pages.
    footer: Option<ContractFooter>,
}

/// A command's prerequisites, duration, outcome and examples.
struct ContractFooter {
    /// Conditions the command needs before it can work.
    requires: &'static str,
    /// Expected duration and the waits the timeout bounds.
    time: &'static str,
    /// Result fields in reading order and any JSON differences.
    prints: &'static str,
    /// Exit classes the command can return.
    exit: &'static str,
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
            time: "runs in the foreground until the window closes, `ark-emulator stop`, \
                Ctrl-C or SIGTERM; the firmware boots in about 10s with hardware \
                acceleration, minutes without; --timeout plays no part",
            prints: "nothing on stdout, apart from the guest console of a run with \
                --kernel and --initrd; the launcher's log lines on stderr, as log events \
                under --json",
            exit: "0 stopped; 1 launch, launcher or QEMU failure; 2 usage; 130/143 \
                interrupted, device stopped",
            examples: "ark-emulator\nark-emulator --headless --image ~/arks/dev.ark",
        }),
    },
    Contract {
        path: "start",
        paragraph: "Boots an emulator in a process of its own and waits until its firmware \
            has booted. The emulator keeps running after start returns, until \
            `ark-emulator stop`, and the locator it prints is what `ark --device` \
            takes. Without --headless the emulator opens the device window. An image \
            that is already booted is waited for and reported with started false, \
            never booted twice. `ark-emulator help images` covers which image a start \
            boots.",
        footer: Some(ContractFooter {
            requires: "a free port from 18181 to 18280, or --port; a display unless \
                --headless; a source build needs --kernel and --initrd",
            time: "about 10s with hardware acceleration, minutes without; --timeout \
                bounds the wait for the firmware, after which the emulator keeps \
                booting",
            prints: "locator, image, created, started, environment, ready, expires; JSON \
                adds port, path, name, serial and log",
            exit: "0 ready; 1 image, firmware, launch or output failure; 2 usage; 3 \
                registry unreachable; 7 not ready in time, still booting; 130/143 \
                interrupted, still booting",
            examples: "ark-emulator start\nark-emulator start --headless --image ~/arks/dev.ark --json",
        }),
    },
    Contract {
        path: "list",
        paragraph: "Lists every emulator the registry on this computer reports, without \
            connecting to any. Environment, name, serial and expiry are what each \
            emulator's firmware reported, absent until it has and never verified. \
            `ark devices` shows the same emulators beside hardware Arks.",
        footer: Some(ContractFooter {
            requires: "nothing; no emulator running is an empty list",
            time: "immediate",
            prints: "locator, image, ready, environment, name, serial, expires; JSON adds \
                port and log",
            exit: "0 done; 1 output failure; 2 usage; 3 registry unreadable; 130/143 \
                interrupted",
            examples: "ark-emulator list\nark-emulator list --json",
        }),
    },
    Contract {
        path: "stop",
        paragraph: "Asks the emulator to shut down, the way closing its window does, and \
            waits until its launcher and QEMU have exited. The guest gets no warning, \
            like hardware that loses power, so stop an emulator only while no `ark` \
            command runs against it. With several running, name one as \
            `ark --device` names it, or pass --all, which stops them one at a time in \
            port order.",
        footer: Some(ContractFooter {
            requires: "a running emulator, named by locator, serial, name or image, or the \
                only one; --all needs none",
            time: "seconds; --timeout bounds the wait for the emulators to exit",
            prints: "stopped, the locators of the emulators that stopped, kept on a later \
                failure or timeout",
            exit: "0 done; 1 output failure; 2 usage; 3 selection, registry or control \
                failure; 7 shutdown not confirmed in time; 130/143 interrupted",
            examples: "ark-emulator stop\nark-emulator stop emulator:18181 --json",
        }),
    },
    Contract {
        path: "button",
        paragraph: "Presses an emulated Ark's reset button from the command line. The window \
            and the command line hold the button independently, and it is down while \
            either holds it. While `ark pair` waits for the button, a press confirms \
            the pairing at once. At any other time, a hold of 5s arms a factory \
            reset, which erases the device's data and pairing once the button is let \
            go, and keeps its enrollment.",
        footer: None,
    },
    Contract {
        path: "button press",
        paragraph: "Presses the button and holds it until `ark-emulator button release`, or \
            for --release-after seconds. --release-after 0 is one short press, released \
            before the command returns. The hold outlives the command and ends if the \
            emulated hardware disconnects. A press while the command line holds the \
            button sends no second press, and replaces or cancels the timer.",
        footer: Some(ContractFooter {
            requires: "a booted emulator, named by locator, serial, name or image, or the \
                only one",
            time: "under a second; --timeout bounds each reply; a positive \
                --release-after returns once the release is scheduled",
            prints: "locator; pressed, the button's state; CLI held, the command line's \
                hold, cli_pressed in JSON; changed, whether that hold or its timer \
                changed; release after, the accepted delay, release_after_seconds in JSON",
            exit: "0 delivered; 1 output failure; 2 usage; 3 selection, control or \
                hardware unavailable; 7 no reply in time, outcome unknown; 130/143 \
                interrupted, outcome unknown",
            examples: "ark-emulator button press --release-after 0\nark-emulator button press emulator:18181 --release-after 3 --json",
        }),
    },
    Contract {
        path: "button release",
        paragraph: "Releases the command line's hold on the button and cancels a pending \
            --release-after. The button stays down while the window holds it. \
            Releasing a button the command line does not hold succeeds without a \
            change, so it clears a press whose outcome is unknown.",
        footer: Some(ContractFooter {
            requires: "a booted emulator, named by locator, serial, name or image, or the \
                only one",
            time: "under a second; --timeout bounds each reply",
            prints: "locator; pressed; CLI held, cli_pressed in JSON; changed; release \
                after, release_after_seconds in JSON, which is null",
            exit: "0 released; 1 output failure; 2 usage; 3 selection, control or \
                hardware unavailable; 7 no reply in time, outcome unknown; 130/143 \
                interrupted, outcome unknown",
            examples: "ark-emulator button release\nark-emulator button release emulator:18181 --json",
        }),
    },
    Contract {
        path: "wipe",
        paragraph: "Replaces the image with an empty one at the same path, so its next boot \
            is a factory-fresh device that needs `ark enroll` and `ark pair` again. \
            Without PATH it wipes the image a start would boot. An image an emulator \
            holds is refused, so stop that emulator first.",
        footer: Some(ContractFooter {
            requires: "an image that exists and no emulator holds; confirmation at a \
                terminal, or --yes",
            time: "a second",
            prints: "path, wiped",
            exit: "0 done; 1 not confirmed, image missing or file error; 2 usage; 3 \
                image in use or registry unreachable; 130/143 interrupted",
            examples: "ark-emulator wipe\nark-emulator wipe ~/arks/dev.ark --yes",
        }),
    },
    Contract {
        path: "doctor",
        paragraph: "Runs every check in order and prints the whole list. It covers newer \
            releases, QEMU, the bundled firmware, hardware acceleration, the data \
            directory, the settings, the remembered image, the registry and a free \
            port. Each check is ok, warn, fail or skip, with a hint where something \
            needs action. It applies no fixes.",
        footer: Some(ContractFooter {
            requires: "nothing; a check that cannot run is skipped",
            time: "seconds; --timeout bounds each network reply of the release lookup",
            prints: "result, name, detail and hint for each check; JSON adds version, \
                firmware, qemu, accel, arch, data_dir, settings, disk, logs_dir and \
                registry",
            exit: "0 no check failed; 1 or 3, the class of the first failed check; 1 \
                output failure; 2 usage; 130/143 interrupted",
            examples: "ark-emulator doctor\nark-emulator doctor --json",
        }),
    },
    Contract {
        path: "completions",
        paragraph: "Prints the completion script for the shell on stdout. It never installs \
            the script.",
        footer: Some(ContractFooter {
            requires: "nothing",
            time: "immediate",
            prints: "the shell's completion script, even under --json",
            exit: "0 done; 1 output failure; 2 usage",
            examples: "ark-emulator completions zsh\nark-emulator completions bash",
        }),
    },
    Contract {
        path: "help",
        paragraph: "Prints the help page of a command, or a topic: agents, images, registry \
            or output. `ark-emulator help --all` prints the whole manual, starting with \
            the root page and the agents topic.",
        footer: Some(ContractFooter {
            requires: "nothing",
            time: "immediate",
            prints: "help text, even under --json",
            exit: "0 done; 1 output failure; 2 no such command or topic",
            examples: "ark-emulator help agents\nark-emulator help start",
        }),
    },
];

/// The contract as it prints: four labeled lines aligned on one column, then
/// the examples as bare commands, since a pasted prompt breaks in a shell.
fn footer(theme: &Theme, fields: &ContractFooter) -> String {
    let mut lines: Vec<String> = [
        ("Requires", fields.requires),
        ("Time", fields.time),
        ("Prints", fields.prints),
        ("Exit", fields.exit),
    ]
    .into_iter()
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
    lines.extend(fields.examples.lines().map(|line| {
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
    let rule = theme.paint(Role::Muted, "-".repeat(theme.width.min(80)));
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
        assert!(page.contains("Each subcommand has its own requirements and output"));
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
        assert!(page.contains("Topics: agents, images, registry, output."));
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

    /// Every long page places a nonempty paragraph after its description,
    /// while only the root keeps its paragraph on the short page.
    #[test]
    fn test_paragraphs_follow_descriptions_on_the_right_pages() {
        /// Checks the preamble of a command and every command below it.
        fn check(command: &mut clap::Command, root: bool, width: usize) {
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
            let (scan, _) = short.split_once("\n\nUsage:").expect(name);
            assert_eq!(scan, if root { preamble } else { description }, "{name}");

            // Visit the actual tree so a new command also needs a paragraph
            for child in command.get_subcommands_mut() {
                check(child, false, width);
            }
        }

        for width in [40, 80, 120] {
            let theme = Theme::fixed(width, Color::Off, false);
            check(&mut command(&theme), true, width);
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
        assert_eq!(pages.len(), paths.len() + TOPICS.len());
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
            assert_eq!(usage.split(['[', '<']).next().unwrap().trim(), path);
        }

        // Each advertised topic has a nonempty page at its expected position
        for (name, index) in [
            ("agents", 1),
            ("images", 12),
            ("registry", 13),
            ("output", 14),
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

    /// The output topic is checked against the codes the tool defines, not
    /// against a second list of them.
    #[test]
    fn test_the_output_topic_names_every_error_code() {
        let output = topic("output").unwrap();
        for code in Code::ALL {
            assert!(
                output.contains(&format!("- `{}` means", code.name())),
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
            "one two three four five six seven eight\nnine ten\n\n  - a bullet that runs on and on and on"
        );
    }
}
