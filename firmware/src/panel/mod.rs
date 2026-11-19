//! The driver of the ED047TC1 panel and of the GT911 touch controller of the
//! LilyGo T5 E-Paper S3 Pro Lite.
//!
//! # Source and licence
//!
//! This module is a copy of part of the crate `lilygo-t5s3paperpro` 1.1.0
//! (<https://github.com/azw413/lilygo-t5s3paperpro-rs>, commit `c9cf0be`), by
//! Andrew Whaley and Frido Koch. That crate is a fork of
//! `fridolin-koch/lilygo-epd47-rs`, and it follows the vendor firmware
//! `Xinyuan-LilyGO/T5S3-4.7-e-paper-PRO`. Its licence is the GNU General
//! Public License version 3, which is in `LICENSE` in this directory. This
//! copy has the same licence.
//!
//! The changes from the upstream crate:
//!
//! - It builds with esp-hal 1.2 (RMT, DMA and delays).
//! - It draws a `ui::Framebuffer`, which keeps the even pixel of each byte in
//!   the high 4 bits. The upstream framebuffer keeps it in the low 4 bits.
//! - It has no framebuffer of its own. The caller keeps the frame that the
//!   panel shows.
//! - The waveform table is in PSRAM, so the Wi-Fi driver keeps the internal
//!   heap.
//! - The refresh functions are `async`. They yield to the executor after each
//!   frame of the waveform, so other tasks can run during a refresh.
//! - The touch read gives the raw point, and it tells a sample with no new
//!   data from a sample with no finger. `logic::touch` maps the point to the
//!   panel. The start of the touch controller is a separate `async` call, so
//!   the caller decides when to try it again.
//! - A failed power-on turns the high voltages off again.
//! - The GT911 INT pin can wake the chip from light sleep
//!   ([`Panel::touch_wake_arm`]).
//! - It has no SD card, battery, RTC, rotation or deep sleep code. The
//!   caller reads and writes the other devices on the I2C bus, for example
//!   the BQ25896 charger, with [`Panel::read_registers`] and
//!   [`Panel::write_registers`].
//!
//! The code stays close to upstream, to make a diff easy. For this reason,
//! the pedantic lints are off in this module.

#![allow(clippy::pedantic, clippy::nursery)]

mod ed047tc1;
mod rmt;

use allocator_api2::vec::Vec;
use esp_alloc::ExternalMemory;

use embassy_futures::yield_now;
use logic::{touch::Sample, waveform};
use ui::Framebuffer;

pub use ed047tc1::{GT911_ADDR_LOW, PCA9535_ADDR, PinConfig, TPS65185_ADDR};

/// Makes the [`PinConfig`] from the fields of `esp_hal::peripherals::Peripherals`.
///
/// The pins are the pins of board H752-01 (`docs/pinmap.md` in the vendor
/// repository, branch `H752-01`).
#[macro_export]
macro_rules! panel_pins {
    ($peripherals:expr) => {
        $crate::panel::PinConfig {
            d0: $peripherals.GPIO5,
            d1: $peripherals.GPIO6,
            d2: $peripherals.GPIO7,
            d3: $peripherals.GPIO15,
            d4: $peripherals.GPIO16,
            d5: $peripherals.GPIO17,
            d6: $peripherals.GPIO18,
            d7: $peripherals.GPIO8,
            i2c_sda: $peripherals.GPIO39,
            i2c_scl: $peripherals.GPIO40,
            le: $peripherals.GPIO42,
            sth: $peripherals.GPIO41,
            ckh: $peripherals.GPIO4,
            ckv: $peripherals.GPIO48,
            stv: $peripherals.GPIO45,
            touch_int: $peripherals.GPIO3,
            touch_rst: $peripherals.GPIO9,
        }
    };
}

/// An error from the panel driver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error {
    /// The RMT channel failed.
    Rmt(esp_hal::rmt::Error),
    /// The RMT configuration is not valid.
    RmtConfig(esp_hal::rmt::ConfigError),
    /// The DMA transfer failed.
    Dma(esp_hal::dma::DmaError),
    /// The DMA buffer is not valid.
    DmaBuffer(esp_hal::dma::DmaBufError),
    /// The I2C transfer failed.
    I2c(esp_hal::i2c::master::Error),
    /// The I2C configuration is not valid.
    I2cConfig(esp_hal::i2c::master::ConfigError),
    /// The LCD configuration is not valid.
    I8080Config(esp_hal::lcd_cam::lcd::i8080::ConfigError),
    /// The power supply did not report that it is ready.
    PowerTimeout,
    /// A peripheral was not available, because an earlier transfer failed.
    Missing,
    /// The touch controller did not answer as a GT911.
    TouchInitFailed,
    /// The touch controller did not start yet.
    TouchNotReady,
}

/// A result of the panel driver.
pub type Result<T> = core::result::Result<T, Error>;

/// A rectangle on the panel, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

const WIDTH: u16 = 960;
const HEIGHT: u16 = 540;
const BOUNDS: Area = Area {
    x: 0,
    y: 0,
    width: WIDTH,
    height: HEIGHT,
};
const CONTRAST_CYCLES_4BPP: &[u16; 15] = &[
    30, 30, 20, 20, 30, 30, 30, 40, 40, 50, 50, 50, 100, 200, 300,
];
const BYTES_PER_LINE: usize = WIDTH as usize / 4;
const LINE_BYTES_4BPP: usize = WIDTH as usize / 2;
const LINE_BYTES_DIFFERENCE: usize = WIDTH as usize;
const DRAW_IMAGE_FRAME_COUNT: usize = 15;
const DU_FRAME_TIMES: [u16; 5] = [1000, 1000, 1000, 1000, 1000];
const DU_LUT_PHASE: [[u8; 4]; 16] = [
    [0x15, 0x55, 0x55, 0x55],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0x00, 0x00, 0x00, 0x00],
    [0xAA, 0xAA, 0xAA, 0xA8],
];
/// The frame times of the extra frames of [`Panel::clean`].
const CLEAN_FRAME_TIMES: [u16; 2] = [1000, 1000];

/// The panel and its touch controller.
pub struct Panel<'a> {
    epd: ed047tc1::Ed047tc1<'a>,
    skipping: u16,
    /// The waveform lookup table (64 KiB). The driver keeps it, so a refresh
    /// does not allocate. It is in PSRAM, because the Wi-Fi driver needs the
    /// internal heap.
    lut: Vec<u8, ExternalMemory>,
}

impl<'a> Panel<'a> {
    /// Sets up the panel bus, the IO expander and the touch reset.
    pub fn new(
        pins: PinConfig<'a>,
        i2c: esp_hal::peripherals::I2C0<'a>,
        dma: esp_hal::peripherals::DMA_CH0<'a>,
        lcd_cam: esp_hal::peripherals::LCD_CAM<'a>,
        rmt: esp_hal::peripherals::RMT<'a>,
    ) -> Result<Self> {
        Ok(Self {
            epd: ed047tc1::Ed047tc1::new(pins, i2c, dma, lcd_cam, rmt)?,
            skipping: 0,
            lut: {
                let mut lut = Vec::new_in(ExternalMemory);
                lut.resize(1 << 16, 0);
                lut
            },
        })
    }

    /// Turns on the high voltages of the panel. Do this before a refresh.
    ///
    /// If power-on fails, the driver turns the high voltages off again, so
    /// the PMIC and the IO expander are in a safe state.
    pub fn power_on(&mut self) -> Result<()> {
        self.epd.power_on()
    }

    /// Turns off the high voltages of the panel. The panel keeps its image.
    pub fn power_off(&mut self) -> Result<()> {
        self.epd.power_off()
    }

    /// Reads one sample of the touch controller, with the raw point of the
    /// first finger. Returns [`Error::TouchNotReady`] until
    /// [`Panel::init_touch`] succeeds.
    pub fn touch_sample(&mut self) -> Result<Sample<(u16, u16)>> {
        self.epd.touch_sample()
    }

    /// Returns `true` if the touch controller started and holds INT low,
    /// because it has a sample that the host did not read.
    pub fn touch_pending(&self) -> bool {
        self.epd.touch_pending()
    }

    /// Writes 0 to the status register of the GT911 (0x814E), and so drops
    /// a sample that the host did not read. Returns
    /// [`Error::TouchNotReady`] until [`Panel::init_touch`] succeeds.
    pub fn touch_clear_status(&mut self) -> Result<()> {
        self.epd.touch_clear_status()
    }

    /// Makes the GT911 INT pin (GPIO3) a light-sleep wake source. A low
    /// level wakes the chip. Call [`Panel::touch_wake_disarm`] after the
    /// sleep. Arm it only after the touch controller started, because INT
    /// selects the I2C address of the GT911 during its reset.
    pub fn touch_wake_arm(&mut self) {
        self.epd.touch_wake_arm();
    }

    /// Removes the wake source of [`Panel::touch_wake_arm`], and returns
    /// `true` if the INT pin ended the last sleep. Call it with interrupts
    /// off, directly after the sleep.
    pub fn touch_wake_disarm(&mut self) -> bool {
        self.epd.touch_wake_disarm()
    }

    /// Resets the touch controller and returns its resolution, as (x, y).
    /// The reset takes about 40 ms, or 80 ms if the controller is at its
    /// second address.
    pub async fn init_touch(&mut self) -> Result<(u16, u16)> {
        self.epd.init_touch().await
    }

    /// Writes the I2C addresses that answer to `found`, and returns their
    /// number.
    pub fn scan(&mut self, found: &mut [u8; 16]) -> usize {
        self.epd.scan(found)
    }

    /// Reads `buffer.len()` registers of the I2C device at `address`, from
    /// `register` on. The panel owns the I2C bus, so the other devices on the
    /// bus (for example the BQ25896 charger) use this function.
    pub fn read_registers(&mut self, address: u8, register: u8, buffer: &mut [u8]) -> Result<()> {
        self.epd.read_registers(address, register, buffer)
    }

    /// Writes `payload` to the I2C device at `address`. The first byte of
    /// `payload` is the first register, and the other bytes are the values.
    pub fn write_registers(&mut self, address: u8, payload: &[u8]) -> Result<()> {
        self.epd.write_registers(address, payload)
    }

    /// Makes the whole panel white. The panel flashes.
    pub async fn clear(&mut self) -> Result<()> {
        self.clear_cycles(BOUNDS, 4, 50).await
    }

    /// Draws `frame` with all 16 grey levels. The panel flashes, and the
    /// refresh clears ghost images.
    pub async fn full(&mut self, frame: &Framebuffer) -> Result<()> {
        self.clear().await?;
        self.draw(frame).await
    }

    /// Draws `area` of `next` with the fast black-and-white waveform. `shown`
    /// is the frame that the panel shows now. The waveform drives a pixel
    /// only if it changes to black or to white.
    pub async fn partial(
        &mut self,
        shown: &Framebuffer,
        next: &Framebuffer,
        area: Area,
    ) -> Result<()> {
        update_du_lut(&mut self.lut, &DU_LUT_PHASE);
        self.drive(area, &DU_FRAME_TIMES, |y, area, line| {
            let (next_line, shown_line) = (row(next, y), row(shown, y));
            build_difference_line(next_line, shown_line, area, line)
        })
        .await
    }

    /// Draws `area` of `next` as [`Panel::partial`] does. Then drives each
    /// pixel of `dots` to its level (black or white) for
    /// [`CLEAN_FRAME_TIMES`]. The panel does not flash. The extra frames
    /// remove the residue of small dots that the fast waveform leaves, for
    /// example when a tile leaves the pending pattern (`ui::Refresh::Clean`).
    /// `logic::waveform` builds the lines and the lookup table of the extra
    /// frames.
    pub async fn clean(
        &mut self,
        shown: &Framebuffer,
        next: &Framebuffer,
        area: Area,
        dots: Area,
    ) -> Result<()> {
        self.partial(shown, next, area).await?;
        waveform::fill_clean_lut(&mut self.lut);
        self.drive(dots, &CLEAN_FRAME_TIMES, |y, dots, line| {
            let x_start = dots.x as usize;
            waveform::clean_line(row(next, y), x_start, x_start + dots.width as usize, line);
            true
        })
        .await
    }

    /// Sends one frame for each of `frame_times` with the lookup table that
    /// the caller prepared. `build` builds the line of row `y` in `area`, and
    /// returns `false` if the row has nothing to drive. The rows out of
    /// `area` are not driven.
    async fn drive(
        &mut self,
        area: Area,
        frame_times: &[u16],
        mut build: impl FnMut(u16, Area, &mut [u8; LINE_BYTES_DIFFERENCE]) -> bool,
    ) -> Result<()> {
        let area = clip(area);
        if area.width == 0 || area.height == 0 {
            return Ok(());
        }
        let mut line = [0u8; LINE_BYTES_DIFFERENCE];
        let mut output = [0u8; BYTES_PER_LINE];
        for &output_time in frame_times {
            self.skipping = 0;
            self.epd.frame_start()?;
            for y in 0..HEIGHT {
                if y < area.y || y >= area.y + area.height || !build(y, area, &mut line) {
                    self.row_skip(output_time)?;
                    continue;
                }
                prepare_dma_difference_buffer(&line, &self.lut, &mut output);
                self.epd.set_buffer(&output)?;
                self.row_write(output_time)?;
            }
            if self.skipping == 0 {
                self.row_write(output_time)?;
            }
            self.epd.frame_end()?;
            yield_now().await;
        }
        Ok(())
    }

    /// Draws `frame` on a white panel with all 16 grey levels.
    async fn draw(&mut self, frame: &Framebuffer) -> Result<()> {
        self.lut.fill(0x55);
        let mut line = [0u8; LINE_BYTES_4BPP];
        let mut output = [0u8; BYTES_PER_LINE];
        for (k, &time) in CONTRAST_CYCLES_4BPP.iter().enumerate() {
            update_lut(&mut self.lut, k);
            self.skipping = 0;
            self.epd.frame_start()?;
            for y in 0..HEIGHT as usize {
                let row = &frame.as_bytes()[y * LINE_BYTES_4BPP..(y + 1) * LINE_BYTES_4BPP];
                if row.iter().all(|byte| *byte == 0xFF) {
                    self.row_skip(time)?;
                    continue;
                }
                // The upstream lookup table needs the even pixel in the low
                // 4 bits.
                for (to, from) in line.iter_mut().zip(row) {
                    *to = from.rotate_left(4);
                }
                prepare_dma_buffer(&line, &self.lut, &mut output);
                self.epd.set_buffer(&output)?;
                self.row_write(time)?;
            }
            if self.skipping == 0 {
                self.row_write(time)?;
            }
            self.epd.frame_end()?;
            yield_now().await;
        }
        Ok(())
    }

    async fn clear_cycles(&mut self, area: Area, cycles: u16, cycle_time: u16) -> Result<()> {
        for _ in 0..cycles {
            for _ in 0..4 {
                self.push_pixels(area, cycle_time, 0)?;
                yield_now().await;
            }
            for _ in 0..4 {
                self.push_pixels(area, cycle_time, 1)?;
                yield_now().await;
            }
        }
        Ok(())
    }

    fn push_pixels(&mut self, area: Area, time: u16, color: u16) -> Result<()> {
        let mut row = [0u8; BYTES_PER_LINE];
        for i in 0..area.width {
            let pos = i + area.x % 4;
            let mask = match color {
                1 => 0b1010_1010,
                _ => 0b0101_0101,
            } & (0b0000_0011 << (2 * (pos % 4)));
            row[(area.x / 4 + pos / 4) as usize] |= mask;
        }
        line_buffer_reorder(&mut row);
        self.epd.frame_start()?;
        for i in 0..HEIGHT {
            if i < area.y {
                self.row_skip(time)?;
                continue;
            }
            if i == area.y {
                self.epd.set_buffer(&row)?;
                self.row_write(time)?;
                continue;
            }
            if i >= area.y + area.height {
                self.row_skip(time)?;
                continue;
            }
            self.row_write(time)?;
        }
        self.row_write(time)?;
        self.epd.frame_end()
    }

    fn row_skip(&mut self, output_time: u16) -> Result<()> {
        match self.skipping {
            0 => {
                self.epd.set_buffer(&[0u8; BYTES_PER_LINE])?;
                self.epd.output_row(output_time)?;
            }
            1 => self.epd.output_row(10)?,
            _ => self.epd.skip()?,
        }
        self.skipping = self.skipping.saturating_add(1);
        Ok(())
    }

    fn row_write(&mut self, output_time: u16) -> Result<()> {
        self.skipping = 0;
        self.epd.output_row(output_time)
    }
}

/// Returns the part of `area` that is on the panel.
fn clip(area: Area) -> Area {
    let x = area.x.min(WIDTH);
    let y = area.y.min(HEIGHT);
    let max_x = area.x.saturating_add(area.width).min(WIDTH);
    let max_y = area.y.saturating_add(area.height).min(HEIGHT);
    Area {
        x,
        y,
        width: max_x.saturating_sub(x),
        height: max_y.saturating_sub(y),
    }
}

fn line_buffer_reorder(data: &mut [u8]) {
    for chunk in data.chunks_exact_mut(4) {
        let val = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let swapped = (val >> 16) | ((val & 0x0000_FFFF) << 16);
        chunk.copy_from_slice(&swapped.to_le_bytes());
    }
}

/// Converts a line of 4-bit pixels (even pixel in the low 4 bits) to the
/// 2-bit codes of one waveform frame. Each lookup converts 4 pixels.
fn prepare_dma_buffer(line: &[u8; LINE_BYTES_4BPP], lut: &[u8], output: &mut [u8; BYTES_PER_LINE]) {
    for (index, out) in output.iter_mut().enumerate() {
        let pixels = u16::from_le_bytes([line[2 * index], line[2 * index + 1]]);
        *out = lut[pixels as usize];
    }
}

/// Converts a difference line (one byte for each pixel: the target in the
/// high 4 bits, the previous level in the low 4 bits) to the 2-bit codes of
/// the fast waveform.
fn prepare_dma_difference_buffer(
    line: &[u8; LINE_BYTES_DIFFERENCE],
    lut: &[u8],
    output: &mut [u8; BYTES_PER_LINE],
) {
    for (index, out) in output.iter_mut().enumerate() {
        let first = u16::from_le_bytes([line[4 * index], line[4 * index + 1]]) as usize;
        let second = u16::from_le_bytes([line[4 * index + 2], line[4 * index + 3]]) as usize;
        *out = lut[first] | (lut[second] << 4);
    }
}

/// Updates the lookup table of the 16-level waveform for frame `k`.
fn update_lut(conversion_lut: &mut [u8], k: usize) {
    let k = DRAW_IMAGE_FRAME_COUNT - k;
    // Reset the pixels that are not to be darkened any longer in this frame.
    for l in (k..1 << 16).step_by(16) {
        conversion_lut[l] &= 0xFC;
    }
    for l in ((k << 4)..(1 << 16)).step_by(1 << 8) {
        for p in 0..16 {
            conversion_lut[l + p] &= 0xF3;
        }
    }
    for l in ((k << 8)..(1 << 16)).step_by(1 << 12) {
        for p in 0..(1 << 8) {
            conversion_lut[l + p] &= 0xCF;
        }
    }
    for entry in conversion_lut.iter_mut().take((k + 1) << 12).skip(k << 12) {
        *entry &= 0x3F;
    }
}

/// Builds the lookup table of the fast waveform.
fn update_du_lut(conversion_lut: &mut [u8], phase: &[[u8; 4]; 16]) {
    for (to, packed) in phase.iter().enumerate() {
        for (from_packed, value) in packed.iter().enumerate() {
            let index = (to << 4) | (from_packed * 4);
            conversion_lut[index] = (value >> 6) & 0x03;
            conversion_lut[index + 1] = (value >> 4) & 0x03;
            conversion_lut[index + 2] = (value >> 2) & 0x03;
            conversion_lut[index + 3] = value & 0x03;
        }
    }
    for outer in (0..=0xFF).rev() {
        let outer_result = conversion_lut[outer] << 2;
        let base = outer << 8;
        conversion_lut.copy_within(0..0x100, base);
        for entry in &mut conversion_lut[base..base + 0x100] {
            *entry |= outer_result;
        }
    }
}

/// Builds the difference line of row `next_line` against `shown_line`. A
/// pixel outside `area` keeps its level. Returns `true` if a pixel changes.
fn build_difference_line(
    next_line: &[u8],
    shown_line: &[u8],
    area: Area,
    line: &mut [u8; LINE_BYTES_DIFFERENCE],
) -> bool {
    let mut dirty = false;
    let x_start = area.x as usize;
    let x_end = x_start + area.width as usize;
    for (x, slot) in line.iter_mut().enumerate() {
        let previous = nibble_at(shown_line, x);
        let target = if (x_start..x_end).contains(&x) {
            nibble_at(next_line, x)
        } else {
            previous
        };
        dirty |= target != previous;
        *slot = (target << 4) | previous;
    }
    dirty
}

/// Returns row `y` of `frame`.
fn row(frame: &Framebuffer, y: u16) -> &[u8] {
    let start = y as usize * LINE_BYTES_4BPP;
    &frame.as_bytes()[start..start + LINE_BYTES_4BPP]
}

/// Returns the level of pixel `x` in a row of a `ui::Framebuffer`, which
/// keeps the even pixel in the high 4 bits.
fn nibble_at(line: &[u8], x: usize) -> u8 {
    let value = line[x / 2];
    if x.is_multiple_of(2) {
        value >> 4
    } else {
        value & 0x0F
    }
}
