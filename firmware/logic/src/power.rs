//! Reads the power source and the charge of the battery from the registers
//! of the charger and the fuel gauge, and decides which changes to send to
//! the core.
//!
//! - The BQ25896 charger (I2C address 0x6B) tells if USB gives power. Refer
//!   to [`source_from_bq25896`].
//! - The BQ27220 fuel gauge (I2C address 0x55) gives the charge in percent.
//!   Refer to [`level_from_bq27220`].
//! - [`PowerMonitor`] gives an event to the core only when a value changes.

use alloc::vec::Vec;

use shared::{Event, PowerSource};

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

/// The `PG_STAT` bit of REG0B: 1 if the input power is good.
const POWER_GOOD: u8 = 1 << 2;
/// The `VBUS_STAT` value of the OTG mode, in which the charger gives power to
/// the USB port.
const VBUS_OTG: u8 = 0b111;

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

/// Returns the charge in percent from the two bytes of the
/// `StateOfCharge()` command of the BQ27220, or `None` if the value is more
/// than 100.
#[must_use]
pub fn level_from_bq27220(bytes: [u8; 2]) -> Option<u8> {
    let percent = u16::from_le_bytes(bytes);
    u8::try_from(percent).ok().filter(|percent| *percent <= 100)
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerMonitor {
    /// The power source that the core has.
    source: PowerSource,
    /// A new power source that waits for a second read.
    pending: Option<PowerSource>,
    /// The charge that the core has, or `None` before the first good read.
    level: Option<u8>,
}

impl PowerMonitor {
    /// Makes a monitor. `source` is the power source that the shell gave in
    /// `Start`, so the monitor does not send it again. The first good read
    /// of the charge always gives an event.
    #[must_use]
    pub const fn new(source: PowerSource) -> Self {
        Self {
            source,
            pending: None,
            level: None,
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
    use shared::{Event, PowerSource};

    use super::{PowerMonitor, level_from_bq27220, source_from_bq25896};

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
