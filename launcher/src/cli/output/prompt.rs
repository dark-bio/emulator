// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Reads the one key that answers a yes or no question, dropping what was
//! typed ahead.

use std::io;
use std::time::Duration;

use console::{Key, Term};

use crate::cli::clock::Clock;
use crate::error::{Code, Error};

/// How long after the question keys still count as typed ahead, since nobody
/// reads and answers a question within 100ms.
const TYPEAHEAD_WINDOW: Duration = Duration::from_millis(100);

/// Where the answer's keys come from, so tests type them without a terminal.
pub(super) trait Keys {
    /// Discards keys queued before the question appears.
    fn discard(&mut self) -> io::Result<()>;
    /// Reads one key, without echoing it or waiting for Enter.
    fn read(&mut self) -> io::Result<Key>;
}

/// The terminal's keys, even when stdout and stderr are redirected.
pub(super) struct Input {
    /// The reader, whose write side marks it as a terminal for console on
    /// Unix.
    terminal: Term,
}

impl Input {
    /// Opens the terminal that supplies stdin.
    pub(super) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        let terminal = {
            use std::os::fd::AsFd;
            let descriptor = io::stdin().as_fd().try_clone_to_owned()?;
            Term::read_write_pair(io::stdin(), std::fs::File::from(descriptor))
        };
        #[cfg(not(unix))]
        let terminal = if Term::stderr().is_term() {
            Term::stderr()
        } else {
            Term::stdout()
        };
        Ok(Self { terminal })
    }
}

impl Keys for Input {
    fn discard(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Sound since stdin stays open and tcflush receives no buffers
            if unsafe { libc::tcflush(io::stdin().as_raw_fd(), libc::TCIFLUSH) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Console::FlushConsoleInputBuffer;
            // Sound since stdin's console handle stays open for this call
            if unsafe { FlushConsoleInputBuffer(io::stdin().as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn read(&mut self) -> io::Result<Key> {
        #[cfg(windows)]
        if !self.terminal.is_term() {
            return redirected_key();
        }
        self.terminal.read_key_raw()
    }
}

/// Reads a key from the console when Windows has redirected stdout and
/// stderr.
#[cfg(windows)]
fn redirected_key() -> io::Result<Key> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        ENABLE_PROCESSED_INPUT, GetConsoleMode, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW,
        SetConsoleMode,
    };

    /// Restores the console's input mode after the key or a failed read.
    struct Mode {
        /// Stdin handle, which remains open while the guard exists.
        handle: HANDLE,
        /// The console's flags before this read.
        original: u32,
    }

    impl Drop for Mode {
        fn drop(&mut self) {
            // Sound since stdin remains open and original came from GetConsoleMode
            unsafe { SetConsoleMode(self.handle, self.original) };
        }
    }

    // Read Ctrl-C as a key so it follows the caller's cleanup path
    let handle = io::stdin().as_raw_handle();
    let mut original = 0;
    // Sound since the handle stays open and original is a writable mode word
    if unsafe { GetConsoleMode(handle, &mut original) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Sound since the handle stays open and these are console input flags
    if unsafe { SetConsoleMode(handle, original & !ENABLE_PROCESSED_INPUT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _mode = Mode { handle, original };

    loop {
        // Sound since every field of INPUT_RECORD accepts zero
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut count = 0;
        // Sound since the handle stays open and the buffer holds one record
        if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 || record.EventType != KEY_EVENT as u16 {
            continue;
        }
        // Sound since EventType identifies the key member of the union
        let event = unsafe { record.Event.KeyEvent };
        if event.bKeyDown == 0 {
            continue;
        }
        // Sound since ReadConsoleInputW fills the UnicodeChar member
        let character = unsafe { event.uChar.UnicodeChar };
        return Ok(match char::from_u32(u32::from(character)) {
            Some('\r') => Key::Enter,
            Some('\u{3}') => Key::CtrlC,
            Some(ch) => Key::Char(ch),
            None => Key::Unknown,
        });
    }
}

/// Shows the question and returns the first answer after the type-ahead
/// window.
///
/// `show` flushes the question, and the window starts then, on `clock`. Keys
/// read within the window are dropped. After it, `y` and `n` answer, Enter
/// takes `default`, and other keys are ignored. Ctrl-C returns the
/// interruption error for the caller's cleanup.
pub(super) fn confirm(
    input: &mut impl Keys,
    clock: &Clock,
    default: bool,
    show: impl FnOnce() -> Result<(), Error>,
    echo: impl FnOnce(bool) -> Result<(), Error>,
) -> Result<bool, Error> {
    // Flush what the platform can, then time type-ahead from the shown question
    input
        .discard()
        .map_err(|err| Error::io(format!("could not flush terminal input: {err}")))?;
    show()?;
    let ready = clock.now() + TYPEAHEAD_WINDOW;

    // Drop every early key before taking an answer, while Ctrl-C always
    // interrupts
    let answer = loop {
        let key = input
            .read()
            .map_err(|err| Error::io(format!("could not read the answer: {err}")))?;
        if key == Key::CtrlC {
            return Err(Error::new(Code::Interrupted, "command interrupted"));
        }
        if clock.now() < ready {
            continue;
        }
        match key {
            Key::Char('y' | 'Y') => break true,
            Key::Char('n' | 'N') => break false,
            Key::Enter => break default,
            _ => {}
        }
    };
    echo(answer)?;
    Ok(answer)
}

/// Tests of the answer keys, typed without a terminal.
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Write as _;
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::super::narration::tests::{Capture, Fixture};
    use super::{self as prompt, *};
    use crate::cli::style::Color;

    /// Keys on a schedule, whose flush drops the ones queued before the
    /// question.
    struct Keys {
        /// The clock each read advances.
        clock: Clock,
        /// Keys already queued when the input is flushed.
        queued: VecDeque<Key>,
        /// Keys arriving later, each after a delay from the read before it.
        arrivals: VecDeque<(u64, Key)>,
        /// Whether the flush happened before the first read.
        discarded: bool,
    }

    impl prompt::Keys for Keys {
        fn discard(&mut self) -> io::Result<()> {
            self.queued.clear();
            self.discarded = true;
            Ok(())
        }

        fn read(&mut self) -> io::Result<Key> {
            assert!(self.discarded, "input is flushed before reading");
            if let Some(key) = self.queued.pop_front() {
                return Ok(key);
            }
            let (millis, key) = self.arrivals.pop_front().expect("test supplies an answer");
            self.clock.advance(Duration::from_millis(millis));
            Ok(key)
        }
    }

    /// Each answer key echoes its word, and other keys and type-ahead are
    /// ignored.
    #[test]
    fn test_prompt_keys_and_typeahead() {
        for (case, keys, expected) in [
            ("yes", vec![(100, Key::Char('y'))], true),
            ("uppercase yes", vec![(100, Key::Char('Y'))], true),
            ("no", vec![(100, Key::Char('n'))], false),
            ("uppercase no", vec![(100, Key::Char('N'))], false),
            ("default", vec![(100, Key::Enter)], false),
            (
                "other keys",
                vec![
                    (100, Key::ArrowLeft),
                    (0, Key::Char('x')),
                    (0, Key::Char('y')),
                ],
                true,
            ),
            (
                "type-ahead",
                vec![
                    (0, Key::Char('y')),
                    (25, Key::Char('n')),
                    (25, Key::Enter),
                    (49, Key::Char('Y')),
                    (1, Key::Char('n')),
                ],
                false,
            ),
        ] {
            for interactive in [false, true] {
                let fixture = Fixture::new(interactive, false, false);
                let mut input = Keys {
                    clock: fixture.clock.clone(),
                    queued: vec![Key::Char('y'), Key::Enter].into(),
                    arrivals: keys.clone().into(),
                    discarded: false,
                };
                let answer = fixture
                    .output
                    .confirm_with(&mut input, "Wipe dev.ark?")
                    .unwrap();
                assert_eq!(answer, expected, "{case}, interactive={interactive}");
                assert_eq!(
                    fixture.stderr.text(),
                    format!(
                        "? Wipe dev.ark? (y/N) {}\n",
                        if expected { "yes" } else { "no" }
                    ),
                    "{case}, interactive={interactive}"
                );
                assert!(
                    input.arrivals.is_empty(),
                    "{case}, early keys cannot answer"
                );
            }
        }
    }

    /// Ctrl-C interrupts the question inside the type-ahead window and after it.
    #[test]
    fn test_prompt_interrupt_returns_error() {
        for millis in [0, 99, 100, 250] {
            let fixture = Fixture::new(false, false, false);
            let mut input = Keys {
                clock: fixture.clock.clone(),
                queued: VecDeque::new(),
                arrivals: vec![(millis, Key::CtrlC)].into(),
                discarded: false,
            };
            let error = fixture
                .output
                .confirm_with(&mut input, "Wipe dev.ark?")
                .unwrap_err();
            assert_eq!(error.exit(), 130, "arrival at {millis}ms");
            fixture.output.error(&error);
            assert_eq!(
                fixture.stderr.text(),
                "? Wipe dev.ark? (y/N) \nerror[interrupted]: command interrupted\n",
                "arrival at {millis}ms, caller receives the error"
            );
        }
    }

    /// Enter takes a yes default, and the window starts when the question shows.
    #[test]
    fn test_prompt_window_follows_question_flush() {
        let clock = Clock::test();
        let mut input = Keys {
            clock: clock.clone(),
            queued: VecDeque::new(),
            arrivals: vec![(99, Key::Char('n')), (1, Key::Enter)].into(),
            discarded: false,
        };
        let mut answer = Capture::default();
        let accepted = prompt::confirm(
            &mut input,
            &clock,
            true,
            || {
                clock.advance(Duration::from_secs(1));
                Ok(())
            },
            |yes| {
                writeln!(answer, "{}", if yes { "yes" } else { "no" }).unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert!(accepted, "Enter takes the true default");
        assert_eq!(answer.text(), "yes\n", "the early no did not answer");
    }

    /// On a terminal the question mark and the choices take color, and the
    /// answer prints plain.
    #[test]
    fn test_prompt_palette() {
        let mut fixture = Fixture::new(true, false, false);
        Arc::get_mut(&mut fixture.output.0).unwrap().err.color = Color::True;
        let mut input = Keys {
            clock: fixture.clock.clone(),
            queued: VecDeque::new(),
            arrivals: vec![(100, Key::Char('y'))].into(),
            discarded: false,
        };
        fixture
            .output
            .confirm_with(&mut input, "Wipe dev.ark?")
            .unwrap();
        assert_eq!(
            fixture.stderr.text(),
            "\x1b[1m\x1b[38;2;232;162;74m?\x1b[0m Wipe dev.ark? \x1b[38;2;124;128;152m(y/N)\x1b[0m yes\n",
            "prompt palette and echoed answer"
        );
    }

    /// Under --json and --no-input the question is the error naming its flag.
    #[test]
    fn test_confirmation_requires_its_flag_without_input() {
        for json in [false, true] {
            let mut fixture = Fixture::new(true, json, false);
            Arc::get_mut(&mut fixture.output.0).unwrap().no_input = !json;
            let error = fixture
                .output
                .confirm("Wipe dev.ark?", "wipe needs confirmation", "--yes")
                .unwrap_err();
            assert_eq!(error.code, Code::ConfirmationRequired, "json={json}");
            fixture.output.error(&error);
            if json {
                assert_eq!(
                    serde_json::from_str::<Value>(&fixture.stderr.text()).unwrap(),
                    json!({"event":"error", "error":{"code":"confirmation-required", "message":"wipe needs confirmation", "hints":["pass --yes to confirm without being asked"]}}),
                    "JSON confirmation error"
                );
            } else {
                assert_eq!(
                    fixture.stderr.text(),
                    "error[confirmation-required]: wipe needs confirmation\nhint: pass --yes to confirm without being asked\n",
                    "no-input confirmation error"
                );
            }
        }
    }
}
