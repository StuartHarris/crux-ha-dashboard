//! The period of the report while the device sleeps on battery, and the
//! count of the checks of a sleep.
//!
//! Home Assistant sets the period in minutes with a number entity on
//! `set/report_period` ([`report_period`]). The core counts the checks of
//! [`CHECK_PERIOD`], so it keeps the period as a number of checks
//! ([`ReportPeriod`]).

use core::{num::NonZeroU16, time::Duration};

use super::CHECK_PERIOD;

/// The period of the report until Home Assistant sets one, and after an
/// empty payload. Refer to [`ReportPeriod::DEFAULT`].
///
/// The core counts the checks of [`CHECK_PERIOD`], so the period must be a
/// whole number of checks. The clock of the chip drifts in light sleep, and
/// each check and each wake with no cause adds a little time. Thus the real
/// period is a minute or two longer.
pub const REPORT_PERIOD: Duration = Duration::from_mins(60);

/// The number of checks in the default [`REPORT_PERIOD`]. The check with
/// this number reports.
pub const REPORT_CHECKS: u16 = checks_in(REPORT_PERIOD.as_secs());

/// The longest report period in minutes (12 hours).
pub const REPORT_PERIOD_MAX: u16 = 720;

/// Returns the number of checks of [`CHECK_PERIOD`] in `seconds`. The
/// asserts make sure at compile time that the time is a whole number of
/// checks, and that the count is in the range of `u16`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the assert makes sure that the count is in the range of u16"
)]
const fn checks_in(seconds: u64) -> u16 {
    let check = CHECK_PERIOD.as_secs();
    assert!(
        seconds.is_multiple_of(check),
        "the time is a whole number of checks"
    );
    let count = seconds / check;
    assert!(count > 0 && count <= u16::MAX as u64, "the count is in u16");
    count as u16
}

/// The longest period is a whole number of checks, and its count fits in
/// `u16`. A shorter period in whole minutes is a whole number of checks
/// too, because a minute is.
const _: () = {
    assert!(
        60_u64.is_multiple_of(CHECK_PERIOD.as_secs()),
        "a minute is a whole number of checks"
    );
    checks_in(REPORT_PERIOD_MAX as u64 * 60);
};

/// The period of the report while the device sleeps on battery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportPeriod {
    /// The device does not report while it sleeps. The checks of the power
    /// source still run.
    Off,
    /// The check with this number in a sleep reports.
    Every(NonZeroU16),
}

impl ReportPeriod {
    /// The period of [`REPORT_PERIOD`].
    pub const DEFAULT: Self = match NonZeroU16::new(REPORT_CHECKS) {
        Some(checks) => Self::Every(checks),
        None => panic!("the default period has checks"),
    };

    /// Returns the period of `minutes`: [`ReportPeriod::Off`] for 0. Returns
    /// `None` for a value more than [`REPORT_PERIOD_MAX`]. Each whole minute is a
    /// whole number of checks, so there is no other limit.
    #[must_use]
    pub const fn of_minutes(minutes: u16) -> Option<Self> {
        if minutes > REPORT_PERIOD_MAX {
            return None;
        }
        match NonZeroU16::new(minutes) {
            None => Some(Self::Off),
            Some(_) => match NonZeroU16::new(checks_in(minutes as u64 * 60)) {
                Some(checks) => Some(Self::Every(checks)),
                None => None,
            },
        }
    }

    /// Returns the period in minutes. The value 0 means that the device does
    /// not report.
    #[must_use]
    pub const fn minutes(self) -> u64 {
        match self {
            Self::Off => 0,
            Self::Every(checks) => checks.get() as u64 * CHECK_PERIOD.as_secs() / 60,
        }
    }
}

impl Default for ReportPeriod {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Returns the report period of the `payload` of a number of Home
/// Assistant, or `None` if the payload is not valid.
///
/// Home Assistant sends a whole number of minutes as an integer, for
/// example `30`. The core also accepts `30.0`. It rejects a fraction, a
/// negative value and a value more than [`REPORT_PERIOD_MAX`]. An empty
/// payload is [`ReportPeriod::DEFAULT`]: a user cleared the retained message.
#[must_use]
pub fn report_period(payload: &[u8]) -> Option<ReportPeriod> {
    let text = core::str::from_utf8(payload).ok()?.trim();
    if text.is_empty() {
        return Some(ReportPeriod::DEFAULT);
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !fraction.bytes().all(|byte| byte == b'0') {
        return None;
    }
    whole.parse::<u16>().ok().and_then(ReportPeriod::of_minutes)
}

/// The number of checks in a sleep. A report ends the sleep, so the count
/// of the next sleep starts at 0. If the report period is off, or if Home
/// Assistant makes the period longer, the count can go past the period. The
/// count stops at `u16::MAX` (about 45 days of checks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Checks(u16);

/// The result of one more check. Refer to [`Checks::tick`].
pub(super) enum Tick {
    /// The sleep continues with this count.
    Again(Checks),
    /// The check reports.
    Report,
}

impl Checks {
    /// Counts one more check. Returns `Report` at the first check at which
    /// the count reaches `period`. If Home Assistant made the period shorter
    /// than the count, the next check reports. If the period is off, no
    /// check reports.
    pub(super) const fn tick(self, period: ReportPeriod) -> Tick {
        let next = self.0.saturating_add(1);
        match period {
            ReportPeriod::Every(checks) if next >= checks.get() => Tick::Report,
            ReportPeriod::Every(_) | ReportPeriod::Off => Tick::Again(Self(next)),
        }
    }

    /// Returns the count of `checks`, for tests that start in the middle of
    /// a sleep.
    #[cfg(test)]
    pub(super) const fn of(checks: u16) -> Self {
        Self(checks)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU16;

    use super::{Checks, REPORT_CHECKS, ReportPeriod, Tick, report_period};

    fn every(checks: u16) -> ReportPeriod {
        ReportPeriod::Every(NonZeroU16::new(checks).unwrap())
    }

    #[test]
    fn the_default_is_sixty_checks_of_a_minute() {
        assert_eq!(REPORT_CHECKS, 60);
        assert_eq!(ReportPeriod::DEFAULT, every(60));
        assert_eq!(ReportPeriod::default(), every(60));
        assert_eq!(ReportPeriod::DEFAULT.minutes(), 60);
    }

    #[test]
    fn a_period_parses_any_whole_minute_up_to_720() {
        assert_eq!(report_period(b"0"), Some(ReportPeriod::Off));
        assert_eq!(report_period(b"3"), Some(every(3)));
        assert_eq!(report_period(b"5"), Some(every(5)));
        assert_eq!(report_period(b"7"), Some(every(7)));
        assert_eq!(report_period(b"719"), Some(every(719)));
        assert_eq!(report_period(b"30"), Some(every(30)));
        assert_eq!(report_period(b"30.0"), Some(every(30)));
        assert_eq!(report_period(b" 45 "), Some(every(45)));
        assert_eq!(report_period(b"720"), Some(every(720)));
        assert_eq!(report_period(b"0.0"), Some(ReportPeriod::Off));
    }

    #[test]
    fn an_empty_payload_is_the_default() {
        assert_eq!(report_period(b""), Some(ReportPeriod::DEFAULT));
        assert_eq!(report_period(b"  "), Some(ReportPeriod::DEFAULT));
    }

    #[test]
    fn a_period_that_is_not_valid_is_none() {
        for payload in [
            &b"721"[..],
            b"725",
            b"65536",
            b"-5",
            b"30.5",
            b".5",
            b"thirty",
            b"30 min",
            b"\xff",
        ] {
            assert_eq!(
                report_period(payload),
                None,
                "{}",
                alloc::string::String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn the_check_that_reaches_the_period_reports() {
        assert!(matches!(
            Checks::of(3).tick(every(5)),
            Tick::Again(Checks(4))
        ));
        assert!(matches!(Checks::of(4).tick(every(5)), Tick::Report));
        // A period shorter than the count reports at the next check.
        assert!(matches!(Checks::of(30).tick(every(5)), Tick::Report));
    }

    #[test]
    fn with_the_period_off_the_count_goes_on_and_stops_at_the_end_of_u16() {
        assert!(matches!(
            Checks::of(100).tick(ReportPeriod::Off),
            Tick::Again(Checks(101))
        ));
        assert!(matches!(
            Checks::of(u16::MAX).tick(ReportPeriod::Off),
            Tick::Again(Checks(u16::MAX))
        ));
        // The count at the end still reports at the longest period.
        assert!(matches!(
            Checks::of(u16::MAX).tick(every(720)),
            Tick::Report
        ));
    }
}
