//! Converts between Unix seconds and a UTC date and time.
//!
//! The functions use the algorithms `days_from_civil` and `civil_from_days`
//! of Howard Hinnant (<https://howardhinnant.github.io/date_algorithms.html>),
//! with unsigned numbers, because the firmware has no time before 1970.

use core::fmt;

/// The number of days from 0000-03-01 to 1970-01-01 in the proleptic
/// Gregorian calendar.
const UNIX_EPOCH_DAYS: u64 = 719_468;
/// The number of days in 400 years.
const DAYS_PER_ERA: u64 = 146_097;
/// The number of seconds in a day.
pub const SECONDS_PER_DAY: u64 = 86_400;

/// A date and a time of day in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    /// The year, for example 2026.
    pub year: u64,
    /// The month, 1 to 12.
    pub month: u64,
    /// The day of the month, 1 to 31.
    pub day: u64,
    /// The hour, 0 to 23.
    pub hour: u64,
    /// The minute, 0 to 59.
    pub minute: u64,
    /// The second, 0 to 59.
    pub second: u64,
}

impl DateTime {
    /// Returns the date and time of `seconds` after the Unix epoch.
    #[must_use]
    pub const fn from_unix(seconds: u64) -> Self {
        let (year, month, day) = civil_from_days(seconds / SECONDS_PER_DAY);
        let time = seconds % SECONDS_PER_DAY;
        Self {
            year,
            month,
            day,
            hour: time / 3_600,
            minute: time / 60 % 60,
            second: time % 60,
        }
    }

    /// Returns the seconds after the Unix epoch. Returns `None` if a field is
    /// out of range, or if the time is before 1970.
    #[must_use]
    pub fn to_unix(&self) -> Option<u64> {
        let valid = self.year >= 1970
            && (1..=12).contains(&self.month)
            && (1..=days_in_month(self.year, self.month)).contains(&self.day)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60;
        valid.then(|| {
            days_from_civil(self.year, self.month, self.day) * SECONDS_PER_DAY
                + self.hour * 3_600
                + self.minute * 60
                + self.second
        })
    }

    /// Returns the day of the week: 0 is Sunday, 6 is Saturday.
    #[must_use]
    pub const fn weekday(&self) -> u64 {
        // 1970-01-01 was a Thursday.
        (days_from_civil(self.year, self.month, self.day) + 4) % 7
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Returns `true` if `year` has 29 February.
#[must_use]
pub const fn is_leap_year(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Returns the number of days in `month` (1 to 12) of `year`, or 0 for a
/// month that is out of range.
#[must_use]
pub const fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Returns the number of days from 1970-01-01 to a date in 1970 or later.
/// The month is 1 to 12, and the day is 1 to 31.
const fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    // The year starts on 1 March, so 29 February is the last day of a year.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_ERA + day_of_era - UNIX_EPOCH_DAYS
}

/// Returns the year, the month and the day of `days` after 1970-01-01.
const fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let days = days + UNIX_EPOCH_DAYS;
    let era = days / DAYS_PER_ERA;
    let day_of_era = days - era * DAYS_PER_ERA;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{DateTime, days_in_month, is_leap_year};

    const fn date(year: u64, month: u64, day: u64) -> DateTime {
        DateTime {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }

    #[test]
    fn known_dates() {
        let cases = [
            (date(1970, 1, 1), 0),
            (date(2000, 1, 1), 946_684_800),
            (date(2000, 2, 29), 951_782_400),
            (date(2000, 3, 1), 951_868_800),
            (date(2024, 2, 29), 1_709_164_800),
            (date(2025, 1, 1), 1_735_689_600),
            (date(2099, 12, 31), 4_102_358_400),
            (date(2100, 3, 1), 4_107_542_400),
        ];
        for (date, seconds) in cases {
            assert_eq!(date.to_unix(), Some(seconds), "{date}");
            assert_eq!(DateTime::from_unix(seconds), date, "{seconds}");
        }
    }

    #[test]
    fn a_time_of_day() {
        // 2026-11-06 12:00:00 UTC, plus 1 hour, 2 minutes and 3 seconds.
        let time = DateTime::from_unix(1_793_966_400 + 3_600 + 2 * 60 + 3);
        assert_eq!(
            time,
            DateTime {
                year: 2026,
                month: 11,
                day: 6,
                hour: 13,
                minute: 2,
                second: 3,
            }
        );
        assert_eq!(alloc::format!("{time}"), "2026-11-06 13:02:03 UTC");
    }

    #[test]
    fn weekdays() {
        // Thursday, Saturday, Wednesday and Thursday.
        assert_eq!(date(1970, 1, 1).weekday(), 4);
        assert_eq!(date(2000, 1, 1).weekday(), 6);
        assert_eq!(date(2000, 3, 1).weekday(), 3);
        assert_eq!(date(2099, 12, 31).weekday(), 4);
    }

    #[test]
    fn leap_years() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2100));
        assert!(!is_leap_year(2025));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2100, 2), 28);
        assert_eq!(days_in_month(2025, 13), 0);
    }

    #[test]
    fn a_field_out_of_range_has_no_time() {
        assert_eq!(date(2025, 2, 29).to_unix(), None);
        assert_eq!(date(2025, 4, 31).to_unix(), None);
        assert_eq!(date(2025, 0, 1).to_unix(), None);
        assert_eq!(date(2025, 1, 0).to_unix(), None);
        assert_eq!(date(1969, 12, 31).to_unix(), None);
        let late = DateTime {
            hour: 24,
            ..date(2025, 1, 1)
        };
        assert_eq!(late.to_unix(), None);
    }

    #[test]
    fn each_day_round_trips() {
        // Every day from 1970 to 2200.
        for day in 0..84_000 {
            let seconds = day * super::SECONDS_PER_DAY + 43_210;
            let date = DateTime::from_unix(seconds);
            assert_eq!(date.to_unix(), Some(seconds), "{date}");
        }
    }
}
