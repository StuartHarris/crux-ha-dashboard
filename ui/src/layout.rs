//! The geometry of the screen: the rectangle of each area and each control.
//!
//! [`draw`](crate::draw) and [`hit_test`](crate::hit_test) both use these
//! functions, so a control is where the touch goes.
//!
//! The panel has about 234 pixels for each inch, so 9 pixels are about 1 mm.
//! Each control that a finger taps is at least 72 pixels (about 8 mm) high.

use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::layout::{MAX_SCENES, MAX_TILES};

/// The panel width in pixels.
pub const WIDTH: u32 = 960;
/// The panel height in pixels.
pub const HEIGHT: u32 = 540;

/// The space between the edge of the panel and the content.
pub const MARGIN: u32 = 12;
/// The space between two cells.
pub const GAP: u32 = 12;

/// The height of the header, which has the page title and the status.
pub const HEADER_HEIGHT: u32 = 60;

/// The number of tile columns.
pub const COLUMNS: u32 = 3;
/// The number of tile rows.
pub const ROWS: u32 = 2;
/// The width of a tile or a scene button.
pub const CELL_WIDTH: u32 = (WIDTH - 2 * MARGIN - (COLUMNS - 1) * GAP) / COLUMNS;
/// The top of the tile grid.
pub const GRID_TOP: u32 = HEADER_HEIGHT + GAP;
/// The height of a tile.
pub const TILE_HEIGHT: u32 = 136;

/// The top of the row of scene buttons.
pub const SCENES_TOP: u32 = GRID_TOP + ROWS * (TILE_HEIGHT + GAP);
/// The height of a scene button.
pub const SCENE_HEIGHT: u32 = 72;

/// The top of the footer, which has the navigation buttons.
pub const FOOTER_TOP: u32 = SCENES_TOP + SCENE_HEIGHT + GAP;
/// The height of a navigation button.
pub const FOOTER_HEIGHT: u32 = HEIGHT - MARGIN - FOOTER_TOP;
/// The width of a navigation button.
pub const NAV_WIDTH: u32 = 160;

// The grid has a cell for each tile and each scene that a page can have.
const _: () = assert!((COLUMNS * ROWS) as usize == MAX_TILES);
const _: () = assert!(COLUMNS as usize == MAX_SCENES);
// The footer buttons are large enough for a finger.
const _: () = assert!(FOOTER_HEIGHT >= 72);
// The scene row ends before the footer.
const _: () = assert!(SCENES_TOP + SCENE_HEIGHT <= FOOTER_TOP);

/// Returns the area of the header.
#[must_use]
pub const fn header() -> Rectangle {
    rect(0, 0, WIDTH, HEADER_HEIGHT)
}

/// Returns the area of the tile with this index, or `None` if the grid has no
/// cell for the index.
#[must_use]
pub const fn tile(index: usize) -> Option<Rectangle> {
    if index >= MAX_TILES {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, reason = "the index is less than 6")]
    let index = index as u32;
    let left = MARGIN + (index % COLUMNS) * (CELL_WIDTH + GAP);
    let top = GRID_TOP + (index / COLUMNS) * (TILE_HEIGHT + GAP);
    Some(rect(left, top, CELL_WIDTH, TILE_HEIGHT))
}

/// Returns the area of the scene button with this index, or `None` if the row
/// has no cell for the index.
#[must_use]
pub const fn scene(index: usize) -> Option<Rectangle> {
    if index >= MAX_SCENES {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, reason = "the index is less than 3")]
    let index = index as u32;
    let left = MARGIN + index * (CELL_WIDTH + GAP);
    Some(rect(left, SCENES_TOP, CELL_WIDTH, SCENE_HEIGHT))
}

/// Returns the area of the button that shows the previous page.
#[must_use]
pub const fn previous() -> Rectangle {
    rect(MARGIN, FOOTER_TOP, NAV_WIDTH, FOOTER_HEIGHT)
}

/// Returns the area of the button that shows the home page.
#[must_use]
pub const fn home() -> Rectangle {
    rect(
        MARGIN + NAV_WIDTH + GAP,
        FOOTER_TOP,
        NAV_WIDTH,
        FOOTER_HEIGHT,
    )
}

/// Returns the area of the button that shows the next page.
#[must_use]
pub const fn next() -> Rectangle {
    rect(
        WIDTH - MARGIN - NAV_WIDTH,
        FOOTER_TOP,
        NAV_WIDTH,
        FOOTER_HEIGHT,
    )
}

/// Returns the area between the home button and the next button. It has the
/// page number and the time of the last update. A tap in it does nothing.
#[must_use]
pub const fn footer_text() -> Rectangle {
    let left = MARGIN + 2 * (NAV_WIDTH + GAP);
    let right = WIDTH - MARGIN - NAV_WIDTH - GAP;
    rect(left, FOOTER_TOP, right - left, FOOTER_HEIGHT)
}

/// Makes a rectangle from pixel values. All values are less than 1000, so
/// the conversion to `i32` is exact.
const fn rect(left: u32, top: u32, width: u32, height: u32) -> Rectangle {
    Rectangle::new(
        Point::new(left.cast_signed(), top.cast_signed()),
        Size::new(width, height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bottom(area: Rectangle) -> u32 {
        area.top_left.y.unsigned_abs() + area.size.height
    }

    #[test]
    fn the_areas_do_not_overlap_and_fit_on_the_panel() {
        let panel = Rectangle::new(Point::zero(), Size::new(WIDTH, HEIGHT));
        let mut areas = vec![header(), previous(), home(), next(), footer_text()];
        areas.extend((0..MAX_TILES).filter_map(tile));
        areas.extend((0..MAX_SCENES).filter_map(scene));
        for (index, area) in areas.iter().enumerate() {
            assert_eq!(panel.intersection(area), *area, "{area:?} is off the panel");
            for other in &areas[index + 1..] {
                assert!(
                    area.intersection(other).is_zero_sized(),
                    "{area:?} overlaps {other:?}"
                );
            }
        }
        assert!(bottom(header()) <= GRID_TOP);
        assert!(bottom(tile(MAX_TILES - 1).unwrap()) <= SCENES_TOP);
    }

    #[test]
    fn the_grid_has_no_cell_after_the_last_tile_or_scene() {
        assert_eq!(tile(MAX_TILES), None);
        assert_eq!(scene(MAX_SCENES), None);
    }
}
