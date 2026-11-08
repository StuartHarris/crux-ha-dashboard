//! The wall clock for `TimeNow`.
//!
//! The clock is the time since the start, plus the time of the start. The
//! time of the start comes from two sources (`logic::clock`):
//!
//! 1. At boot, `main` reads the PCF8563 RTC before the core starts.
//! 2. After the device joins the network, the SNTP task sets the clock from
//!    the SNTP server. Then the display task writes the time to the RTC, if
//!    the RTC time is wrong. It tries a failed write again, up to 3 times.
//!
//! Until one of the sources gives a valid time, the clock starts at the Unix
//! epoch. The core treats a time before 2025 as not valid
//! (`shared::EARLIEST_VALID_TIME`), so it shows no "Updated HH:MM" text, not
//! a wrong one. The core asks for the time each time that it needs it, so a
//! new time of the start has an effect at once. The timers use the
//! monotonic clock, so a new time of the start does not change them.
//!
//! TODO(5b): on the board, make sure that the RTC answers at 0x51 and keeps
//! the time with no power from USB, that a read and a write of the RTC do
//! not disturb the GT911 on the same bus, and that SNTP gets a reply through
//! the router.

use core::{
    fmt,
    sync::atomic::{AtomicU32, Ordering},
};

use crux_time::Instant;
use logic::clock::{self, civil::DateTime};

/// The time of the start, in seconds since the Unix epoch. The chip has no
/// 64-bit atomics, and 32 bits are enough until 2106.
static START_SECONDS: AtomicU32 = AtomicU32::new(0);

/// Where the time of the start comes from.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// The PCF8563 RTC.
    Rtc,
    /// The SNTP server.
    Sntp,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rtc => "the RTC",
            Self::Sntp => "SNTP",
        })
    }
}

/// Returns the time since the start, in microseconds.
pub fn uptime_micros() -> u64 {
    embassy_time::Instant::now().as_micros()
}

/// Returns the wall clock now.
pub fn now() -> Instant {
    let micros = uptime_micros();
    let seconds = u64::from(START_SECONDS.load(Ordering::Relaxed)) + micros / 1_000_000;
    let nanos = u32::try_from((micros % 1_000_000) * 1_000).unwrap_or(0);
    Instant::new(seconds, nanos)
}

/// Sets the time of the start, and writes the new time to the log.
pub fn set(start: u32, source: Source) {
    let before = now().seconds();
    START_SECONDS.store(start, Ordering::Relaxed);
    let after = now().seconds();
    if clock::is_valid(before) {
        log::info!(
            "clock: set from {source} to {}, a change of {} s",
            DateTime::from_unix(after),
            i128::from(after) - i128::from(before)
        );
    } else {
        log::info!("clock: set from {source} to {}", DateTime::from_unix(after));
    }
}

/// Writes a log line if the clock is not set.
pub fn log_not_set() {
    if !clock::is_valid(now().seconds()) {
        log::warn!("clock: not set, so the core gets no valid time of day");
    }
}
