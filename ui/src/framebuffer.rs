//! A framebuffer in memory with 16 grey levels.

use alloc::{vec, vec::Vec};
use core::convert::Infallible;

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};

use crate::layout::{HEIGHT, WIDTH};

/// A 960×540 image in memory, with 4 bits for each pixel.
///
/// Each byte has two pixels. The high 4 bits are the pixel with the even x
/// value, and the low 4 bits are the next pixel. The rows go from the top to
/// the bottom, with no space between them. The value 0 is black and 15 is
/// white.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    bytes: Vec<u8>,
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framebuffer {
    /// The number of bytes in the framebuffer.
    pub const LEN: usize = (WIDTH * HEIGHT / 2) as usize;

    /// Makes a white framebuffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: vec![0xFF; Self::LEN],
        }
    }

    /// Returns the colour of the pixel at `point`, or `None` if the point is
    /// not on the panel.
    #[must_use]
    pub fn pixel(&self, point: Point) -> Option<Gray4> {
        let (index, high) = Self::index(point)?;
        let byte = self.bytes[index];
        let luma = if high { byte >> 4 } else { byte & 0x0F };
        Some(Gray4::new(luma))
    }

    /// Returns the bytes in the format that [`Framebuffer`] describes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the grey level (0 to 15) of each pixel, in rows from the top.
    pub fn lumas(&self) -> impl Iterator<Item = u8> + '_ {
        self.bytes.iter().flat_map(|byte| [byte >> 4, byte & 0x0F])
    }

    /// Returns the smallest rectangle that contains all the pixels that are
    /// different in `other`, or `None` if the frames are the same.
    #[must_use]
    pub fn changed_area(&self, other: &Self) -> Option<Rectangle> {
        let width = WIDTH as usize;
        crate::bounding_box(
            self.lumas()
                .zip(other.lumas())
                .enumerate()
                .filter(|(_, (old, new))| old != new)
                .filter_map(|(index, _)| {
                    let x = i32::try_from(index % width).ok()?;
                    let y = i32::try_from(index / width).ok()?;
                    Some(Point::new(x, y))
                }),
        )
    }

    /// Returns the index of the byte for `point`, and `true` if the pixel is
    /// in the high 4 bits.
    fn index(point: Point) -> Option<(usize, bool)> {
        let x = u32::try_from(point.x).ok().filter(|x| *x < WIDTH)?;
        let y = u32::try_from(point.y).ok().filter(|y| *y < HEIGHT)?;
        let offset = usize::try_from(y * WIDTH + x).ok()?;
        Some((offset / 2, x % 2 == 0))
    }

    fn set(&mut self, point: Point, color: Gray4) {
        if let Some((index, high)) = Self::index(point) {
            let byte = &mut self.bytes[index];
            *byte = if high {
                (*byte & 0x0F) | (color.luma() << 4)
            } else {
                (*byte & 0xF0) | color.luma()
            };
        }
    }
}

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH, HEIGHT)
    }
}

impl DrawTarget for Framebuffer {
    type Color = Gray4;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            self.set(point, color);
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        for point in area.points() {
            self.set(point, color);
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.bytes.fill(color.luma() << 4 | color.luma());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pixel_keeps_its_neighbour() {
        let mut frame = Framebuffer::new();
        let Ok(()) = Pixel(Point::new(10, 3), Gray4::BLACK).draw(&mut frame);
        assert_eq!(frame.pixel(Point::new(10, 3)), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(Point::new(11, 3)), Some(Gray4::WHITE));
        let Ok(()) = Pixel(Point::new(11, 3), Gray4::new(5)).draw(&mut frame);
        assert_eq!(frame.pixel(Point::new(10, 3)), Some(Gray4::BLACK));
        // Pixel (10, 3) is pixel 2890, which is in byte 1445.
        let index = 1445;
        assert_eq!(frame.as_bytes()[index], 0x05);
    }

    #[test]
    fn the_changed_area_contains_the_changed_pixels() {
        let old = Framebuffer::new();
        let mut new = old.clone();
        assert_eq!(old.changed_area(&new), None);
        let Ok(()) = Pixel(Point::new(11, 3), Gray4::BLACK).draw(&mut new);
        let Ok(()) = Pixel(Point::new(4, 20), Gray4::new(7)).draw(&mut new);
        assert_eq!(
            old.changed_area(&new),
            Some(Rectangle::new(Point::new(4, 3), Size::new(8, 18)))
        );
    }

    #[test]
    fn a_point_off_the_panel_has_no_pixel() {
        let mut frame = Framebuffer::new();
        let Ok(()) = Pixel(Point::new(960, 0), Gray4::BLACK).draw(&mut frame);
        let Ok(()) = Pixel(Point::new(-1, 0), Gray4::BLACK).draw(&mut frame);
        assert_eq!(frame.pixel(Point::new(960, 0)), None);
        assert_eq!(frame.pixel(Point::new(0, 540)), None);
        assert!(frame.lumas().all(|luma| luma == 15));
    }
}
