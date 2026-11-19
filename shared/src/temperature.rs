//! The target temperature of a thermostat, and the steps of the − and +
//! buttons of a climate tile.
//!
//! The core does no arithmetic with floats. It reads the `temperature`
//! attribute of the entity as decimal text into a whole number of hundredths
//! of a degree. A step goes to the next mark of [`STEP`] up or down. The value
//! becomes a float only in the JSON of the service call, and each mark of
//! [`STEP`] has an exact float value.

use crate::entities::EntityState;

/// A temperature in hundredths of a degree, for example `1950` for 19.5 °C.
pub type Centi = i32;

/// The step of one tap on the − or the + button of a climate tile: 0.5 °C.
pub const STEP: Centi = 50;

/// The name of the attribute that gives the target temperature.
pub const TARGET: &str = "temperature";

/// Returns the target temperature of a thermostat, or `None` if the entity
/// has no `temperature` attribute or the value is not a decimal number. A
/// thermostat that is off can have no target temperature.
#[must_use]
pub fn target(entity: &EntityState) -> Option<Centi> {
    parse(entity.attribute(TARGET)?)
}

/// Returns the smallest mark of [`STEP`] that is more than `from`. For
/// example, 19.5 °C gives 20 °C and 19.3 °C gives 19.5 °C.
#[must_use]
pub const fn up(from: Centi) -> Centi {
    (from.div_euclid(STEP) + 1) * STEP
}

/// Returns the largest mark of [`STEP`] that is less than `from`. For
/// example, 19.5 °C gives 19 °C and 19.7 °C gives 19.5 °C.
#[must_use]
pub const fn down(from: Centi) -> Centi {
    let below = from.div_euclid(STEP);
    if below * STEP == from {
        (below - 1) * STEP
    } else {
        below * STEP
    }
}

/// Returns `centi` in degrees, for the JSON of a service call.
///
/// The division by 100 is exact for a mark of [`STEP`], because each half
/// degree has an exact float value and IEEE 754 division gives the nearest
/// float.
#[must_use]
pub fn degrees(centi: Centi) -> f64 {
    f64::from(centi) / 100.0
}

/// Reads decimal text, for example `19.5`, `21` or `-3.25`, as hundredths.
///
/// The core rounds a third decimal place to the nearest hundredth, so the
/// text `19.4999` of a float gives 19.50 °C. The core ignores the digits
/// after the third decimal place. Returns `None` for text with an exponent,
/// with no digits, with other characters or with too many digits.
#[must_use]
pub fn parse(text: &str) -> Option<Centi> {
    let text = text.trim();
    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let mut value: Centi = 0;
    for digit in whole.bytes() {
        value = value.checked_mul(10)?.checked_add(digit_value(digit)?)?;
    }
    // The first three decimal places, as thousandths.
    let mut thousandths: Centi = 0;
    let mut places = 0;
    for digit in fraction.bytes() {
        let digit = digit_value(digit)?;
        if places < 3 {
            thousandths = thousandths * 10 + digit;
            places += 1;
        }
    }
    while places < 3 {
        thousandths *= 10;
        places += 1;
    }
    let centi = value
        .checked_mul(100)?
        .checked_add((thousandths + 5) / 10)?;
    Some(if negative { -centi } else { centi })
}

/// Returns the value of an ASCII digit, or `None` for another byte.
const fn digit_value(byte: u8) -> Option<Centi> {
    if byte.is_ascii_digit() {
        Some((byte - b'0') as Centi)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{degrees, down, parse, up};

    #[test]
    fn parse_reads_decimal_text_as_hundredths() {
        assert_eq!(parse("19.5"), Some(1950));
        assert_eq!(parse("21"), Some(2100));
        assert_eq!(parse("20.0"), Some(2000));
        assert_eq!(parse("19.25"), Some(1925));
        assert_eq!(parse(".5"), Some(50));
        assert_eq!(parse("-3.25"), Some(-325));
        // The text of a float that is near a half degree.
        assert_eq!(parse("19.4999999"), Some(1950));
        assert_eq!(parse("19.5000001"), Some(1950));
        assert_eq!(parse("19.299999"), Some(1930));
        for text in ["", ".", "-", "abc", "1e2", "19,5", "null", "99999999999"] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn up_goes_to_the_next_half_degree() {
        assert_eq!(up(1950), 2000);
        assert_eq!(up(1930), 1950);
        assert_eq!(up(1999), 2000);
        assert_eq!(up(2000), 2050);
        assert_eq!(up(2100), 2150);
        assert_eq!(up(-30), 0);
        assert_eq!(up(-50), 0);
    }

    #[test]
    fn down_goes_to_the_previous_half_degree() {
        assert_eq!(down(1950), 1900);
        assert_eq!(down(1970), 1950);
        assert_eq!(down(1901), 1900);
        assert_eq!(down(2000), 1950);
        assert_eq!(down(2100), 2050);
        assert_eq!(down(30), 0);
        assert_eq!(down(0), -50);
        assert_eq!(down(-30), -50);
    }

    #[test]
    #[allow(clippy::float_cmp, reason = "a half degree has an exact float value")]
    fn degrees_is_exact_for_a_half_degree() {
        assert_eq!(degrees(2000), 20.0);
        assert_eq!(degrees(1950), 19.5);
        assert_eq!(degrees(3250), 32.5);
    }
}
