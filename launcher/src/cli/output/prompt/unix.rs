// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Reads the answer's key and watches for signals together, so the terminal's
//! mode comes back before either ends the question.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use console::Key;
use signal_hook::iterator::{backend::SignalDelivery, exfiltrator::SignalOnly};

/// Stdin's prompt reader and the modes restored when the question ends.
pub(in crate::cli::output) struct Input {
    /// The terminal's flags before the question.
    original: libc::termios,
    /// Whether this reader has switched stdin to raw mode.
    raw: bool,
    /// The signal actions in place before the question, restored after it.
    actions: [(i32, libc::sigaction); 2],
    /// The signals watched while the question holds the input.
    signals: Option<SignalDelivery<UnixStream, SignalOnly>>,
}

impl Input {
    /// Saves stdin's mode and the current signal actions.
    pub(in crate::cli::output) fn new() -> io::Result<Self> {
        // Save the terminal and signal state before the question prints
        // Sound since these C structs accept zero and the pointers are writable
        let mut original = unsafe { std::mem::zeroed() };
        // Sound since stdin stays open and original holds one termios value
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Sound since sigaction writes each initialized struct before it is used
        let mut actions = unsafe {
            [
                (libc::SIGINT, std::mem::zeroed()),
                (libc::SIGTERM, std::mem::zeroed()),
            ]
        };
        for (signal, action) in &mut actions {
            // Sound since a null action queries the signal into writable storage
            if unsafe { libc::sigaction(*signal, std::ptr::null(), action) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            original,
            raw: false,
            actions,
            signals: None,
        })
    }

    /// Reads an answer key, skipping escape sequences and other keys.
    pub(super) fn read_key(&mut self) -> io::Result<Key> {
        if !self.raw {
            // Watch signals only while waiting for a key
            let (read, write) = UnixStream::pair()?;
            self.signals = Some(SignalDelivery::with_pipe(
                read,
                write,
                SignalOnly,
                [libc::SIGINT, libc::SIGTERM],
            )?);
            let mut mode = self.original;
            // Sound since mode is initialized and stdin stays open
            unsafe {
                libc::cfmakeraw(&mut mode);
                mode.c_oflag = self.original.c_oflag;
                if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &mode) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            self.raw = true;
        }

        let byte = self.byte(true)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if byte == 0x1b {
            match self.byte(false)? {
                Some(3) => return Ok(Key::CtrlC),
                Some(b'[' | b'O') => {
                    // CSI and SS3 keys end with a byte in the final character range
                    while let Some(byte) = self.byte(false)? {
                        if byte == 3 {
                            return Ok(Key::CtrlC);
                        }
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            return Ok(Key::Unknown);
        }
        Ok(match byte {
            b'\r' | b'\n' => Key::Enter,
            3 => Key::CtrlC,
            b'y' | b'Y' | b'n' | b'N' => Key::Char(char::from(byte)),
            _ => Key::Unknown,
        })
    }

    /// Reads a byte, or restores the terminal and exits when a signal arrives.
    fn byte(&mut self, wait: bool) -> io::Result<Option<u8>> {
        let signals = self.signals.as_mut().unwrap();
        let mut descriptors = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signals.get_read().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // Sound since the array holds two open descriptors for this poll
            let ready =
                unsafe { libc::poll(descriptors.as_mut_ptr(), 2, if wait { -1 } else { 0 }) };
            let error = (ready < 0).then(io::Error::last_os_error);
            if let Some(signal) = signals.pending().next() {
                self.restore()?;
                crate::ipc::local::exit(128 + signal);
            }
            if let Some(error) = error {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(None);
            }
            if descriptors[0].revents != 0 {
                let mut byte = 0;
                // Sound since stdin stays open and byte holds the one byte read
                let count = unsafe {
                    libc::read(libc::STDIN_FILENO, std::ptr::addr_of_mut!(byte).cast(), 1)
                };
                if count == 1 {
                    return Ok(Some(byte));
                }
                if count == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    /// Restores the terminal's saved flags.
    fn restore(&mut self) -> io::Result<()> {
        if self.raw {
            // Sound since stdin stays open and original came from tcgetattr
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.raw = false;
        }
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = self.restore();
        for (signal, action) in &self.actions {
            // Sound since these are the saved actions. Management commands have
            // no other Unix signal installer while this question owns input.
            unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) };
        }
        // A signal after the last key still stops the wipe before it starts
        if let Some(signals) = &mut self.signals
            && let Some(signal) = signals.pending().next()
        {
            crate::ipc::local::exit(128 + signal);
        }
        drop(self.signals.take());
    }
}
