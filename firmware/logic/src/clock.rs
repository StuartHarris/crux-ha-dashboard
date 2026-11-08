//! The wall clock of the firmware: where its time comes from, and when to
//! ask SNTP again.
//!
//! The firmware keeps the wall clock as the time of the start (whole Unix
//! seconds) plus the time since the start. This module calculates the time
//! of the start from two sources:
//!
//! 1. At boot, from the PCF8563 RTC ([`pcf8563`], [`start_from_rtc`]).
//! 2. After the device joins the network, from an SNTP server ([`sntp`],
//!    [`start_from_sntp`]).
//!
//! The firmware writes the SNTP time to the RTC if the RTC has no valid time,
//! or if its time is more than [`RTC_TOLERANCE_SECONDS`] from the SNTP time
//! ([`rtc_needs_write`]). It writes at the start of a second
//! ([`next_second`]), and tries a failed write again ([`RtcWrite`]).
//! [`SyncSchedule`] decides when to ask SNTP, and a request that stops
//! because the network went down is not a failure ([`Outcome`]).
//!
//! A time before `shared::EARLIEST_VALID_TIME` (2025) is not valid. The core
//! shows no time of day until the clock has a valid time.

pub mod civil;
pub mod pcf8563;
pub mod sntp;

use shared::EARLIEST_VALID_TIME;

use crate::backoff::Backoff;
use sntp::UnixTime;

/// The largest difference between the RTC and SNTP that the firmware accepts
/// with no write to the RTC.
///
/// The RTC keeps whole seconds, and the firmware reads it at an unknown
/// point in its second. Thus a difference of 1 second is normal.
pub const RTC_TOLERANCE_SECONDS: u64 = 2;

/// The time from one good SNTP reply to the next request: 12 hours.
///
/// The crystal of the RTC and of the ESP32-S3 can drift by some seconds in a
/// day. Two requests in a day keep the clock in the minute that the
/// dashboard shows, with little network traffic.
pub const SYNC_INTERVAL_MS: u64 = 12 * 60 * 60 * 1_000;

/// The number of failures in sequence before the firmware stops the quick
/// retries. With `crate::backoff`, the waits are 1, 2, 4 and 8 seconds.
pub const MAX_FAILURES: u32 = 5;

/// The wait after [`MAX_FAILURES`] failures in sequence: 1 hour.
///
/// The server or the route is down. A retry each 30 seconds (the longest
/// wait of `crate::backoff`) uses the network for no result, and the RTC
/// keeps the time in the meantime.
pub const RETRY_LATER_MS: u64 = 60 * 60 * 1_000;

/// Returns `true` if `seconds` is a valid time of day for the core.
#[must_use]
pub const fn is_valid(seconds: u64) -> bool {
    seconds >= EARLIEST_VALID_TIME
}

/// Returns the time of the start from the RTC time `rtc` (from
/// [`pcf8563::decode_block`]) and the time since the start.
///
/// The uptime is rounded to the nearest second, so the error from the
/// rounding is at most 0.5 s.
///
/// Returns `None` if the RTC has no time, or if its time is not valid. Then
/// the clock stays unset until SNTP sets it.
#[must_use]
pub fn start_from_rtc(rtc: Option<u64>, uptime_micros: u64) -> Option<u32> {
    let rtc = rtc.filter(|seconds| is_valid(*seconds))?;
    let uptime_seconds = uptime_micros.saturating_add(500_000) / 1_000_000;
    u32::try_from(rtc.checked_sub(uptime_seconds)?).ok()
}

/// Returns the time of the start from an SNTP time and the time since the
/// start when the reply arrived. The result is rounded to the nearest
/// second.
///
/// Returns `None` if the SNTP time is not valid.
#[must_use]
pub fn start_from_sntp(time: UnixTime, uptime_micros: u64) -> Option<u32> {
    if !is_valid(time.seconds) {
        return None;
    }
    let now_nanos = u128::from(time.seconds) * 1_000_000_000 + u128::from(time.nanos);
    let start_nanos = now_nanos.checked_sub(u128::from(uptime_micros) * 1_000)?;
    u32::try_from((start_nanos + 500_000_000) / 1_000_000_000).ok()
}

/// Returns `true` if the firmware must write the time `now` to the RTC,
/// which has the time `rtc` (from [`pcf8563::decode_block`]).
#[must_use]
pub fn rtc_needs_write(rtc: Option<u64>, now: u64) -> bool {
    rtc.is_none_or(|rtc| rtc.abs_diff(now) > RTC_TOLERANCE_SECONDS)
}

/// Returns the next whole second after the clock time `seconds` plus
/// `nanos`, and the wait until it in microseconds (rounded up).
///
/// The PCF8563 starts to count a new second when the firmware writes it, and
/// a read gives only whole seconds. Thus the firmware waits for the start of
/// a second, and then writes that second. If `nanos` is 0, the second
/// starts now.
#[must_use]
pub fn next_second(seconds: u64, nanos: u32) -> (u64, u64) {
    if nanos == 0 {
        (seconds, 0)
    } else {
        let wait_nanos = 1_000_000_000_u64.saturating_sub(u64::from(nanos));
        (seconds + 1, wait_nanos.div_ceil(1_000))
    }
}

/// The number of tries to write the RTC after one SNTP reply.
pub const RTC_WRITE_TRIES: u32 = 3;
/// The wait after a failed write of the RTC, before the next try.
pub const RTC_RETRY_MS: u64 = 1_000;

/// A write of the RTC that the display task must do, with the tries that
/// are left.
///
/// After [`RTC_WRITE_TRIES`] failures, the firmware stops until the next
/// SNTP reply. The times are in milliseconds since the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtcWrite {
    /// The tries that are left. 0 means that no write is pending.
    tries_left: u32,
    /// The time of the next try.
    next_ms: u64,
}

impl RtcWrite {
    /// Records a request for a write at `now_ms`, after a good SNTP reply.
    /// A new request gives all the tries again.
    pub const fn request(&mut self, now_ms: u64) {
        self.tries_left = RTC_WRITE_TRIES;
        self.next_ms = now_ms;
    }

    /// Returns `true` if a write is pending and it is time to try it.
    #[must_use]
    pub const fn due(&self, now_ms: u64) -> bool {
        self.tries_left > 0 && now_ms >= self.next_ms
    }

    /// Records a write that succeeded, or that was not necessary.
    pub const fn succeeded(&mut self) {
        self.tries_left = 0;
    }

    /// Records a failed try at `now_ms`. Returns `true` if no tries are
    /// left: then log that the firmware stops.
    pub const fn failed(&mut self, now_ms: u64) -> bool {
        self.tries_left = self.tries_left.saturating_sub(1);
        self.next_ms = now_ms.saturating_add(RTC_RETRY_MS);
        self.tries_left == 0
    }
}

/// The result of one SNTP request, for [`SyncSchedule::record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A good reply set the clock.
    Synced,
    /// The request failed while the network was up: DNS, the send, no reply
    /// before the timeout, or a reply that is not valid.
    Failed,
    /// The network went down before or during the request. This is not a
    /// failure of the server, so the firmware waits for the network and
    /// tries again with no longer wait.
    NetworkDown,
}

impl Outcome {
    /// Returns the outcome of a request. `synced` is `true` if a good reply
    /// set the clock. `network_up` is `true` if the network stayed up for
    /// all of the request.
    #[must_use]
    pub const fn of(synced: bool, network_up: bool) -> Self {
        match (synced, network_up) {
            (true, _) => Self::Synced,
            (false, true) => Self::Failed,
            (false, false) => Self::NetworkDown,
        }
    }
}

/// When to send the next SNTP request.
///
/// The first request is at once, when the device has a network address.
/// After a good reply, the next request is after [`SYNC_INTERVAL_MS`]. After
/// a failure, the next request is after the wait of `crate::backoff`. After
/// [`MAX_FAILURES`] failures in sequence, the next request is after
/// [`RETRY_LATER_MS`], and the backoff starts again.
///
/// The times are in milliseconds since the start. A new Wi-Fi connection
/// does not change the schedule. The firmware waits for a network address
/// when a request is due.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncSchedule {
    /// The time of the next request.
    next_ms: u64,
    backoff: Backoff,
    /// The number of failures since the last good reply or the last long
    /// wait.
    failures: u32,
}

impl SyncSchedule {
    /// Returns the time of the next request.
    #[must_use]
    pub const fn next_ms(&self) -> u64 {
        self.next_ms
    }

    /// Records a good reply at `now_ms`, and returns the wait until the next
    /// request, in milliseconds.
    pub const fn succeeded(&mut self, now_ms: u64) -> u64 {
        self.backoff.succeeded();
        self.failures = 0;
        self.next_ms = now_ms.saturating_add(SYNC_INTERVAL_MS);
        SYNC_INTERVAL_MS
    }

    /// Records the outcome of a request that ended at `now_ms`. Returns the
    /// wait until the next request in milliseconds, or `None` for
    /// [`Outcome::NetworkDown`]. Then the schedule does not change: the
    /// request is still due, and the firmware sends it when the network is
    /// up again.
    pub fn record(&mut self, outcome: Outcome, now_ms: u64) -> Option<u64> {
        match outcome {
            Outcome::Synced => Some(self.succeeded(now_ms)),
            Outcome::Failed => Some(self.failed(now_ms)),
            Outcome::NetworkDown => None,
        }
    }

    /// Records a failure at `now_ms`, and returns the wait until the next
    /// request, in milliseconds.
    pub fn failed(&mut self, now_ms: u64) -> u64 {
        self.failures += 1;
        let wait = if self.failures >= MAX_FAILURES {
            self.failures = 0;
            self.backoff.succeeded();
            RETRY_LATER_MS
        } else {
            self.backoff.failed(now_ms)
        };
        self.next_ms = now_ms.saturating_add(wait);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FAILURES, Outcome, RETRY_LATER_MS, RTC_RETRY_MS, RTC_WRITE_TRIES, RtcWrite,
        SYNC_INTERVAL_MS, SyncSchedule, next_second, rtc_needs_write, sntp::UnixTime,
        start_from_rtc, start_from_sntp,
    };
    use shared::EARLIEST_VALID_TIME;

    /// 2026-11-08 13:45:30 UTC.
    const NOW: u64 = 1_794_145_530;

    #[test]
    fn the_start_from_the_rtc_is_its_time_minus_the_uptime() {
        // The uptime is rounded to the nearest second.
        assert_eq!(
            start_from_rtc(Some(NOW), 2_400_000),
            Some(u32::try_from(NOW - 2).unwrap())
        );
        assert_eq!(
            start_from_rtc(Some(NOW), 2_500_000),
            Some(u32::try_from(NOW - 3).unwrap())
        );
        assert_eq!(
            start_from_rtc(Some(NOW), 2_900_000),
            Some(u32::try_from(NOW - 3).unwrap())
        );
        assert_eq!(
            start_from_rtc(Some(EARLIEST_VALID_TIME), 0),
            Some(u32::try_from(EARLIEST_VALID_TIME).unwrap())
        );
    }

    #[test]
    fn an_rtc_with_no_valid_time_gives_no_start() {
        assert_eq!(start_from_rtc(None, 0), None);
        // The RTC kept time, but it was never set: 2000-01-01.
        assert_eq!(start_from_rtc(Some(946_684_800), 0), None);
        assert_eq!(start_from_rtc(Some(EARLIEST_VALID_TIME - 1), 0), None);
    }

    #[test]
    fn the_start_from_sntp_is_rounded_to_the_nearest_second() {
        let time = |nanos| UnixTime {
            seconds: NOW,
            nanos,
        };
        let start = u32::try_from(NOW - 10).unwrap();
        // 10.25 s since the start.
        assert_eq!(start_from_sntp(time(0), 10_250_000), Some(start));
        assert_eq!(start_from_sntp(time(700_000_000), 10_250_000), Some(start));
        assert_eq!(
            start_from_sntp(time(800_000_000), 10_250_000),
            Some(start + 1)
        );
    }

    #[test]
    fn an_sntp_time_before_2025_gives_no_start() {
        let time = UnixTime {
            seconds: EARLIEST_VALID_TIME - 1,
            nanos: 0,
        };
        assert_eq!(start_from_sntp(time, 0), None);
    }

    #[test]
    fn the_rtc_is_written_if_it_has_no_time_or_a_wrong_time() {
        assert!(rtc_needs_write(None, NOW));
        assert!(rtc_needs_write(Some(946_684_800), NOW));
        assert!(rtc_needs_write(Some(NOW - 3), NOW));
        assert!(rtc_needs_write(Some(NOW + 3), NOW));
        assert!(!rtc_needs_write(Some(NOW - 2), NOW));
        assert!(!rtc_needs_write(Some(NOW), NOW));
        assert!(!rtc_needs_write(Some(NOW + 2), NOW));
    }

    #[test]
    fn the_first_request_is_at_once() {
        assert_eq!(SyncSchedule::default().next_ms(), 0);
    }

    #[test]
    fn a_good_reply_waits_for_the_interval() {
        let mut schedule = SyncSchedule::default();
        assert_eq!(schedule.succeeded(5_000), SYNC_INTERVAL_MS);
        assert_eq!(schedule.next_ms(), 5_000 + SYNC_INTERVAL_MS);
    }

    #[test]
    fn failures_back_off_then_wait_longer() {
        let mut schedule = SyncSchedule::default();
        let mut waits = alloc::vec::Vec::new();
        let mut now = 0;
        for _ in 0..2 * MAX_FAILURES {
            let wait = schedule.failed(now);
            assert_eq!(schedule.next_ms(), now + wait);
            waits.push(wait);
            now += wait;
        }
        assert_eq!(
            waits,
            [
                1_000,
                2_000,
                4_000,
                8_000,
                RETRY_LATER_MS,
                1_000,
                2_000,
                4_000,
                8_000,
                RETRY_LATER_MS
            ]
        );
    }

    #[test]
    fn a_good_reply_resets_the_failures() {
        let mut schedule = SyncSchedule::default();
        for _ in 0..MAX_FAILURES - 1 {
            schedule.failed(0);
        }
        schedule.succeeded(0);
        assert_eq!(schedule.failed(0), 1_000);
    }

    #[test]
    fn the_rtc_write_waits_for_the_next_second() {
        assert_eq!(next_second(NOW, 0), (NOW, 0));
        assert_eq!(next_second(NOW, 1), (NOW + 1, 1_000_000));
        assert_eq!(next_second(NOW, 250_000_000), (NOW + 1, 750_000));
        assert_eq!(next_second(NOW, 999_999_999), (NOW + 1, 1));
    }

    #[test]
    fn a_failed_rtc_write_is_tried_again_up_to_the_limit() {
        let mut write = RtcWrite::default();
        assert!(!write.due(0));
        write.request(100);
        assert!(write.due(100));
        let mut now = 100;
        for try_number in 1..=RTC_WRITE_TRIES {
            assert!(write.due(now));
            let gave_up = write.failed(now);
            assert_eq!(gave_up, try_number == RTC_WRITE_TRIES);
            assert!(!write.due(now + RTC_RETRY_MS - 1));
            now += RTC_RETRY_MS;
        }
        assert!(!write.due(now));
        // The next SNTP reply gives all the tries again.
        write.request(now);
        assert!(write.due(now));
    }

    #[test]
    fn a_good_rtc_write_ends_the_tries() {
        let mut write = RtcWrite::default();
        write.request(0);
        assert!(!write.failed(0));
        write.succeeded();
        assert!(!write.due(RTC_RETRY_MS));
    }

    #[test]
    fn the_outcome_of_a_request() {
        assert_eq!(Outcome::of(true, true), Outcome::Synced);
        assert_eq!(Outcome::of(true, false), Outcome::Synced);
        assert_eq!(Outcome::of(false, true), Outcome::Failed);
        assert_eq!(Outcome::of(false, false), Outcome::NetworkDown);
    }

    #[test]
    fn a_network_down_is_not_a_failure() {
        let mut schedule = SyncSchedule::default();
        assert_eq!(schedule.record(Outcome::Failed, 0), Some(1_000));
        assert_eq!(schedule.record(Outcome::NetworkDown, 1_000), None);
        assert_eq!(schedule.next_ms(), 1_000);
        // The backoff did not grow, so the next failure waits 2 s.
        assert_eq!(schedule.record(Outcome::Failed, 1_000), Some(2_000));
        // Many network down outcomes do not reach the long wait.
        for _ in 0..2 * MAX_FAILURES {
            assert_eq!(schedule.record(Outcome::NetworkDown, 3_000), None);
        }
        assert_eq!(schedule.next_ms(), 3_000);
        assert_eq!(schedule.record(Outcome::Failed, 3_000), Some(4_000));
        assert_eq!(
            schedule.record(Outcome::Synced, 7_000),
            Some(SYNC_INTERVAL_MS)
        );
    }
}
