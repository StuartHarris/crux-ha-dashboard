//! The lines and the lookup table of the extra frames of a clean refresh
//! (`ui::Refresh::Clean`).
//!
//! The panel driver (`firmware/src/panel`) sends one 2-bit code for each
//! pixel in each frame of a waveform: [`DARKEN`], [`LIGHTEN`] or [`NO_DRIVE`].
//! For the fast waveform, the driver gives each pixel one byte: the target
//! level in the high 4 bits and the previous level in the low 4 bits. A
//! lookup table of 2^16 entries converts the bytes of two pixels to their
//! two codes in one step.
//!
//! The extra frames of a clean refresh drive each pixel of an area to its
//! target, also a pixel that does not change. A pixel with the target black
//! (0) darkens, and a pixel with the target white (15) lightens. A pixel out
//! of the area gets the target [`IDLE`], which does nothing.
//!
//! This module has no hardware code, so its tests run on the host.

/// The code that darkens a pixel.
pub const DARKEN: u8 = 0b01;
/// The code that lightens a pixel.
pub const LIGHTEN: u8 = 0b10;
/// The code that does not drive a pixel.
pub const NO_DRIVE: u8 = 0b00;
/// The target that [`clean_line`] gives to a pixel out of the area. The
/// lookup table does not drive it.
pub const IDLE: u8 = 1;
/// The number of entries of the lookup table.
pub const LUT_LEN: usize = 1 << 16;

/// Returns the code of the extra frames for the byte of one pixel (the
/// target in the high 4 bits). The previous level does not matter.
#[must_use]
pub const fn clean_code(pixel: u8) -> u8 {
    match pixel >> 4 {
        0 => DARKEN,
        15 => LIGHTEN,
        _ => NO_DRIVE,
    }
}

/// Fills `lut` with the lookup table of the extra frames.
///
/// The index is the bytes of two pixels, the first pixel in the low byte.
/// The entry has the code of the first pixel in bits 0 and 1, and the code
/// of the second pixel in bits 2 and 3.
pub fn fill_clean_lut(lut: &mut [u8]) {
    for (index, entry) in lut.iter_mut().enumerate().take(LUT_LEN) {
        let [first, second] = u16::try_from(index).unwrap_or_default().to_le_bytes();
        *entry = clean_code(first) | (clean_code(second) << 2);
    }
}

/// Builds the line of one row for the extra frames.
///
/// `next_row` is a row of a `ui::Framebuffer`, with the even pixel in the
/// high 4 bits of each byte. Each pixel from `x_start` up to (not including)
/// `x_end` gets its level in `next_row` as the target. The other pixels get
/// the target [`IDLE`].
pub fn clean_line(next_row: &[u8], x_start: usize, x_end: usize, line: &mut [u8]) {
    for (x, slot) in line.iter_mut().enumerate() {
        let target = if (x_start..x_end).contains(&x) {
            level(next_row, x)
        } else {
            IDLE
        };
        *slot = target << 4;
    }
}

/// Returns the level of pixel `x` in a row of a `ui::Framebuffer`.
fn level(row: &[u8], x: usize) -> u8 {
    let byte = row.get(x / 2).copied().unwrap_or(0xFF);
    if x.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0F
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::{DARKEN, IDLE, LIGHTEN, LUT_LEN, NO_DRIVE, clean_line, fill_clean_lut};

    #[test]
    fn the_lookup_table_darkens_to_black_lightens_to_white_and_leaves_the_rest() {
        let mut lut = vec![0xFF; LUT_LEN];
        fill_clean_lut(&mut lut);
        let entry = |first: u8, second: u8| lut[usize::from(u16::from_le_bytes([first, second]))];
        let idle = IDLE << 4;
        // The previous level (the low 4 bits) does not matter.
        for previous in [0x0, 0x7, 0xF] {
            let black = previous;
            let white = 0xF0 | previous;
            let idle = idle | previous;
            assert_eq!(entry(black, idle), DARKEN, "{previous}");
            assert_eq!(entry(idle, white), LIGHTEN << 2, "{previous}");
            assert_eq!(entry(white, black), LIGHTEN | (DARKEN << 2), "{previous}");
            assert_eq!(entry(idle, idle), NO_DRIVE, "{previous}");
            // A grey target is not driven.
            assert_eq!(entry(0x70 | previous, idle), NO_DRIVE, "{previous}");
        }
    }

    #[test]
    fn a_line_has_the_targets_in_the_area_and_idle_out_of_it() {
        // Four pixels: black, white, black, white.
        let row = [0x0F, 0x0F];
        let mut line = [0xAA; 4];
        clean_line(&row, 1, 3, &mut line);
        assert_eq!(line, [IDLE << 4, 0xF0, 0x00, IDLE << 4]);

        let mut all: Vec<u8> = vec![0; 4];
        clean_line(&row, 0, 4, &mut all);
        assert_eq!(all, [0x00, 0xF0, 0x00, 0xF0]);
    }
}
