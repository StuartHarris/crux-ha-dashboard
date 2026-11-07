//! The timers of the timer task.
//!
//! The shell gives each timer a token (refer to [`crate::shell`]). The timer
//! task keeps the time at which each timer expires. When a timer expires, the
//! task sends [`Signal::Timer`] with the id and the token.
//!
//! The times are in milliseconds since the start of the device.

use alloc::vec::Vec;

use crux_time::TimerId;

use crate::shell::Signal;

/// The timers that run.
#[derive(Clone, Debug, Default)]
pub struct Timers {
    pending: Vec<Pending>,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    due_ms: u64,
    id: TimerId,
    token: u32,
}

impl Timers {
    /// Starts a timer that expires at `due_ms`.
    pub fn start(&mut self, id: TimerId, token: u32, due_ms: u64) {
        self.pending.push(Pending { due_ms, id, token });
    }

    /// Stops the timer with `token`, if it runs.
    pub fn cancel(&mut self, token: u32) {
        self.pending.retain(|pending| pending.token != token);
    }

    /// Returns the time at which the next timer expires, or `None` if no
    /// timer runs.
    #[must_use]
    pub fn next_due(&self) -> Option<u64> {
        self.pending.iter().map(|pending| pending.due_ms).min()
    }

    /// Removes the timers that expired at `now_ms`, and returns their
    /// signals, from the earliest.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Signal> {
        let mut expired: Vec<Pending> = Vec::new();
        self.pending.retain(|pending| {
            let due = pending.due_ms <= now_ms;
            if due {
                expired.push(*pending);
            }
            !due
        });
        expired.sort_by_key(|pending| pending.due_ms);
        expired
            .into_iter()
            .map(|Pending { id, token, .. }| Signal::Timer { id, token })
            .collect()
    }

    /// Returns the number of timers that run.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.pending.len()
    }

    /// Returns `true` if no timer runs.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crux_time::TimerId;

    use super::Timers;
    use crate::shell::Signal;

    #[test]
    fn timers_expire_in_order() {
        let mut timers = Timers::default();
        timers.start(TimerId(1), 10, 500);
        timers.start(TimerId(2), 11, 200);
        timers.start(TimerId(3), 12, 900);
        assert_eq!(timers.next_due(), Some(200));
        assert_eq!(timers.expire(100), []);
        assert_eq!(
            timers.expire(600),
            [
                Signal::Timer {
                    id: TimerId(2),
                    token: 11
                },
                Signal::Timer {
                    id: TimerId(1),
                    token: 10
                },
            ]
        );
        assert_eq!(timers.len(), 1);
        assert_eq!(timers.next_due(), Some(900));
    }

    #[test]
    fn a_cancelled_timer_does_not_expire() {
        let mut timers = Timers::default();
        timers.start(TimerId(1), 10, 500);
        timers.cancel(10);
        assert!(timers.is_empty());
        assert_eq!(timers.next_due(), None);
        assert_eq!(timers.expire(1000), []);
    }

    #[test]
    fn a_new_token_for_the_same_id_is_a_new_timer() {
        let mut timers = Timers::default();
        timers.start(TimerId(1), 10, 500);
        timers.cancel(10);
        timers.start(TimerId(1), 11, 800);
        assert_eq!(
            timers.expire(800),
            [Signal::Timer {
                id: TimerId(1),
                token: 11
            }]
        );
    }
}
