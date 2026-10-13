// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The clock that times narration and the prompt, which tests advance by
//! hand.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Monotonic time, and waits a stopped worker can cut short, for one output.
#[derive(Clone, Default)]
pub(super) struct Clock(
    /// The time and its wakeups, shared with the ticker.
    Arc<Time>,
);

/// The clock's state, locked across a wait or a wakeup, so no wakeup is lost.
#[derive(Default)]
struct Time {
    /// A test's fixed time, or none for the system clock.
    instant: Mutex<Option<Instant>>,
    /// Wakes a waiter when time advances or its worker stops.
    changed: Condvar,
}

impl Clock {
    /// The current instant.
    pub(super) fn now(&self) -> Instant {
        self.0.instant.lock().unwrap().unwrap_or_else(Instant::now)
    }

    /// Waits until `deadline`, and returns false when the worker stops first.
    pub(super) fn wait(&self, deadline: Instant, stopped: &AtomicBool) -> bool {
        let mut instant = self.0.instant.lock().unwrap();
        loop {
            if stopped.load(Ordering::SeqCst) {
                return false;
            }
            let now = instant.unwrap_or_else(Instant::now);
            if now >= deadline {
                return true;
            }
            instant = if instant.is_some() {
                self.0.changed.wait(instant).unwrap()
            } else {
                self.0
                    .changed
                    .wait_timeout(instant, deadline - now)
                    .unwrap()
                    .0
            };
        }
    }

    /// Stops a worker and wakes its pending wait.
    pub(super) fn stop(&self, stopped: &AtomicBool) {
        let _instant = self.0.instant.lock().unwrap();
        stopped.store(true, Ordering::SeqCst);
        self.0.changed.notify_all();
    }

    /// A clock that moves only when the test advances it.
    #[cfg(test)]
    pub(super) fn test() -> Self {
        let clock = Self::default();
        *clock.0.instant.lock().unwrap() = Some(Instant::now());
        clock
    }

    /// Advances test time and wakes every wait that can now finish.
    #[cfg(test)]
    pub(super) fn advance(&self, duration: std::time::Duration) {
        let mut instant = self.0.instant.lock().unwrap();
        *instant.as_mut().expect("a test clock") += duration;
        self.0.changed.notify_all();
    }
}
