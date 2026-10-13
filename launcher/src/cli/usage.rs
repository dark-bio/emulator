// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Turns clap's parse errors into usage errors, keeping near matches as hints.

use std::ffi::OsString;

use clap::error::{ContextKind, ContextValue, ErrorKind};

use crate::error::{Code, Error};

/// Builds a usage error in the tool's words, pointing at the command's help
/// when clap has no near match.
pub(crate) fn from_clap(
    error: &clap::Error,
    root: &clap::Command,
    arguments: &[OsString],
) -> Error {
    // Read the rejected command, option or value from clap's context
    let path = command_path(root, arguments);
    let argument = context(error, ContextKind::InvalidArg);
    let value = context(error, ContextKind::InvalidValue);
    let option = argument.split_whitespace().next().unwrap_or("argument");
    if error.kind() == ErrorKind::UnknownArgument
        && option.strip_prefix("--").is_some_and(|name| {
            root.get_arguments().any(|arg| {
                arg.get_long() == Some(name.split('=').next().unwrap_or(name))
                    && !arg.is_global_set()
                    && arg.get_id() != "version"
                    && arg.get_id() != "help"
            })
        })
    {
        return Error::new(Code::Usage, super::args::BOOT_USAGE).hint("see `ark-emulator --help`");
    }
    let message = match error.kind() {
        ErrorKind::InvalidSubcommand => format!(
            "unknown command '{}'",
            context(error, ContextKind::InvalidSubcommand)
        ),
        ErrorKind::UnknownArgument if option.starts_with('-') => {
            format!("unknown option '{argument}'")
        }
        ErrorKind::UnknownArgument => format!("unexpected argument '{argument}'"),
        ErrorKind::MissingRequiredArgument => match error.get(ContextKind::InvalidArg) {
            Some(ContextValue::Strings(arguments)) => arguments
                .iter()
                .map(|argument| missing_argument(argument))
                .collect::<Vec<_>>()
                .join("; "),
            _ => missing_argument(&argument),
        },
        ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            "missing a command".into()
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation if value.is_empty() => {
            format!("missing a value for {option}")
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => match option {
            "--env" => format!("unknown environment '{value}'"),
            "--arch" => format!("unknown architecture '{value}'"),
            "<SHELL>" => format!("unknown shell '{value}'"),
            "--timeout" => format!(
                "timeout must be a number of seconds from 1 to {}",
                super::args::MAX_TIMEOUT
            ),
            "--port" => "port must be a number from 1 to 65535".into(),
            "--memory" => "memory must be a whole number of MiB from 0 to 4294967295".into(),
            "--release-after" => "release delay must be whole seconds from 0 to 4294967295".into(),
            _ => format!("unknown value '{value}' for {option}"),
        },
        ErrorKind::ArgumentConflict => format!(
            "{argument} cannot be combined with {}",
            context(error, ContextKind::PriorArg)
        ),
        _ => "invalid arguments".into(),
    };

    // Keep clap's suggestions, qualifying a command with its parent path
    let mut result = Error::new(Code::Usage, message);
    for (kind, label) in [
        (ContextKind::SuggestedSubcommand, "command"),
        (ContextKind::SuggestedArg, "option"),
        (ContextKind::SuggestedValue, "value"),
    ] {
        for suggested in suggestions(error, kind) {
            let suggested = if kind == ContextKind::SuggestedSubcommand {
                format!("{path} {suggested}")
            } else {
                suggested
            };
            result = result.hint(format!("a similar {label} exists: `{suggested}`"));
        }
    }
    if result.hints.is_empty() {
        result = result.hint(format!("see `{path} --help`"));
    }
    result
}

/// Names a missing argument in words instead of its placeholder.
fn missing_argument(argument: &str) -> String {
    match argument {
        "<SHELL>" => "missing the shell for completions".into(),
        "--kernel <PATH>" => "missing the kernel image (--kernel)".into(),
        "--initrd <PATH>" => "missing the initramfs (--initrd)".into(),
        _ => format!("missing {argument}"),
    }
}

/// Reads one context value as plain text.
fn context(error: &clap::Error, kind: ContextKind) -> String {
    error.get(kind).map(ToString::to_string).unwrap_or_default()
}

/// Reads the near matches clap suggests, in its order.
pub(crate) fn suggestions(error: &clap::Error, kind: ContextKind) -> Vec<String> {
    match error.get(kind) {
        Some(ContextValue::String(value)) => vec![value.clone()],
        Some(ContextValue::Strings(values)) => values.clone(),
        _ => Vec::new(),
    }
}

/// Finds a near match among help topics with clap's value matcher.
pub(crate) fn similar(
    value: &str,
    candidates: impl IntoIterator<Item = &'static str>,
) -> Option<String> {
    use clap::builder::TypedValueParser;
    let parser = clap::builder::PossibleValuesParser::new(candidates);
    let error = parser
        .parse_ref(
            &clap::Command::new("ark-emulator"),
            None,
            std::ffi::OsStr::new(value),
        )
        .err()?;
    suggestions(&error, ContextKind::SuggestedValue).pop()
}

/// Finds the command path, skipping options and their values.
fn command_path(root: &clap::Command, arguments: &[OsString]) -> String {
    let mut command = root;
    let mut path = String::from("ark-emulator");
    let mut arguments = arguments.iter().skip(1);
    while let Some(value) = arguments.next() {
        let value = value.to_string_lossy();
        if value == "--" {
            break;
        }
        if let Some(long) = value.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            let Some(option) = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some(name))
            else {
                break;
            };
            if option.get_action().takes_values() && !attached {
                arguments.next();
            }
        } else if let Some(short) = value.strip_prefix('-') {
            for (index, name) in short.char_indices() {
                let Some(option) = command
                    .get_arguments()
                    .find(|arg| arg.get_short() == Some(name))
                else {
                    return path;
                };
                if option.get_action().takes_values() {
                    if index + name.len_utf8() == short.len() {
                        arguments.next();
                    }
                    break;
                }
            }
        } else if let Some(child) = command.find_subcommand(value.as_ref()) {
            path.push(' ');
            path.push_str(child.get_name());
            command = child;
        } else {
            break;
        }
    }
    path
}
