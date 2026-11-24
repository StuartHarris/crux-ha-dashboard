//! The local time of the dashboard: UK time (Europe/London).
//!
//! The core shows each time as UK time. The binary contains the time zone
//! data for Europe/London, so the core needs no help from the shell. The data
//! comes from the IANA time zone database when the crate compiles.

use jiff::{Timestamp, civil::DateTime, tz::TimeZone};

/// The UK time zone, from the time zone data in the binary.
static UK: TimeZone = jiff::tz::get!("Europe/London");

/// Returns `time` as a UK date and time of day.
#[must_use]
pub fn uk_time(time: Timestamp) -> DateTime {
    UK.to_datetime(time)
}

#[cfg(test)]
mod tests {
    use jiff::{Timestamp, civil::date, tz::TimeZone};

    use super::{UK, uk_time};

    /// Returns the UTC time at `hour:minute` on the date.
    fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Timestamp {
        TimeZone::UTC
            .to_timestamp(date(year, month, day).at(hour, minute, 0, 0))
            .unwrap()
    }

    /// Returns the UK offset from UTC at the time, in hours.
    fn hours(time: Timestamp) -> i32 {
        UK.to_offset(time).seconds() / 3600
    }

    /// The days on which summer time starts and stops, from the published
    /// UK dates. The clocks change at 01:00 UTC on these days.
    const CHANGES: [(i16, i8, i8); 6] = [
        (2024, 31, 27),
        (2025, 30, 26),
        (2026, 29, 25),
        (2027, 28, 31),
        (2028, 26, 29),
        (2032, 28, 31),
    ];

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
        assert_eq!(local.date(), date(2026, 7, 1));
        assert_eq!((local.hour(), local.minute()), (0, 30));
    }
}
