//! A wait that doubles after each failure, for an operation that can fail
//! again and again, for example the start of the touch controller.
//!
//! The times are in milliseconds since the start of the device.

/// The first wait after a failure, in milliseconds.
pub const FIRST_WAIT_MS: u64 = 1_000;
/// The longest wait, in milliseconds.
pub const MAX_WAIT_MS: u64 = 30_000;

/// The state of an operation that waits longer after each failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    /// The time of the next try, or `None` to try now.
    next_try_ms: Option<u64>,
    /// The wait after the next failure.
    wait_ms: u64,
}

impl Backoff {
    /// Returns `true` if it is time to try at `now_ms`.
    #[must_use]
    pub fn ready(&self, now_ms: u64) -> bool {
        self.next_try_ms.is_none_or(|next| now_ms >= next)
    }

    /// Records a failure at `now_ms`, and returns the wait until the next
    /// try, in milliseconds. Log the failure with this wait: then the log
    /// has one line for each try, not one line for each poll.
    pub fn failed(&mut self, now_ms: u64) -> u64 {
        let wait = self.wait_ms.clamp(FIRST_WAIT_MS, MAX_WAIT_MS);
        self.next_try_ms = Some(now_ms.saturating_add(wait));
        self.wait_ms = wait.saturating_mul(2).min(MAX_WAIT_MS);
        wait
    }

    /// Records a success. The next failure waits the shortest time again.
    pub const fn succeeded(&mut self) {
        *self = Self {
            next_try_ms: None,
            wait_ms: 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, MAX_WAIT_MS};

    #[test]
    fn the_wait_doubles_to_the_maximum() {
        let mut backoff = Backoff::default();
        assert!(backoff.ready(0));
        let waits: Vec<u64> = (0..7).map(|_| backoff.failed(0)).collect();
        assert_eq!(waits, [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
    }

    #[test]
    fn there_is_no_try_before_the_wait_ends() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.failed(500), 1_000);
        assert!(!backoff.ready(1_499));
        assert!(backoff.ready(1_500));
        assert_eq!(backoff.failed(1_500), 2_000);
        assert!(!backoff.ready(3_000));
        assert!(backoff.ready(3_500));
    }

    #[test]
    fn a_success_resets_the_wait() {
        let mut backoff = Backoff::default();
        for _ in 0..10 {
            backoff.failed(0);
        }
        assert_eq!(backoff.failed(0), MAX_WAIT_MS);
        backoff.succeeded();
        assert!(backoff.ready(0));
        assert_eq!(backoff.failed(0), 1_000);
    }
}
