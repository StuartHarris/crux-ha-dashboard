//! Reads and writes the PCF8563 RTC through the I2C bus of the panel.
//!
//! The panel driver owns the I2C bus, so these functions take the panel.
//! `logic::clock::pcf8563` decodes the registers and makes the bytes of a
//! write.

use logic::clock::pcf8563::{ADDRESS, Block, FIRST_REGISTER};

use crate::panel::{Panel, Result};

/// Reads the registers from 0x00 to 0x08: the two control registers and the
/// seven time registers. Give them to `pcf8563::decode_block`.
///
/// # Errors
///
/// Returns the error of the I2C transfer.
pub fn read(panel: &mut Panel<'_>) -> Result<Block> {
    let mut block: Block = [0; 9];
    panel.read_registers(ADDRESS, FIRST_REGISTER, &mut block)?;
    Ok(block)
}

/// Writes `payload` from `pcf8563::write_payload` to the RTC in one I2C
/// transfer.
///
/// # Errors
///
/// Returns the error of the I2C transfer.
pub fn write(panel: &mut Panel<'_>, payload: &[u8; 10]) -> Result<()> {
    panel.write_registers(ADDRESS, payload)
}
