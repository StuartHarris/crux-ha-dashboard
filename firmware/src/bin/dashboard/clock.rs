//! The wall clock for `TimeNow`.
//!
//! The clock is the time since the start, plus the time of the start. The
//! firmware does not know the time of the start yet, so the clock starts at
//! the Unix epoch. The core treats a time before 2025 as not valid
//! (`shared::EARLIEST_VALID_TIME`), so it shows no "Updated HH:MM" text, not
//! a wrong one.
//!
//! TODO(5b): read the time of the start from the PCF8563 RTC (I2C 0x51).
//! Set the RTC with SNTP after the first Wi-Fi connection.

use core::sync::atomic::{AtomicU32, Ordering};

use crux_time::Instant;

/// The time of the start, in seconds since the Unix epoch. The chip has no
/// 64-bit atomics, and 32 bits are enough until 2106.
static START_SECONDS: AtomicU32 = AtomicU32::new(0);

/// Returns the wall clock now.
pub fn now() -> Instant {
    let micros = embassy_time::Instant::now().as_micros();
    let seconds = u64::from(START_SECONDS.load(Ordering::Relaxed)) + micros / 1_000_000;
    let nanos = u32::try_from((micros % 1_000_000) * 1_000).unwrap_or(0);
    Instant::new(seconds, nanos)
}

/// Writes a log line if the clock is not set.
pub fn log_not_set() {
    if u64::from(START_SECONDS.load(Ordering::Relaxed)) < shared::EARLIEST_VALID_TIME {
        log::warn!("clock: not set, so the core gets no valid time of day");
    }
}
