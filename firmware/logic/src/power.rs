//! Reads the power source and the charge of the battery from the registers
//! of the charger and the fuel gauge, and decides which changes to send to
//! the core.
//!
//! - The BQ25896 charger (I2C address 0x6B) tells if USB gives power, and
//!   the phase of the charge. Refer to [`source_from_bq25896`] and
//!   [`charge_from_bq25896`].
//! - The BQ27220 fuel gauge (I2C address 0x55) gives the charge in percent,
//!   the voltage, the current and the temperature. Refer to
//!   [`level_from_bq27220`], [`voltage_from_bq27220`],
//!   [`current_from_bq27220`] and [`tenth_celsius_from_bq27220`].
//! - [`PowerMonitor`] gives an event to the core only when a value changes.
//!   It rounds the diagnostics first ([`round_reading`]).
//!
//! The registers and the units come from the TI data sheets: the BQ25896
//! data sheet (SLUSC76C, table 17, REG0B) and the BQ27220 technical
//! reference manual (SLUUBD4A, table 2-1, standard commands).

use alloc::vec::Vec;

use shared::{Charge, Event, PowerSource, Reading};

/// The I2C address of the BQ25896 charger.
pub const CHARGER_ADDRESS: u8 = 0x6B;
/// The status register of the BQ25896 (REG0B). It has `VBUS_STAT` in bits
/// 7:5, `CHRG_STAT` in bits 4:3, `PG_STAT` in bit 2 and `VSYS_STAT` in
/// bit 0.
pub const CHARGER_STATUS: u8 = 0x0B;
/// The I2C address of the BQ27220 fuel gauge.
pub const GAUGE_ADDRESS: u8 = 0x55;
/// The `StateOfCharge()` command of the BQ27220 (0x2C and 0x2D). The gauge
/// gives the charge in percent as a little-endian `u16`.
pub const GAUGE_STATE_OF_CHARGE: u8 = 0x2C;
/// The `AverageCurrent()` command of the BQ27220 (0x14 and 0x15).
///
/// The gauge gives the average current in mA as a little-endian `i16`
/// (SLUUBD4A, table 2-1). The average is a filter of `Current()` (section
/// 4.9.9), which is a signed value in mA (section 2.8). The gauge enters
/// its CHARGE mode when `Current()` is above a positive threshold, and its
/// DISCHARGE mode when it is below a negative threshold (section 1.1.11).
/// Thus a positive value charges the battery, and a negative value
/// discharges it. A check on the board is still to do. Table 2-1 gives the
/// unit of `Current()` as mAh, but section 2.8 gives mA.
pub const GAUGE_AVERAGE_CURRENT: u8 = 0x14;
/// The `Voltage()` command of the BQ27220 (0x08 and 0x09). The gauge gives
/// the voltage of the cell in mV as a little-endian `u16`, from 0 to 6000
/// mV (SLUUBD4A, section 2.6).
pub const GAUGE_VOLTAGE: u8 = 0x08;
/// The `Temperature()` command of the BQ27220 (0x06 and 0x07).
///
/// The gauge gives the temperature in units of 0.1 K as a little-endian
/// `u16` (SLUUBD4A, section 2.5). The configuration of the gauge selects
/// its internal sensor or a thermistor.
pub const GAUGE_TEMPERATURE: u8 = 0x06;

/// The `PG_STAT` bit of REG0B: 1 if the input power is good.
const POWER_GOOD: u8 = 1 << 2;
/// The `VBUS_STAT` value of the OTG mode, in which the charger gives power to
/// the USB port.
const VBUS_OTG: u8 = 0b111;
/// The highest voltage that the BQ27220 gives, in mV.
const MAX_VOLTAGE_MV: u16 = 6000;
/// The lowest voltage that is plausible for the cell, in mV. A lower value,
/// for example 0 from a read of a gauge that is not ready, is not valid.
const MIN_VOLTAGE_MV: u16 = 2000;
/// 0 °C in units of 0.01 K.
const ZERO_CELSIUS_CENTI_KELVIN: i32 = 27_315;
/// The range of a temperature that is valid, in tenths of a degree Celsius:
/// -40 °C to 100 °C. The gauge works from -40 °C to 85 °C.
const VALID_TENTH_CELSIUS: core::ops::RangeInclusive<i32> = -400..=1000;
/// The step of the rounded voltage, in mV.
pub const VOLTAGE_STEP_MV: i32 = 10;
/// The step of the rounded current, in mA.
pub const CURRENT_STEP_MA: i32 = 10;
/// The step of the rounded temperature, in tenths of a degree Celsius.
pub const TEMPERATURE_STEP_TENTHS: i32 = 5;

/// Returns the power source from REG0B of the BQ25896.
///
/// The device is on the stand if the input power is good (`PG_STAT` is 1).
/// In the OTG mode (`VBUS_STAT` is 111), the battery gives the power, so the
/// device is on battery.
#[must_use]
pub const fn source_from_bq25896(reg0b: u8) -> PowerSource {
    let vbus = reg0b >> 5;
    if reg0b & POWER_GOOD != 0 && vbus != VBUS_OTG {
        PowerSource::Stand
    } else {
        PowerSource::Battery
    }
}

/// Returns the phase of the charge from REG0B of the BQ25896: `CHRG_STAT`
/// in bits 4:3.
///
/// | `CHRG_STAT` | Phase |
/// | --- | --- |
/// | 00 | not charging |
/// | 01 | pre-charge (below `VBATLOWV`) |
/// | 10 | fast charge (constant current, then constant voltage) |
/// | 11 | charge termination done |
#[must_use]
pub const fn charge_from_bq25896(reg0b: u8) -> Charge {
    match (reg0b >> 3) & 0b11 {
        0b00 => Charge::NotCharging,
        0b01 => Charge::PreCharging,
        0b10 => Charge::FastCharging,
        _ => Charge::Done,
    }
}

/// Returns the charge in percent from the two bytes of the
/// `StateOfCharge()` command of the BQ27220, or `None` if the value is more
/// than 100.
#[must_use]
pub fn level_from_bq27220(bytes: [u8; 2]) -> Option<u8> {
    let percent = u16::from_le_bytes(bytes);
    u8::try_from(percent).ok().filter(|percent| *percent <= 100)
}

/// Returns the current in mA from the two bytes of the `AverageCurrent()`
/// or `Current()` command of the BQ27220. A negative value is a discharge.
#[must_use]
pub const fn current_from_bq27220(bytes: [u8; 2]) -> i16 {
    i16::from_le_bytes(bytes)
}

/// Returns the voltage in mV from the two bytes of the `Voltage()` command
/// of the BQ27220, or `None` if the value is less than 2000 mV or more than
/// 6000 mV.
#[must_use]
pub fn voltage_from_bq27220(bytes: [u8; 2]) -> Option<u16> {
    Some(u16::from_le_bytes(bytes))
        .filter(|millivolts| (MIN_VOLTAGE_MV..=MAX_VOLTAGE_MV).contains(millivolts))
}

/// Returns the temperature in tenths of a degree Celsius from the two bytes
/// of the `Temperature()` command of the BQ27220.
///
/// Returns `None` if the value is not from -40 °C to 100 °C. The gauge
/// gives units of 0.1 K. The function rounds to the nearest tenth.
#[must_use]
pub fn tenth_celsius_from_bq27220(bytes: [u8; 2]) -> Option<i16> {
    let centi_celsius = i32::from(u16::from_le_bytes(bytes)) * 10 - ZERO_CELSIUS_CENTI_KELVIN;
    let tenths = round_to(centi_celsius, 10) / 10;
    if !VALID_TENTH_CELSIUS.contains(&tenths) {
        return None;
    }
    i16::try_from(tenths).ok()
}

/// Returns `value` rounded to the nearest multiple of `step`. A value
/// halfway between two multiples goes up.
const fn round_to(value: i32, step: i32) -> i32 {
    (2 * value + step).div_euclid(2 * step) * step
}

/// Returns the reading with each value rounded.
///
/// The voltage goes to 10 mV, the current to 10 mA and the temperature to
/// 0.5 °C. A small change of a value then gives the same reading, so each
/// poll does not give an event.
#[must_use]
pub fn round_reading(reading: Reading) -> Reading {
    Reading {
        charge: reading.charge,
        voltage_mv: reading.voltage_mv.map(|millivolts| {
            // At most 6000 mV after the decoder, so the result fits.
            u16::try_from(round_to(i32::from(millivolts), VOLTAGE_STEP_MV)).unwrap_or(u16::MAX)
        }),
        current_ma: reading
            .current_ma
            .map(|milliamps| saturate(round_to(i32::from(milliamps), CURRENT_STEP_MA))),
        tenth_celsius: reading
            .tenth_celsius
            .map(|tenths| saturate(round_to(i32::from(tenths), TEMPERATURE_STEP_TENTHS))),
    }
}

/// Returns `value` as an `i16`, limited to the range of `i16`.
fn saturate(value: i32) -> i16 {
    i16::try_from(value).unwrap_or(if value < 0 { i16::MIN } else { i16::MAX })
}

/// Decides when the power source and the charge changed.
///
/// A change of the power source needs two good, equal reads in a row,
/// because the contacts of a USB plug can bounce. A failed read (`None`)
/// keeps the last value and gives no event. A failed read of the source also
/// clears a change that waits for its second read.
///
/// The fuel gauge can give 0% before it has an estimate of the charge. Thus
/// a level of 0 while the device is on the stand is not known, as a failed
/// read.
///
/// A monitor from [`PowerMonitor::fixed`] keeps its power source. It ignores
/// the reads of the charger for the source, and it gives only the events of
/// the charge and of the diagnostics.
///
/// The diagnostics go through [`PowerMonitor::read`]. The monitor rounds
/// them, and gives `PowerRead` only when a rounded value changes. A failed
/// read of one value keeps the last good value, as for the charge. So a
/// value is `None` only until its first good read.
///
/// A change of the phase of the charge (`CHRG_STAT`) also needs two good,
/// equal reads in a row, as the power source does. Near the end of a charge
/// and at the threshold of a new charge, the phase can toggle, and a start
/// or an end of a charge is an urgent change in the core. The first good
/// read of the phase counts at once.
///
/// [`PowerMonitor::poll`] gives one read of everything, and returns
/// `PowerRead` before the other events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerMonitor {
    /// The power source that the core has.
    source: PowerSource,
    /// The monitor ignores the reads of the charger.
    fixed: bool,
    /// A new power source that waits for a second read.
    pending: Option<PowerSource>,
    /// The charge that the core has, or `None` before the first good read.
    level: Option<u8>,
    /// The rounded diagnostics that the core has.
    reading: Reading,
    /// A new phase of the charge that waits for a second read.
    pending_charge: Option<Charge>,
}

impl PowerMonitor {
    /// Makes a monitor. `source` is the power source that the shell gave in
    /// `Start`, so the monitor does not send it again. The first good read
    /// of the charge always gives an event.
    #[must_use]
    pub const fn new(source: PowerSource) -> Self {
        Self {
            source,
            fixed: false,
            pending: None,
            level: None,
            reading: Reading {
                charge: None,
                voltage_mv: None,
                current_ma: None,
                tenth_celsius: None,
            },
            pending_charge: None,
        }
    }

    /// Makes a monitor that keeps the power source `source`, for example
    /// `Battery` while the device is on USB power for a test of the sleep.
    /// The monitor sends no `PowerSourceChanged`, and the rule for 0% uses
    /// `source`.
    #[must_use]
    pub const fn fixed(source: PowerSource) -> Self {
        Self {
            fixed: true,
            ..Self::new(source)
        }
    }

    /// Returns `true` if a new power source waits for its second read. The
    /// shell then reads again soon, so the change does not wait for a full
    /// poll interval.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Gives the result of one read of each chip, and returns the events
    /// for the core. `None` is a read that failed. Refer to the type
    /// documentation for the rules.
    pub fn update(&mut self, source: Option<PowerSource>, level: Option<u8>) -> Vec<Event> {
        let mut events = Vec::new();
        let source = if self.fixed { None } else { source };
        let on_stand = source.unwrap_or(self.source) == PowerSource::Stand;
        let level = level.filter(|level| *level > 0 || !on_stand);
        let Some(source) = source else {
            self.pending = None;
            return self.update_level(level, events);
        };
        if source == self.source {
            self.pending = None;
        } else if self.pending == Some(source) {
            self.pending = None;
            self.source = source;
            events.push(Event::PowerSourceChanged(source));
        } else {
            self.pending = Some(source);
        }
        self.update_level(level, events)
    }

    /// Gives the result of one read of each chip, as [`PowerMonitor::update`]
    /// and [`PowerMonitor::read`] do, and returns the events for the core.
    /// `reading` is `None` if the shell did not read the diagnostics.
    ///
    /// `PowerRead` comes first. At a move to or off the stand, the core then
    /// has the new phase of the charge before the new power source.
    pub fn poll(
        &mut self,
        source: Option<PowerSource>,
        level: Option<u8>,
        reading: Option<Reading>,
    ) -> Vec<Event> {
        let mut events: Vec<Event> = reading
            .and_then(|reading| self.read(reading))
            .into_iter()
            .collect();
        events.extend(self.update(source, level));
        events
    }

    /// Gives one read of the diagnostics, with each value as the decoder
    /// gives it. `None` is a read that failed, and keeps the last good
    /// value. Returns `PowerRead` with the rounded values if one of them
    /// changed. A new phase of the charge counts only at its second read in
    /// a row.
    pub fn read(&mut self, read: Reading) -> Option<Event> {
        let read = round_reading(read);
        let last = self.reading;
        let reading = Reading {
            charge: self.debounce_charge(read.charge),
            voltage_mv: read.voltage_mv.or(last.voltage_mv),
            current_ma: read.current_ma.or(last.current_ma),
            tenth_celsius: read.tenth_celsius.or(last.tenth_celsius),
        };
        if reading == last {
            return None;
        }
        self.reading = reading;
        Some(Event::PowerRead(reading))
    }

    /// Returns the phase of the charge after the read `read`. A failed read
    /// keeps the last phase and clears a change that waits. A new phase
    /// counts at its second read in a row. The first good read counts at
    /// once.
    fn debounce_charge(&mut self, read: Option<Charge>) -> Option<Charge> {
        let last = self.reading.charge;
        let (Some(read), Some(_)) = (read, last) else {
            self.pending_charge = None;
            return read.or(last);
        };
        if Some(read) == last {
            self.pending_charge = None;
            last
        } else if self.pending_charge == Some(read) {
            self.pending_charge = None;
            Some(read)
        } else {
            self.pending_charge = Some(read);
            last
        }
    }

    /// Adds the event for `level` to `events`, if the level is good and
    /// changed.
    fn update_level(&mut self, level: Option<u8>, mut events: Vec<Event>) -> Vec<Event> {
        if let Some(level) = level
            && self.level != Some(level)
        {
            self.level = Some(level);
            events.push(Event::BatteryChanged(Some(level)));
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use shared::{Charge, Event, PowerSource, Reading};

    use super::{
        PowerMonitor, charge_from_bq25896, current_from_bq27220, level_from_bq27220, round_reading,
        source_from_bq25896, tenth_celsius_from_bq27220, voltage_from_bq27220,
    };

    /// A read of the diagnostics with no value.
    const NOTHING: Reading = Reading {
        charge: None,
        voltage_mv: None,
        current_ma: None,
        tenth_celsius: None,
    };

    /// A read of the diagnostics on battery, as the decoders give it.
    const READ: Reading = Reading {
        charge: Some(Charge::NotCharging),
        voltage_mv: Some(3874),
        current_ma: Some(-43),
        tenth_celsius: Some(251),
    };

    #[test]
    fn the_charger_status_gives_the_phase_of_the_charge() {
        // CHRG_STAT is bits 4:3. Bit 1 always reads 1.
        assert_eq!(charge_from_bq25896(0x00), Charge::NotCharging);
        assert_eq!(charge_from_bq25896(0x02), Charge::NotCharging);
        // A USB host, power good, pre-charge (01).
        assert_eq!(charge_from_bq25896(0b0010_1110), Charge::PreCharging);
        // A USB host, power good, fast charge (10).
        assert_eq!(charge_from_bq25896(0b0011_0110), Charge::FastCharging);
        // An adapter, power good, charge done (11).
        assert_eq!(charge_from_bq25896(0b0101_1110), Charge::Done);
        // The other fields do not change the phase.
        assert_eq!(charge_from_bq25896(0b1110_0101), Charge::NotCharging);
        assert_eq!(charge_from_bq25896(0b1111_1111), Charge::Done);
    }

    #[test]
    fn the_gauge_gives_the_voltage_in_millivolts() {
        // 0x0F1E is 3870 mV, little-endian.
        assert_eq!(voltage_from_bq27220([0x1E, 0x0F]), Some(3870));
        assert_eq!(voltage_from_bq27220([0x68, 0x10]), Some(4200));
        // 2000 mV and 6000 mV are the limits.
        assert_eq!(voltage_from_bq27220([0xD0, 0x07]), Some(2000));
        assert_eq!(voltage_from_bq27220([0xCF, 0x07]), None);
        assert_eq!(voltage_from_bq27220([0x70, 0x17]), Some(6000));
        // A gauge that is not ready can give 0.
        assert_eq!(voltage_from_bq27220([0, 0]), None);
        // The gauge gives at most 6000 mV.
        assert_eq!(voltage_from_bq27220([0x71, 0x17]), None);
        assert_eq!(voltage_from_bq27220([0xFF, 0xFF]), None);
    }

    #[test]
    fn the_gauge_gives_the_temperature_in_tenths_of_a_degree_celsius() {
        // 2981 is 298.1 K, which is 24.95 °C. Half a tenth goes up.
        assert_eq!(tenth_celsius_from_bq27220([0xA5, 0x0B]), Some(250));
        // 2982 is 298.2 K, which is 25.05 °C.
        assert_eq!(tenth_celsius_from_bq27220([0xA6, 0x0B]), Some(251));
        // 2731 is 273.1 K, which is -0.05 °C.
        assert_eq!(tenth_celsius_from_bq27220([0xAB, 0x0A]), Some(0));
        // 2631 is 263.1 K, which is -10.05 °C.
        assert_eq!(tenth_celsius_from_bq27220([0x47, 0x0A]), Some(-100));
        // -40 °C (2331) and 100 °C (3731) are the limits.
        assert_eq!(tenth_celsius_from_bq27220([0x1B, 0x09]), Some(-400));
        assert_eq!(tenth_celsius_from_bq27220([0x93, 0x0E]), Some(1000));
        assert_eq!(tenth_celsius_from_bq27220([0x1A, 0x09]), None);
        assert_eq!(tenth_celsius_from_bq27220([0x94, 0x0E]), None);
        // 0 K and the largest value are not valid.
        assert_eq!(tenth_celsius_from_bq27220([0, 0]), None);
        assert_eq!(tenth_celsius_from_bq27220([0xFF, 0xFF]), None);
    }

    #[test]
    fn a_reading_is_rounded_to_its_steps() {
        let round = |voltage_mv, current_ma, tenth_celsius| {
            let reading = round_reading(Reading {
                charge: None,
                voltage_mv: Some(voltage_mv),
                current_ma: Some(current_ma),
                tenth_celsius: Some(tenth_celsius),
            });
            (
                reading.voltage_mv.unwrap(),
                reading.current_ma.unwrap(),
                reading.tenth_celsius.unwrap(),
            )
        };
        // 10 mV, 10 mA and 0.5 °C. Halfway goes up.
        assert_eq!(round(3874, 4, 252), (3870, 0, 250));
        assert_eq!(round(3875, 5, 253), (3880, 10, 255));
        assert_eq!(round(3866, 16, 247), (3870, 20, 245));
        // Below 0, halfway also goes up.
        assert_eq!(round(0, -4, -12), (0, 0, -10));
        assert_eq!(round(0, -5, -13), (0, 0, -15));
        assert_eq!(round(0, -6, -2), (0, -10, 0));
        assert_eq!(round(0, -15, -3), (0, -10, -5));
        assert_eq!(round(0, -16, -8), (0, -20, -10));
        // The limits saturate.
        assert_eq!(round(6000, i16::MIN, 1000), (6000, i16::MIN, 1000));
        assert_eq!(round(5999, i16::MAX, -400), (6000, i16::MAX, -400));
        // The phase and the values that are not known stay.
        assert_eq!(
            round_reading(Reading {
                charge: Some(Charge::Done),
                ..NOTHING
            }),
            Reading {
                charge: Some(Charge::Done),
                ..NOTHING
            }
        );
    }

    #[test]
    fn a_reading_goes_only_when_a_rounded_value_changes() {
        let mut monitor = PowerMonitor::new(PowerSource::Battery);
        let first = Reading {
            charge: Some(Charge::NotCharging),
            voltage_mv: Some(3870),
            current_ma: Some(-40),
            tenth_celsius: Some(250),
        };
        assert_eq!(monitor.read(READ), Some(Event::PowerRead(first)));
        assert_eq!(monitor.read(READ), None);
        // Small changes give the same rounded reading.
        assert_eq!(
            monitor.read(Reading {
                voltage_mv: Some(3866),
                current_ma: Some(-36),
                tenth_celsius: Some(248),
                ..READ
            }),
            None
        );
        // A rounded value changes.
        assert_eq!(
            monitor.read(Reading {
                current_ma: Some(-46),
                ..READ
            }),
            Some(Event::PowerRead(Reading {
                current_ma: Some(-50),
                ..first
            }))
        );
    }

    #[test]
    fn a_failed_read_of_a_value_keeps_its_last_value() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        // Nothing read yet, so no reading.
        assert_eq!(monitor.read(NOTHING), None);
        // Only the charger answers. The gauge values are not known.
        assert_eq!(
            monitor.read(Reading {
                charge: Some(Charge::FastCharging),
                ..NOTHING
            }),
            Some(Event::PowerRead(Reading {
                charge: Some(Charge::FastCharging),
                ..NOTHING
            }))
        );
        let _ = monitor.read(Reading {
            charge: Some(Charge::FastCharging),
            ..READ
        });
        // Each read fails, so the reading stays.
        assert_eq!(monitor.read(NOTHING), None);
        // The charger fails and the voltage changes.
        assert_eq!(
            monitor.read(Reading {
                charge: None,
                voltage_mv: Some(3900),
                ..READ
            }),
            Some(Event::PowerRead(Reading {
                charge: Some(Charge::FastCharging),
                voltage_mv: Some(3900),
                current_ma: Some(-40),
                tenth_celsius: Some(250),
            }))
        );
    }

    #[test]
    fn a_fixed_source_still_gives_the_phase_of_the_charge() {
        let mut monitor = PowerMonitor::fixed(PowerSource::Battery);
        assert_eq!(
            monitor.read(Reading {
                charge: Some(Charge::FastCharging),
                ..NOTHING
            }),
            Some(Event::PowerRead(Reading {
                charge: Some(Charge::FastCharging),
                ..NOTHING
            }))
        );
    }

    /// Returns `READ` with the phase `charge`.
    const fn with_charge(charge: Charge) -> Reading {
        Reading {
            charge: Some(charge),
            ..READ
        }
    }

    /// Returns the event of the rounded `READ` with the phase `charge`.
    const fn read_event(charge: Charge) -> Event {
        Event::PowerRead(Reading {
            charge: Some(charge),
            voltage_mv: Some(3870),
            current_ma: Some(-40),
            tenth_celsius: Some(250),
        })
    }

    #[test]
    fn a_new_phase_of_the_charge_needs_two_equal_reads() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        // The first good read counts at once.
        assert_eq!(
            monitor.read(with_charge(Charge::FastCharging)),
            Some(read_event(Charge::FastCharging))
        );
        // One different read does not change the phase.
        assert_eq!(monitor.read(with_charge(Charge::Done)), None);
        assert_eq!(monitor.read(with_charge(Charge::FastCharging)), None);
        // A phase that toggles at each read never counts.
        for _ in 0..3 {
            assert_eq!(monitor.read(with_charge(Charge::Done)), None);
            assert_eq!(monitor.read(with_charge(Charge::FastCharging)), None);
        }
        // Two equal reads in a row count.
        assert_eq!(monitor.read(with_charge(Charge::Done)), None);
        assert_eq!(
            monitor.read(with_charge(Charge::Done)),
            Some(read_event(Charge::Done))
        );
    }

    #[test]
    fn a_failed_read_of_the_charger_clears_a_new_phase_that_waits() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        let _ = monitor.read(with_charge(Charge::FastCharging));
        assert_eq!(monitor.read(with_charge(Charge::Done)), None);
        assert_eq!(monitor.read(READ_NO_CHARGE), None);
        // The change needs two more good reads.
        assert_eq!(monitor.read(with_charge(Charge::Done)), None);
        assert_eq!(
            monitor.read(with_charge(Charge::Done)),
            Some(read_event(Charge::Done))
        );
    }

    /// `READ` with a failed read of the charger.
    const READ_NO_CHARGE: Reading = Reading {
        charge: None,
        ..READ
    };

    #[test]
    fn a_poll_gives_the_reading_before_the_change_of_the_source() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        let charging = with_charge(Charge::FastCharging);
        let _ = monitor.poll(Some(PowerSource::Stand), Some(80), Some(charging));
        // The plug comes out: the source and the phase change on two reads.
        let off = with_charge(Charge::NotCharging);
        assert_eq!(
            monitor.poll(Some(PowerSource::Battery), Some(80), Some(off)),
            []
        );
        assert_eq!(
            monitor.poll(Some(PowerSource::Battery), Some(80), Some(off)),
            [
                read_event(Charge::NotCharging),
                Event::PowerSourceChanged(PowerSource::Battery)
            ]
        );
        // With no reading, only the source and the charge go.
        assert_eq!(monitor.poll(Some(PowerSource::Stand), None, None), []);
        assert_eq!(
            monitor.poll(Some(PowerSource::Stand), None, None),
            [Event::PowerSourceChanged(PowerSource::Stand)]
        );
    }

    #[test]
    fn the_charger_status_gives_the_power_source() {
        // No input: VBUS_STAT 000, PG_STAT 0.
        assert_eq!(source_from_bq25896(0x00), PowerSource::Battery);
        // PG_STAT 1 alone.
        assert_eq!(source_from_bq25896(0b0000_0100), PowerSource::Stand);
        // A USB host (VBUS_STAT 001), power good, fast charge (CHRG_STAT 10).
        assert_eq!(source_from_bq25896(0b0011_0100), PowerSource::Stand);
        // An adapter (VBUS_STAT 010), power good, charge done (CHRG_STAT 11).
        assert_eq!(source_from_bq25896(0b0101_1100), PowerSource::Stand);
        // VBUS_STAT says USB, but the power is not good yet.
        assert_eq!(source_from_bq25896(0b0010_0000), PowerSource::Battery);
        // The OTG mode (VBUS_STAT 111) is on battery.
        assert_eq!(source_from_bq25896(0b1110_0000), PowerSource::Battery);
        assert_eq!(source_from_bq25896(0b1110_0100), PowerSource::Battery);
        // VSYS_STAT (bit 0) does not change the source.
        assert_eq!(source_from_bq25896(0b0000_0001), PowerSource::Battery);
    }

    #[test]
    fn the_gauge_gives_the_charge_in_percent() {
        assert_eq!(level_from_bq27220([0, 0]), Some(0));
        assert_eq!(level_from_bq27220([78, 0]), Some(78));
        assert_eq!(level_from_bq27220([100, 0]), Some(100));
        // More than 100, also in the high byte, is not valid.
        assert_eq!(level_from_bq27220([101, 0]), None);
        assert_eq!(level_from_bq27220([50, 1]), None);
        assert_eq!(level_from_bq27220([0xFF, 0xFF]), None);
    }

    #[test]
    fn the_gauge_gives_a_signed_current() {
        assert_eq!(current_from_bq27220([0, 0]), 0);
        // 500 mA of charge.
        assert_eq!(current_from_bq27220([0xF4, 0x01]), 500);
        // A discharge of 5 mA.
        assert_eq!(current_from_bq27220([0xFB, 0xFF]), -5);
        assert_eq!(current_from_bq27220([0x00, 0x80]), i16::MIN);
    }

    #[test]
    fn a_fixed_source_ignores_the_charger() {
        let mut monitor = PowerMonitor::fixed(PowerSource::Battery);
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), Some(90)),
            [Event::BatteryChanged(Some(90))]
        );
        assert_eq!(monitor.update(Some(PowerSource::Stand), Some(90)), []);
        assert!(!monitor.is_pending());
        // The rule for 0% uses the fixed source, so 0% is a real level.
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), Some(0)),
            [Event::BatteryChanged(Some(0))]
        );
    }

    #[test]
    fn the_first_level_is_sent_and_the_start_source_is_not() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), Some(78)),
            [Event::BatteryChanged(Some(78))]
        );
        assert_eq!(monitor.update(Some(PowerSource::Stand), Some(78)), []);
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), Some(77)),
            [Event::BatteryChanged(Some(77))]
        );
    }

    #[test]
    fn a_new_source_needs_two_equal_reads() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        let _ = monitor.update(Some(PowerSource::Stand), Some(80));

        assert_eq!(monitor.update(Some(PowerSource::Battery), Some(80)), []);
        assert!(monitor.is_pending());
        assert_eq!(
            monitor.update(Some(PowerSource::Battery), Some(80)),
            [Event::PowerSourceChanged(PowerSource::Battery)]
        );
        assert!(!monitor.is_pending());
        assert_eq!(monitor.update(Some(PowerSource::Battery), Some(80)), []);
    }

    #[test]
    fn a_bounce_gives_no_event() {
        let mut monitor = PowerMonitor::new(PowerSource::Battery);
        assert_eq!(monitor.update(Some(PowerSource::Stand), None), []);
        assert_eq!(monitor.update(Some(PowerSource::Battery), None), []);
        assert!(!monitor.is_pending());
        // The next read of the stand is a first read again.
        assert_eq!(monitor.update(Some(PowerSource::Stand), None), []);
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), None),
            [Event::PowerSourceChanged(PowerSource::Stand)]
        );
    }

    #[test]
    fn a_failed_read_keeps_the_last_values() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        assert_eq!(monitor.update(None, None), []);
        let _ = monitor.update(Some(PowerSource::Stand), Some(60));
        assert_eq!(monitor.update(None, None), []);

        // A failed read of the source between two reads of a new source
        // clears the change that waits. The change needs two more good
        // reads.
        assert_eq!(monitor.update(Some(PowerSource::Battery), None), []);
        assert_eq!(monitor.update(None, Some(60)), []);
        assert!(!monitor.is_pending());
        assert_eq!(monitor.update(Some(PowerSource::Battery), None), []);
        assert_eq!(
            monitor.update(Some(PowerSource::Battery), Some(60)),
            [Event::PowerSourceChanged(PowerSource::Battery)]
        );
    }

    #[test]
    fn a_failed_level_read_does_not_clear_a_source_change() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        assert_eq!(monitor.update(Some(PowerSource::Battery), None), []);
        assert_eq!(
            monitor.update(Some(PowerSource::Battery), None),
            [Event::PowerSourceChanged(PowerSource::Battery)]
        );
    }

    #[test]
    fn zero_percent_on_the_stand_is_not_known() {
        let mut monitor = PowerMonitor::new(PowerSource::Stand);
        // The gauge has no estimate yet.
        assert_eq!(monitor.update(Some(PowerSource::Stand), Some(0)), []);
        assert_eq!(monitor.update(None, Some(0)), []);
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), Some(5)),
            [Event::BatteryChanged(Some(5))]
        );
        assert_eq!(monitor.update(Some(PowerSource::Stand), Some(0)), []);

        // On battery, 0% is a real level.
        let mut monitor = PowerMonitor::new(PowerSource::Battery);
        assert_eq!(
            monitor.update(Some(PowerSource::Battery), Some(0)),
            [Event::BatteryChanged(Some(0))]
        );
    }

    #[test]
    fn a_source_change_and_a_level_change_give_two_events() {
        let mut monitor = PowerMonitor::new(PowerSource::Battery);
        let _ = monitor.update(Some(PowerSource::Battery), Some(20));
        let _ = monitor.update(Some(PowerSource::Stand), Some(20));
        assert_eq!(
            monitor.update(Some(PowerSource::Stand), Some(21)),
            [
                Event::PowerSourceChanged(PowerSource::Stand),
                Event::BatteryChanged(Some(21))
            ]
        );
    }
}
