// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Timed stages and transient notices, drawn on the page both streams share.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Spacing, emit, separate};
use crate::cli::clock::Clock;
use crate::cli::style::{self, Role, Theme};

/// The worker that redraws once a second, joined before its owner moves on.
pub(super) struct Ticker {
    /// Clock that wakes the worker for a redraw or shutdown.
    clock: Clock,
    /// Whether the worker has to stop.
    stopped: Arc<AtomicBool>,
    /// Worker joined when the ticker drops.
    worker: Option<JoinHandle<()>>,
}

impl Ticker {
    /// Starts redrawing once a second on `clock`.
    pub(super) fn start(clock: Clock, mut draw: impl FnMut(Instant) + Send + 'static) -> Self {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = stopped.clone();
        let worker_clock = clock.clone();
        let mut next = clock.now() + Duration::from_secs(1);
        let worker = std::thread::spawn(move || {
            while worker_clock.wait(next, &worker_stop) {
                let now = worker_clock.now();
                next = now + Duration::from_secs(1);
                draw(now);
            }
        });
        Self {
            clock,
            stopped,
            worker: Some(worker),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.clock.stop(&self.stopped);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// One stage's words and timing, kept while other events print.
pub(super) struct Activity {
    /// The stage's name in JSON.
    pub(super) stage: String,
    /// Words shown while waiting and on failure.
    pub(super) message: String,
    /// Words shown on success.
    pub(super) completed: String,
    /// When the timer started.
    pub(super) started: Instant,
    /// When the line last printed, which paces the redraws.
    pub(super) drawn: Option<Instant>,
}

/// A progress event with every fact present, null when unknown.
pub(super) fn progress(stage: Option<&str>, message: &str) -> Value {
    json!({
        "event": "progress", "message": message, "stage": stage,
        "phase": null, "step": null, "steps": null, "percent": null,
        "done_bytes": null, "total_bytes": null, "remaining_seconds": null,
    })
}

/// Erases the live line, leaving its stage in progress.
pub(super) fn close_line(page: &mut Spacing, writer: &mut impl Write) {
    if std::mem::take(&mut page.live) {
        let _ = write!(writer, "\r\x1b[2K");
        let _ = writer.flush();
    }
    page.notice = None;
}

/// Paints a narration line with plain words and muted facts.
fn line(theme: &Theme, words: &str, elapsed: &str, failed: Option<bool>) -> String {
    let message = format!(
        "{}{}",
        theme.inline(words),
        theme.paint(Role::Muted, format!(" {} {elapsed}", theme.glyph("·", "-"))),
    );
    match failed {
        Some(failed) => theme.mark(if failed { Role::Failure } else { Role::Success }, &message),
        None => format!("{} {message}", theme.glyph("›", ">")),
    }
}

/// Draws the stage's line at once, then every second on a terminal and every
/// 5s in a pipe.
pub(super) fn draw(
    theme: &Theme,
    page: &mut Spacing,
    writer: &mut impl Write,
    now: Instant,
    json: bool,
) {
    let Some(active) = page.active.as_mut() else {
        return;
    };
    let interval = Duration::from_secs(if theme.interactive && !json { 1 } else { 5 });
    if active
        .drawn
        .is_some_and(|last| now.saturating_duration_since(last) < interval)
    {
        return;
    }
    active.drawn = Some(now);
    let elapsed = style::duration(now.saturating_duration_since(active.started).as_secs());
    let document = progress(
        Some(&active.stage),
        &format!("{} · {elapsed}", active.message),
    );
    let line = line(theme, &active.message, &elapsed, None);
    if theme.interactive && !json {
        close_line(page, writer);
        separate(page, writer);
        let _ = write!(
            writer,
            "{}",
            theme.truncate(&line, theme.width.saturating_sub(1))
        );
        page.live = true;
        let _ = writer.flush();
    } else {
        emit(
            page,
            writer,
            json,
            &document,
            &style::wrap(&line, theme.width, 2),
        );
    }
}

/// Ends the stage as one line with its words and its time.
pub(super) fn end_activity(
    theme: &Theme,
    page: &mut Spacing,
    writer: &mut impl Write,
    now: Instant,
    json: bool,
    failed: bool,
) {
    close_line(page, writer);
    let Some(active) = page.active.take() else {
        return;
    };
    let words = if failed {
        &active.message
    } else {
        &active.completed
    };
    let elapsed = style::duration(now.saturating_duration_since(active.started).as_secs());
    let document = progress(Some(&active.stage), &format!("{words} · {elapsed}"));
    let line = line(theme, words, &elapsed, Some(failed));
    emit(
        page,
        writer,
        json,
        &document,
        &style::wrap(
            &line,
            theme.width,
            if theme.unicode || failed { 2 } else { 3 },
        ),
    );
}

/// Tests of the narration, on captured streams and a test clock.
#[cfg(test)]
pub(super) mod tests {
    use std::io;
    use std::path::Path;
    use std::sync::Mutex;

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::Cli;
    use crate::cli::output::Output;
    use crate::cli::style::Color;
    use crate::error::{Code, Error};

    /// Captured bytes shared by a writer and its assertions.
    #[derive(Clone, Default)]
    pub(in crate::cli::output) struct Capture(
        /// Bytes written by either the caller or the ticker.
        Arc<Mutex<Vec<u8>>>,
    );

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        /// Everything the output wrote, cursor movements included.
        pub(in crate::cli::output) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// An output with its own captures and a test clock.
    pub(in crate::cli::output) struct Fixture {
        /// The output under test.
        pub(in crate::cli::output) output: Output,
        /// What went to stdout.
        stdout: Capture,
        /// What went to stderr.
        pub(in crate::cli::output) stderr: Capture,
        /// The clock shared by the output and the keys.
        pub(in crate::cli::output) clock: Clock,
    }

    impl Fixture {
        /// Builds an output on a fixed terminal.
        pub(in crate::cli::output) fn new(interactive: bool, json: bool, quiet: bool) -> Self {
            let mut global = Cli::parse_from(["ark-emulator"]).global;
            global.json = json;
            global.quiet = quiet;
            let mut output = Output::new(&global);
            let state = Arc::get_mut(&mut output.0).unwrap();
            let clock = Clock::test();
            state.clock = clock.clone();
            state.err = Theme {
                interactive: interactive && !json,
                ..Theme::fixed(80, Color::Off, true)
            };
            state.out = state.err.clone();
            let stdout = Capture::default();
            let stderr = Capture::default();
            state.stdout = Arc::new(Mutex::new(Box::new(stdout.clone())));
            state.stderr = Arc::new(Mutex::new(Box::new(stderr.clone())));
            Self {
                output,
                stdout,
                stderr,
                clock,
            }
        }

        /// Stops the worker, so the test alone decides when a redraw happens.
        fn manual(&self) {
            drop(self.output.0.ticker.lock().unwrap().take());
        }

        /// Advances the clock and redraws if one is due.
        fn advance(&self, millis: u64) {
            self.clock.advance(Duration::from_millis(millis));
            draw(
                &self.output.0.err,
                &mut self.output.0.spacing.lock().unwrap(),
                &mut *self.output.0.stderr.lock().unwrap(),
                self.clock.now(),
                self.output.json(),
            );
        }
    }

    /// A terminal shows the line at once, then redraws it every second.
    #[test]
    fn test_live_stage_redraws_and_finishes() {
        let fixture = Fixture::new(true, false, false);
        fixture.output.booting(Path::new("/arks/dev.ark"), false);
        fixture.manual();
        assert_eq!(
            fixture.stderr.text(),
            "› Booting dev.ark · 0s",
            "first line needs no clock advance"
        );
        fixture.advance(999);
        assert_eq!(
            fixture.stderr.text(),
            "› Booting dev.ark · 0s",
            "no redraw before one second"
        );
        fixture.advance(1);
        fixture.advance(1000);
        fixture.output.end_stage();
        fixture
            .output
            .block(
                &json!({"locator":"emulator:18181"}),
                None,
                &[("Locator", "locator")],
            )
            .unwrap();
        assert_eq!(
            fixture.stderr.text(),
            "› Booting dev.ark · 0s\r\x1b[2K› Booting dev.ark · 1s\r\x1b[2K› Booting dev.ark · 2s\r\x1b[2K✓ Booted dev.ark · 2s\n\n",
            "live lines are erased and the outcome separates the result"
        );
        assert_eq!(
            fixture.stdout.text(),
            "  Locator  emulator:18181\n",
            "stdout contains the result alone"
        );
    }

    /// A pipe repeats the line every 5s, and every JSON event carries all eight
    /// facts.
    #[test]
    fn test_pipe_and_json_stage_cadence() {
        for json in [false, true] {
            let fixture = Fixture::new(false, json, false);
            fixture.output.booting(Path::new("dev.ark"), false);
            fixture.manual();
            for millis in [1000, 3999, 1, 4999, 1] {
                fixture.advance(millis);
            }
            fixture.output.end_stage();
            if json {
                let events: Vec<Value> = fixture
                    .stderr
                    .text()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                let expected: Vec<Value> = [
                    "Booting dev.ark · 0s",
                    "Booting dev.ark · 5s",
                    "Booting dev.ark · 10s",
                    "Booted dev.ark · 10s",
                ]
                .into_iter()
                .map(|message| {
                    json!({
                        "event":"progress", "message":message, "stage":"booting", "phase":null,
                        "step":null, "steps":null, "percent":null, "done_bytes":null,
                        "total_bytes":null, "remaining_seconds":null,
                    })
                })
                .collect();
                assert_eq!(
                    events, expected,
                    "JSON carries complete facts on starts, repeats and outcomes"
                );
            } else {
                assert_eq!(
                    fixture.stderr.text(),
                    "› Booting dev.ark · 0s\n› Booting dev.ark · 5s\n› Booting dev.ark · 10s\n✓ Booted dev.ark · 10s\n",
                    "pipe cadence and outcome"
                );
            }
        }
    }

    /// A boot reads in its caller's words, for an existing image, a new one and
    /// a failure.
    #[test]
    fn test_start_stage_shapes() {
        for (created, failed, expected) in [
            (
                false,
                false,
                "› Booting dev.ark · 0s\n✓ Booted dev.ark · 9s\n",
            ),
            (
                true,
                false,
                "› Booting dev.ark, a new image · 0s\n✓ Booted dev.ark, a new image · 9s\n",
            ),
            (
                false,
                true,
                "› Booting dev.ark · 0s\n✗ Booting dev.ark · 12s\n",
            ),
            (
                true,
                true,
                "› Booting dev.ark, a new image · 0s\n✗ Booting dev.ark, a new image · 12s\n",
            ),
        ] {
            let fixture = Fixture::new(false, false, false);
            fixture.output.booting(Path::new("/arks/dev.ark"), created);
            fixture.manual();
            fixture
                .clock
                .advance(Duration::from_secs(if failed { 12 } else { 9 }));
            fixture.output.outcome(failed);
            assert_eq!(
                fixture.stderr.text(),
                expected,
                "created={created}, failed={failed}"
            );
        }
    }

    /// An event erases the live line, and the next redraw brings it back.
    #[test]
    fn test_event_during_stage_and_notice_ownership() {
        let fixture = Fixture::new(true, false, false);
        let old = fixture.output.notice("Stopping Lab");
        fixture.output.booting(Path::new("dev.ark"), false);
        fixture.manual();
        drop(old);
        let skipped = fixture.output.checking("qemu");
        drop(skipped);
        fixture.output.event("note", "still waiting");
        fixture.advance(1000);
        fixture.output.end_stage();
        assert_eq!(
            fixture.stderr.text(),
            "› Stopping Lab\r\x1b[2K› Booting dev.ark · 0s\r\x1b[2Knote: still waiting\n› Booting dev.ark · 1s\r\x1b[2K✓ Booted dev.ark · 1s\n",
            "notices cannot erase an active stage"
        );
    }

    /// A notice is erased before a result, and never prints in a pipe, under
    /// --json or under --quiet.
    #[test]
    fn test_notice_lifecycle() {
        for (interactive, json, quiet) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let fixture = Fixture::new(interactive, json, quiet);
            let notice = fixture.output.checking("qemu");
            drop(notice);
            fixture
                .output
                .block(
                    &json!({"stopped":"emulator:18181"}),
                    None,
                    &[("Stopped", "stopped")],
                )
                .unwrap();
            assert_eq!(
                fixture.stderr.text(),
                if interactive && !json && !quiet {
                    "› Checking qemu\r\x1b[2K"
                } else {
                    ""
                },
                "interactive={interactive}, json={json}, quiet={quiet}"
            );
        }
    }

    /// Quiet hides every stage line, and an error keeps its hints.
    #[test]
    fn test_quiet_stages_and_error_hints() {
        for quiet in [false, true] {
            for json in [false, true] {
                let fixture = Fixture::new(false, json, quiet);
                fixture.output.booting(Path::new("dev.ark"), false);
                fixture.manual();
                fixture.clock.advance(Duration::from_secs(12));
                fixture.output.error(
                    &Error::new(Code::Timeout, "boot timed out").hint("watch `ark-emulator list`"),
                );
                if json {
                    let events: Vec<Value> = fixture
                        .stderr
                        .text()
                        .lines()
                        .map(|line| serde_json::from_str(line).unwrap())
                        .collect();
                    let error = json!({"code":"timeout", "message":"boot timed out", "hints":["watch `ark-emulator list`"]});
                    assert_eq!(
                        events.len(),
                        if quiet { 1 } else { 3 },
                        "quiet={quiet}, no separate hint event"
                    );
                    assert_eq!(
                        events.last().unwrap(),
                        &json!({"event":"error", "error":error}),
                        "error retains its hints"
                    );
                    assert_eq!(
                        serde_json::from_str::<Value>(&fixture.stdout.text()).unwrap(),
                        json!({"error":error}),
                        "error result retains its hints"
                    );
                    if !quiet {
                        assert_eq!(
                            events[1]["message"], "Booting dev.ark · 12s",
                            "failed outcome precedes the error"
                        );
                    }
                } else {
                    assert_eq!(
                        fixture.stderr.text(),
                        format!(
                            "{}error[timeout]: boot timed out\nhint: watch `ark-emulator list`\n",
                            if quiet {
                                ""
                            } else {
                                "› Booting dev.ark · 0s\n✗ Booting dev.ark · 12s\n"
                            }
                        ),
                        "quiet={quiet}, reading hints follow the error"
                    );
                }
            }
        }
    }

    /// The ticker moves only with the test clock, and stops without waiting for
    /// a tick.
    #[test]
    fn test_ticker_waits_on_the_test_clock() {
        let clock = Clock::test();
        let start = clock.now();
        let (sender, receiver) = std::sync::mpsc::channel();
        let ticker = Ticker::start(clock.clone(), move |now| sender.send(now).unwrap());
        clock.advance(Duration::from_millis(999));
        assert!(receiver.try_recv().is_err(), "no tick before 1s");
        clock.advance(Duration::from_millis(1));
        assert_eq!(
            receiver.recv().unwrap(),
            start + Duration::from_secs(1),
            "first tick"
        );
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            receiver.recv().unwrap(),
            start + Duration::from_secs(2),
            "second tick"
        );
        drop(ticker);
        assert!(
            receiver.recv().is_err(),
            "drop stops and joins the waiting worker"
        );
    }

    /// Only the mark takes color and the facts print muted, long times
    /// included.
    #[test]
    fn test_stage_palette_and_duration() {
        let mut fixture = Fixture::new(true, false, false);
        Arc::get_mut(&mut fixture.output.0).unwrap().err.color = Color::True;
        fixture.output.booting(Path::new("dev.ark"), false);
        fixture.manual();
        fixture.clock.advance(Duration::from_secs(62));
        fixture.output.end_stage();
        assert_eq!(
            fixture.stderr.text(),
            "› Booting dev.ark\x1b[38;2;124;128;152m · 0s\x1b[0m\r\x1b[2K\x1b[1m\x1b[38;2;148;202;110m✓\x1b[0m Booted dev.ark\x1b[38;2;124;128;152m · 1m 02s\x1b[0m\n",
            "plain words, colored mark and muted timer"
        );
    }

    /// Without glyphs a line keeps its long timer and fits a narrow terminal.
    #[test]
    fn test_ascii_stages_and_narrow_live_lines() {
        let mut fixture = Fixture::new(true, false, false);
        let state = Arc::get_mut(&mut fixture.output.0).unwrap();
        state.err.unicode = false;
        fixture.output.booting(Path::new("dev.ark"), false);
        fixture.manual();
        fixture.clock.advance(Duration::from_secs(3900));
        fixture.output.end_stage();
        assert_eq!(
            fixture.stderr.text(),
            "> Booting dev.ark - 0s\r\x1b[2Kok Booted dev.ark - 1h 05m\n",
            "ASCII marks, separator and long timer"
        );

        let mut fixture = Fixture::new(true, false, false);
        Arc::get_mut(&mut fixture.output.0).unwrap().err.width = 24;
        fixture
            .output
            .booting(Path::new("a very long image name.ark"), false);
        fixture.manual();
        assert_eq!(
            fixture.stderr.text(),
            "› Booting a very long …",
            "one live row leaves the last terminal column free"
        );
        fixture.output.fail_stage();
    }
}
