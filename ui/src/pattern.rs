//! The pattern of a tile or a button that waits for Home Assistant.
//!
//! The pattern is a grid of black dots on white. Each dot is a square of
//! [`DOT`] × [`DOT`] pixels, at the top left of a cell of [`CELL`] × [`CELL`]
//! pixels. Thus 25% of the pixels are black, and the pattern looks light
//! grey. The cells start at the top left of the panel, so the dots of all the
//! tiles are on one grid.
//!
//! The pattern has only black and white pixels, so a partial refresh can
//! draw it. A partial refresh drives only the pixels that change, for a short
//! time. When the pattern goes away, the dots can leave a small residue. Thus
//! [`Refresher`](crate::Refresher) asks for a clean refresh of the area when
//! dots of the shown frame are not in the next frame. Refer to
//! [`removed_dots`].

use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};

use crate::Framebuffer;

/// The size of a cell of the grid, in pixels.
pub const CELL: i32 = 4;

/// The size of a dot, in pixels.
pub const DOT: i32 = 2;

/// Returns `true` if the pattern has a black pixel at `point`.
#[must_use]
pub const fn is_black(point: Point) -> bool {
    point.x.rem_euclid(CELL) < DOT && point.y.rem_euclid(CELL) < DOT
}

/// Returns `true` if `frame` has a dot of the pattern in the cell with the
/// top-left pixel `cell`: the pixels of the dot are black, and the other
/// pixels of the cell are white.
fn has_dot(frame: &Framebuffer, cell: Point) -> bool {
    Rectangle::new(cell, Size::new(CELL.unsigned_abs(), CELL.unsigned_abs()))
        .points()
        .all(|point| {
            let expected = if is_black(point) {
                Gray4::BLACK
            } else {
                Gray4::WHITE
            };
            frame.pixel(point) == Some(expected)
        })
}

/// The dots of the pattern that the next frame removes. Refer to
/// [`removed_dots`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    /// The number of cells that lose their dot.
    pub count: usize,
    /// The smallest rectangle that contains these cells, or `None` if no
    /// cell loses its dot.
    pub area: Option<Rectangle>,
}

/// Returns the cells in `area` that have a dot of the pattern in `shown` and
/// no dot in `next`.
///
/// Text and icons can have a small square that looks like a dot, so a caller
/// compares the number with a limit and does not trust one dot.
#[must_use]
pub fn removed_dots(shown: &Framebuffer, next: &Framebuffer, area: Rectangle) -> Removed {
    let first = |start: i32| start.div_euclid(CELL) * CELL;
    let Some(bottom_right) = area.bottom_right() else {
        return Removed::default();
    };
    let mut count = 0;
    let mut corners: Option<(Point, Point)> = None;
    let mut y = first(area.top_left.y);
    while y <= bottom_right.y {
        let mut x = first(area.top_left.x);
        while x <= bottom_right.x {
            let cell = Point::new(x, y);
            if has_dot(shown, cell) && !has_dot(next, cell) {
                count += 1;
                let last = cell + Point::new(CELL - 1, CELL - 1);
                corners = Some(corners.map_or((cell, last), |(low, high)| {
                    (low.component_min(cell), high.component_max(last))
                }));
            }
            x += CELL;
        }
        y += CELL;
    }
    Removed {
        count,
        area: corners.map(|(low, high)| Rectangle::with_corners(low, high)),
    }
}

#[cfg(test)]
pub mod tests {
    use embedded_graphics::{pixelcolor::Gray4, prelude::*, primitives::Rectangle};

    use super::{is_black, removed_dots};
    use crate::Framebuffer;

    /// Returns a frame with the pattern in `area`.
    #[must_use]
    pub fn with_pattern(area: Rectangle) -> Framebuffer {
        let mut frame = Framebuffer::new();
        let pixels = area
            .points()
            .filter(|point| is_black(*point))
            .map(|point| Pixel(point, Gray4::BLACK));
        let Ok(()) = frame.draw_iter(pixels);
        frame
    }

    #[test]
    fn a_quarter_of_the_pixels_are_black_in_squares_of_two() {
        let area = Rectangle::new(Point::zero(), Size::new(8, 8));
        let black: usize = area.points().filter(|point| is_black(*point)).count();
        assert_eq!(black, 16);
        assert!(is_black(Point::new(1, 1)));
        assert!(!is_black(Point::new(2, 1)));
        assert!(is_black(Point::new(4, 5)));
    }

    #[test]
    fn removed_dots_counts_the_dots_that_go() {
        let area = Rectangle::new(Point::new(8, 8), Size::new(16, 16));
        let pattern = with_pattern(area);
        let white = Framebuffer::new();
        let removed = removed_dots(&pattern, &white, area);
        assert_eq!(removed.count, 16);
        assert_eq!(removed.area, Some(area));
        assert_eq!(removed_dots(&white, &pattern, area).count, 0);
        assert_eq!(removed_dots(&pattern, &pattern, area).area, None);
        // An area that is not on the grid counts each cell that it touches.
        let shifted = Rectangle::new(Point::new(9, 9), Size::new(4, 4));
        let removed = removed_dots(&pattern, &white, shifted);
        assert_eq!(removed.count, 4);
        assert_eq!(
            removed.area,
            Some(Rectangle::new(Point::new(8, 8), Size::new(8, 8)))
        );
    }
}
