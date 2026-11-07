//! The geometry of the screen: the rectangle of each area and each control.
//!
//! [`draw`](crate::draw) and [`hit_test`](crate::hit_test) both use these
//! functions, so a control is where the touch goes.
//!
//! A room page has the header, a grid of tiles, a row of scene buttons and
//! the footer. The index page has a taller header, with the weather and the
//! All lights toggle, then a grid of room buttons and the same footer.
//!
//! The panel has about 234 pixels for each inch, so 9 pixels are about 1 mm.
//! Each control that a finger taps is at least 72 pixels (about 8 mm) high.

use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::layout::{MAX_ROOMS, MAX_SCENES, MAX_TILES};

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

/// The height of the header of the index page. Below the title and the
/// status, it has the weather and the All lights toggle.
pub const INDEX_HEADER_HEIGHT: u32 = 144;
/// The height of the All lights toggle.
pub const ALL_LIGHTS_HEIGHT: u32 = 72;
/// The number of rows of room buttons on the index page. The grid has
/// [`COLUMNS`] columns.
pub const ROOM_ROWS: u32 = 3;
/// The top of the grid of room buttons.
pub const ROOMS_TOP: u32 = INDEX_HEADER_HEIGHT + GAP;
/// The height of a room button. The grid of room buttons ends one gap
/// before the footer.
pub const ROOM_HEIGHT: u32 = (FOOTER_TOP - GAP - ROOMS_TOP - (ROOM_ROWS - 1) * GAP) / ROOM_ROWS;

// The grid has a cell for each tile and each scene that a page can have.
const _: () = assert!((COLUMNS * ROWS) as usize == MAX_TILES);
const _: () = assert!(COLUMNS as usize == MAX_SCENES);
// The index grid has a cell for each room.
const _: () = assert!((COLUMNS * ROOM_ROWS) as usize == MAX_ROOMS);
// The footer buttons, the room buttons and the All lights toggle are large
// enough for a finger.
const _: () = assert!(FOOTER_HEIGHT >= 72);
const _: () = assert!(ROOM_HEIGHT >= 72);
const _: () = assert!(ALL_LIGHTS_HEIGHT >= 72);
// The All lights toggle ends before the line at the bottom of the header.
const _: () = assert!(HEADER_HEIGHT + ALL_LIGHTS_HEIGHT + 6 <= INDEX_HEADER_HEIGHT);
// The scene row ends before the footer.
const _: () = assert!(SCENES_TOP + SCENE_HEIGHT <= FOOTER_TOP);

/// Returns the area of the header.
#[must_use]
pub const fn header() -> Rectangle {
    rect(0, 0, WIDTH, HEADER_HEIGHT)
}

/// Returns the area of the header of the index page.
#[must_use]
pub const fn index_header() -> Rectangle {
    rect(0, 0, WIDTH, INDEX_HEADER_HEIGHT)
}

/// Returns the area of the weather in the header of the index page: the
/// first two columns below the title. A tap in it does nothing.
#[must_use]
pub const fn weather() -> Rectangle {
    rect(
        MARGIN,
        HEADER_HEIGHT,
        2 * CELL_WIDTH + GAP,
        ALL_LIGHTS_HEIGHT,
    )
}

/// Returns the area of the All lights toggle in the header of the index
/// page: the last column below the status.
#[must_use]
pub const fn all_lights() -> Rectangle {
    rect(
        MARGIN + 2 * (CELL_WIDTH + GAP),
        HEADER_HEIGHT,
        CELL_WIDTH,
        ALL_LIGHTS_HEIGHT,
    )
}

/// Returns the area of the room button with this index on the index page,
/// or `None` if the grid has no cell for the index.
#[must_use]
pub const fn room(index: usize) -> Option<Rectangle> {
    if index >= MAX_ROOMS {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, reason = "the index is less than 9")]
    let index = index as u32;
    let left = MARGIN + (index % COLUMNS) * (CELL_WIDTH + GAP);
    let top = ROOMS_TOP + (index / COLUMNS) * (ROOM_HEIGHT + GAP);
    Some(rect(left, top, CELL_WIDTH, ROOM_HEIGHT))
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
    fn the_areas_of_the_index_do_not_overlap_and_fit_on_the_panel() {
        let panel = Rectangle::new(Point::zero(), Size::new(WIDTH, HEIGHT));
        let mut areas = vec![index_header(), previous(), home(), next(), footer_text()];
        areas.extend((0..MAX_ROOMS).filter_map(room));
        for (index, area) in areas.iter().enumerate() {
            assert_eq!(panel.intersection(area), *area, "{area:?} is off the panel");
            for other in &areas[index + 1..] {
                assert!(
                    area.intersection(other).is_zero_sized(),
                    "{area:?} overlaps {other:?}"
                );
            }
        }
        // The weather and the toggle are in the header, below the title.
        for area in [weather(), all_lights()] {
            assert_eq!(index_header().intersection(&area), area);
            assert!(area.top_left.y.unsigned_abs() >= HEADER_HEIGHT);
        }
        assert!(weather().intersection(&all_lights()).is_zero_sized());
        assert!(bottom(room(MAX_ROOMS - 1).unwrap()) + GAP <= FOOTER_TOP);
    }

    #[test]
    fn the_grid_has_no_cell_after_the_last_tile_scene_or_room() {
        assert_eq!(tile(MAX_TILES), None);
        assert_eq!(scene(MAX_SCENES), None);
        assert_eq!(room(MAX_ROOMS), None);
    }
}
