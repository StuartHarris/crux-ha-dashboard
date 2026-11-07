//! The local time of the dashboard: UK time (Europe/London).
//!
//! The core shows each time as UK time. It calculates the offset from UTC
//! with a rule, so it needs no time zone data and no help from the shell:
//!
//! - British Summer Time (BST, UTC+1) starts at 01:00 UTC on the last Sunday
//!   in March.
//! - Greenwich Mean Time (GMT, UTC+0) starts again at 01:00 UTC on the last
//!   Sunday in October.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, Offset, Utc};

/// The offset from UTC during British Summer Time, in seconds.
const BST_SECONDS: i32 = 3600;

/// The hour (UTC) at which the clocks change, on the last Sunday in March
/// and on the last Sunday in October.
const CHANGE_HOUR: u32 = 1;

/// Returns the UK offset from UTC at `time`: UTC+1 during British Summer
/// Time, and UTC+0 at other times.
///
/// Summer time starts at 01:00 UTC on the last Sunday in March. It stops
/// at 01:00 UTC on the last Sunday in October. The start is part of summer
/// time, and the end is not.
#[must_use]
pub fn uk_offset(time: DateTime<Utc>) -> FixedOffset {
    let utc = time.naive_utc();
    let year = utc.year();
    let summer = match (change(year, 3), change(year, 10)) {
        (Some(start), Some(end)) => start <= utc && utc < end,
        _ => false,
    };
    let seconds = if summer { BST_SECONDS } else { 0 };
    FixedOffset::east_opt(seconds).unwrap_or_else(|| Utc.fix())
}

/// Returns `time` as a UK date and time of day.
#[must_use]
pub fn uk_time(time: DateTime<Utc>) -> NaiveDateTime {
    time.with_timezone(&uk_offset(time)).naive_local()
}

/// Returns the time of the clock change in `month` of `year`: 01:00 UTC on
/// the last Sunday of the month. Returns `None` if the date is not valid.
fn change(year: i32, month: u32) -> Option<NaiveDateTime> {
    let last = last_day(year, month)?;
    let back = u64::from(last.weekday().num_days_from_sunday());
    let sunday = last.checked_sub_days(chrono::Days::new(back))?;
    sunday.and_hms_opt(CHANGE_HOUR, 0, 0)
}

/// Returns the last day of `month` in `year`.
fn last_day(year: i32, month: u32) -> Option<NaiveDate> {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate, Timelike, Utc};

    use super::{change, uk_offset, uk_time};

    /// Returns the UTC time at `hour:minute` on the date.
    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_opt(hour, minute, 0))
            .map(|time| time.and_utc())
            .unwrap()
    }

    /// Returns the offset at the time, in hours.
    fn hours(time: DateTime<Utc>) -> i32 {
        uk_offset(time).local_minus_utc() / 3600
    }

    /// The days on which summer time starts and stops, from the published
    /// UK dates.
    const CHANGES: [(i32, u32, u32); 6] = [
        (2024, 31, 27),
        (2025, 30, 26),
        (2026, 29, 25),
        (2027, 28, 31),
        (2028, 26, 29),
        (2032, 28, 31),
    ];

    #[test]
    fn the_clocks_change_on_the_last_sundays_of_march_and_october() {
        for (year, march, october) in CHANGES {
            assert_eq!(change(year, 3), Some(at(year, 3, march, 1, 0).naive_utc()));
            assert_eq!(
                change(year, 10),
                Some(at(year, 10, october, 1, 0).naive_utc())
            );
        }
    }

    #[test]
    fn summer_time_starts_at_01_00_utc_on_the_last_sunday_in_march() {
        for (year, march, _) in CHANGES {
            assert_eq!(hours(at(year, 3, march, 0, 59)), 0, "{year}");
            assert_eq!(hours(at(year, 3, march, 1, 0)), 1, "{year}");
            // The clocks go from 00:59 GMT to 02:00 BST.
            assert_eq!(uk_time(at(year, 3, march, 0, 59)).hour(), 0);
            assert_eq!(uk_time(at(year, 3, march, 1, 0)).hour(), 2);
        }
    }

    #[test]
    fn summer_time_stops_at_01_00_utc_on_the_last_sunday_in_october() {
        for (year, _, october) in CHANGES {
            assert_eq!(hours(at(year, 10, october, 0, 59)), 1, "{year}");
            assert_eq!(hours(at(year, 10, october, 1, 0)), 0, "{year}");
            // The clocks go from 01:59 BST back to 01:00 GMT.
            assert_eq!(uk_time(at(year, 10, october, 0, 59)).hour(), 1);
            assert_eq!(uk_time(at(year, 10, october, 1, 0)).hour(), 1);
        }
    }

    #[test]
    fn the_sunday_before_each_change_has_the_old_offset() {
        for (year, march, october) in CHANGES {
            assert_eq!(hours(at(year, 3, march - 7, 12, 0)), 0, "{year}");
            assert_eq!(hours(at(year, 10, october - 7, 12, 0)), 1, "{year}");
        }
    }

    #[test]
    fn winter_and_summer() {
        assert_eq!(hours(at(2026, 1, 1, 0, 0)), 0);
        assert_eq!(hours(at(2026, 7, 1, 8, 41)), 1);
        assert_eq!(hours(at(2026, 12, 31, 23, 59)), 0);
        // 29 February in a leap year is in winter.
        assert_eq!(hours(at(2028, 2, 29, 12, 0)), 0);
    }

    #[test]
    fn the_local_time_can_be_on_the_next_day() {
        let local = uk_time(at(2026, 6, 30, 23, 30));
        assert_eq!(local.date(), NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
        assert_eq!((local.hour(), local.minute()), (0, 30));
    }
}
