//! The control and time registers of the PCF8563 real-time clock (RTC).
//!
//! The firmware reads the nine registers from 0x00 to 0x08 in one I2C
//! transfer ([`Block`], [`decode_block`]), and writes them in one transfer
//! ([`write_payload`]). The time registers keep BCD numbers:
//!
//! | Register | Bits that the RTC uses | Content |
//! | --- | --- | --- |
//! | 0x00 | 7, 5, 3 | `Control_status_1`: TEST1, STOP, TESTC |
//! | 0x01 | 4..0 | `Control_status_2`: the alarm and timer interrupts |
//! | 0x02 | 7, 6..0 | VL (the clock integrity flag), seconds 0 to 59 |
//! | 0x03 | 6..0 | minutes 0 to 59 |
//! | 0x04 | 5..0 | hours 0 to 23 |
//! | 0x05 | 5..0 | day of the month 1 to 31 |
//! | 0x06 | 2..0 | day of the week 0 to 6 |
//! | 0x07 | 7, 4..0 | the century bit, month 1 to 12 |
//! | 0x08 | 7..0 | year 0 to 99 |
//!
//! The RTC sets VL when its supply voltage was too low, and then the time is
//! not reliable. A write of the seconds register clears VL.
//!
//! If STOP is set, the clock does not count. If TEST1 is set, the RTC is in
//! a test mode. In both cases the time is not reliable. A write sets
//! `Control_status_1` to 0, so the clock counts in normal mode after the
//! write. The write puts back the value of `Control_status_2` that the
//! firmware read, so the write does not change the interrupt settings. A
//! write of 1 to the AF and TF flags does not change them. The firmware
//! does not write the alarm and timer registers (0x09 to 0x0F).
//!
//! The other bits have no function, and their value on a read is not
//! defined. The decoder ignores them.
//!
//! The registers keep years 00 to 99, which this module reads as 2000 to
//! 2099. The meaning of the century bit is different in different drivers,
//! so the encoder writes 0 and the decoder ignores it. The day of the week
//! is 0 for Sunday, as in the PCF8563 data sheet. The RTC keeps UTC.

use super::civil::DateTime;

/// The I2C address of the RTC.
pub const ADDRESS: u8 = 0x51;
/// The first register of a read and of a write (`Control_status_1`).
pub const FIRST_REGISTER: u8 = 0x00;

/// The registers from 0x00 to 0x08: the two control registers, then the
/// seven time registers.
pub type Block = [u8; 9];

/// The seven time registers, from 0x02 to 0x08.
pub type Registers = [u8; 7];

/// The VL flag in the seconds register.
const VL: u8 = 0x80;
/// The TEST1 bit in `Control_status_1`.
const TEST1: u8 = 0x80;
/// The STOP bit in `Control_status_1`.
const STOP: u8 = 0x20;
/// The bits of `Control_status_2` that the RTC uses.
const CONTROL_2_BITS: u8 = 0x1F;

/// Returns the time in a block that the firmware read from register 0x00.
///
/// Returns `None` if STOP or TEST1 is set, or if [`decode`] gives `None`.
#[must_use]
pub fn decode_block(block: &Block) -> Option<u64> {
    if block[0] & (TEST1 | STOP) != 0 {
        return None;
    }
    let mut time: Registers = [0; 7];
    time.copy_from_slice(&block[2..]);
    decode(&time)
}

/// Returns the bytes of an I2C write of `seconds` after the Unix epoch.
///
/// The bytes are the register address 0x00, `Control_status_1` (0),
/// `Control_status_2` (from `control_2`, the value that the firmware read),
/// and the seven time registers from [`encode`].
///
/// Returns `None` if the time is not in the years 2000 to 2099.
#[must_use]
pub fn write_payload(seconds: u64, control_2: u8) -> Option<[u8; 10]> {
    let time = encode(seconds)?;
    let mut payload = [0; 10];
    payload[0] = FIRST_REGISTER;
    payload[1] = 0;
    payload[2] = control_2 & CONTROL_2_BITS;
    payload[3..].copy_from_slice(&time);
    Some(payload)
}

/// Returns the time in the registers as seconds after the Unix epoch.
///
/// Returns `None` if VL is set, if a register is not valid BCD, or if a
/// field is out of range (for example 31 April).
#[must_use]
pub fn decode(registers: &Registers) -> Option<u64> {
    let [seconds, minutes, hours, day, weekday, month, year] = *registers;
    if seconds & VL != 0 || weekday & 0x07 > 6 {
        return None;
    }
    DateTime {
        year: 2000 + from_bcd(year)?,
        month: from_bcd(month & 0x1F)?,
        day: from_bcd(day & 0x3F)?,
        hour: from_bcd(hours & 0x3F)?,
        minute: from_bcd(minutes & 0x7F)?,
        second: from_bcd(seconds & 0x7F)?,
    }
    .to_unix()
}

/// Returns the registers for `seconds` after the Unix epoch, with VL and the
/// century bit clear.
///
/// Returns `None` if the time is not in the years 2000 to 2099.
#[must_use]
pub fn encode(seconds: u64) -> Option<Registers> {
    let time = DateTime::from_unix(seconds);
    if !(2000..=2099).contains(&time.year) {
        return None;
    }
    Some([
        to_bcd(time.second),
        to_bcd(time.minute),
        to_bcd(time.hour),
        to_bcd(time.day),
        to_bcd(time.weekday()),
        to_bcd(time.month),
        to_bcd(time.year - 2000),
    ])
}

/// Returns the value of a BCD byte, or `None` if a digit is more than 9.
fn from_bcd(byte: u8) -> Option<u64> {
    let (tens, units) = (byte >> 4, byte & 0x0F);
    (tens <= 9 && units <= 9).then(|| u64::from(tens * 10 + units))
}

/// Returns the BCD byte of a value from 0 to 99.
fn to_bcd(value: u64) -> u8 {
    u8::try_from(((value / 10 % 10) << 4) | (value % 10)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{Block, Registers, decode, decode_block, encode, write_payload};

    /// 2026-11-08 13:45:30 UTC, a Sunday.
    const SUNDAY: Registers = [0x30, 0x45, 0x13, 0x08, 0x00, 0x11, 0x26];
    const SUNDAY_SECONDS: u64 = 1_794_145_530;

    /// [`SUNDAY`] after the control registers, with the alarm interrupt on.
    const SUNDAY_BLOCK: Block = [0x00, 0x02, 0x30, 0x45, 0x13, 0x08, 0x00, 0x11, 0x26];

    #[test]
    fn a_block_has_the_control_registers_then_the_time() {
        assert_eq!(decode_block(&SUNDAY_BLOCK), Some(SUNDAY_SECONDS));
        // TESTC (bit 3) is normal after a power-on reset.
        let mut block = SUNDAY_BLOCK;
        block[0] = 0x08;
        assert_eq!(decode_block(&block), Some(SUNDAY_SECONDS));
    }

    #[test]
    fn stop_or_test1_has_no_time() {
        for control_1 in [0x20, 0x80, 0xA0] {
            let mut block = SUNDAY_BLOCK;
            block[0] = control_1;
            assert_eq!(decode_block(&block), None, "{control_1:02X}");
        }
    }

    #[test]
    fn vl_set_in_a_block_has_no_time() {
        let mut block = SUNDAY_BLOCK;
        block[2] |= 0x80;
        assert_eq!(decode_block(&block), None);
    }

    #[test]
    fn a_write_starts_the_clock_and_keeps_control_2() {
        let payload = write_payload(SUNDAY_SECONDS, 0x02).unwrap();
        assert_eq!(payload[0], 0x00, "the first register");
        assert_eq!(payload[1], 0x00, "`Control_status_1`: STOP and TEST1 clear");
        assert_eq!(payload[2], 0x02, "`Control_status_2` as read");
        assert_eq!(payload[3..], SUNDAY);
        // The bits of `Control_status_2` with no function are written as 0.
        assert_eq!(write_payload(SUNDAY_SECONDS, 0xFF).unwrap()[2], 0x1F);
        // The payload from register 0x00 decodes to the same time.
        let mut block: Block = [0; 9];
        block.copy_from_slice(&payload[1..]);
        assert_eq!(decode_block(&block), Some(SUNDAY_SECONDS));
        assert_eq!(write_payload(4_102_444_800, 0), None);
    }

    #[test]
    fn a_known_time() {
        assert_eq!(decode(&SUNDAY), Some(SUNDAY_SECONDS));
        assert_eq!(encode(SUNDAY_SECONDS), Some(SUNDAY));
    }

    #[test]
    fn known_dates() {
        let cases: [(u64, Registers); 5] = [
            // 2000-01-01 00:00:00, Saturday.
            (946_684_800, [0x00, 0x00, 0x00, 0x01, 0x06, 0x01, 0x00]),
            // 2000-02-29 23:59:59, Tuesday.
            (951_868_799, [0x59, 0x59, 0x23, 0x29, 0x02, 0x02, 0x00]),
            // 2000-03-01 00:00:00, Wednesday.
            (951_868_800, [0x00, 0x00, 0x00, 0x01, 0x03, 0x03, 0x00]),
            // 2024-02-29 12:00:00, Thursday.
            (1_709_208_000, [0x00, 0x00, 0x12, 0x29, 0x04, 0x02, 0x24]),
            // 2099-12-31 23:59:59, Thursday.
            (4_102_444_799, [0x59, 0x59, 0x23, 0x31, 0x04, 0x12, 0x99]),
        ];
        for (seconds, registers) in cases {
            assert_eq!(encode(seconds), Some(registers), "{seconds}");
            assert_eq!(decode(&registers), Some(seconds), "{registers:02X?}");
        }
    }

    #[test]
    fn vl_set_has_no_time() {
        let mut registers = SUNDAY;
        registers[0] |= 0x80;
        assert_eq!(decode(&registers), None);
    }

    #[test]
    fn bits_with_no_function_are_ignored() {
        // Minutes bit 7, hours and days bits 7 and 6, weekday bits 7 to 3,
        // and month bits 6 and 5, with the century bit.
        let registers = [0x30, 0xC5, 0xD3, 0xC8, 0xF8, 0xF1, 0x26];
        assert_eq!(decode(&registers), Some(SUNDAY_SECONDS));
    }

    #[test]
    fn bad_bcd_has_no_time() {
        for (index, bad) in [
            (0, 0x5A),
            (1, 0x0F),
            (2, 0x1A),
            (3, 0x1B),
            (5, 0x0C),
            (6, 0xA0),
        ] {
            let mut registers = SUNDAY;
            registers[index] = bad;
            assert_eq!(decode(&registers), None, "register {index} = {bad:02X}");
        }
    }

    #[test]
    fn a_field_out_of_range_has_no_time() {
        let cases: [(usize, u8); 8] = [
            (0, 0x60), // second 60
            (1, 0x60), // minute 60
            (2, 0x24), // hour 24
            (3, 0x00), // day 0
            (3, 0x31), // 31 November
            (4, 0x07), // weekday 7
            (5, 0x00), // month 0
            (5, 0x13), // month 13
        ];
        for (index, value) in cases {
            let mut registers = SUNDAY;
            registers[index] = value;
            assert_eq!(decode(&registers), None, "register {index} = {value:02X}");
        }
        // 29 February 2025.
        assert_eq!(decode(&[0, 0, 0, 0x29, 0x06, 0x02, 0x25]), None);
    }

    #[test]
    fn a_time_out_of_the_century_has_no_registers() {
        assert_eq!(encode(946_684_799), None); // 1999-12-31 23:59:59
        assert_eq!(encode(4_102_444_800), None); // 2100-01-01
    }

    #[test]
    fn times_round_trip() {
        // About every 3 days and 7 hours, from 2000 to 2099, with all the
        // seconds of a minute in turn.
        let mut seconds = 946_684_800;
        while seconds < 4_102_444_800 {
            let registers = encode(seconds).unwrap();
            assert_eq!(decode(&registers), Some(seconds), "{registers:02X?}");
            seconds += 3 * 86_400 + 7 * 3_600 + 13;
        }
    }
}
