//! Draws the dashboard's view model on a 960×540 panel with 16 grey levels.
//!
//! The simulator and the firmware use the same code, so the simulator shows what
//! the panel shows. The view model has all the text, so this crate only draws.

#![cfg_attr(not(test), no_std)]

use embedded_graphics::{
    mono_font::{MonoTextStyle, iso_8859_1::FONT_10X20},
    pixelcolor::Gray4,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::Text,
};
use shared::{TileState, ViewModel};

/// The panel width in pixels.
pub const WIDTH: u32 = 960;
/// The panel height in pixels.
pub const HEIGHT: u32 = 540;

/// The number of tile columns in the grid.
const COLUMNS: u32 = 3;
/// The space around and between the cells, in pixels.
const GAP: u32 = 16;
/// The width of a tile or a scene button.
const CELL_WIDTH: u32 = (WIDTH - GAP * (COLUMNS + 1)) / COLUMNS;
/// The top of the tile grid.
const GRID_TOP: u32 = 64;
/// The height of a tile.
const TILE_HEIGHT: u32 = 140;
/// The top of the scene row.
const SCENES_TOP: u32 = GRID_TOP + 2 * (TILE_HEIGHT + GAP);
/// The height of a scene button.
const SCENE_HEIGHT: u32 = 60;
/// The width of one character of the font.
const CHAR_WIDTH: u32 = 10;

/// Draws the view model on the display.
///
/// The page title and the status are at the top. The tiles are in a grid of
/// three columns and two rows. The scene buttons are in one row under the
/// grid. The page number and the time of the last update are at the bottom.
///
/// # Errors
///
/// Returns the error of the display if a draw operation fails.
pub fn draw<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    display.clear(Gray4::WHITE)?;
    let black = MonoTextStyle::new(&FONT_10X20, Gray4::BLACK);

    Text::new(&view.title, point(GAP, 40), black).draw(display)?;
    Text::new(&view.status, point(right_aligned(&view.status), 40), black).draw(display)?;

    for (index, tile) in (0..).zip(&view.tiles) {
        let left = GAP + (index % COLUMNS) * (CELL_WIDTH + GAP);
        let top = GRID_TOP + (index / COLUMNS) * (TILE_HEIGHT + GAP);
        let area = Rectangle::new(point(left, top), Size::new(CELL_WIDTH, TILE_HEIGHT));

        let (style, ink) = match tile.state {
            TileState::On => (PrimitiveStyle::with_fill(Gray4::BLACK), Gray4::WHITE),
            TileState::Unavailable => (PrimitiveStyle::with_fill(Gray4::new(0xC)), Gray4::BLACK),
            TileState::Off | TileState::Value | TileState::Unknown => {
                (PrimitiveStyle::with_stroke(Gray4::BLACK, 2), Gray4::BLACK)
            }
        };
        area.into_styled(style).draw(display)?;

        let text = MonoTextStyle::new(&FONT_10X20, ink);
        draw_clipped(display, &tile.label, point(left + 16, top + 40), text)?;
        draw_clipped(display, &tile.text, point(left + 16, top + 100), text)?;
    }

    for (index, scene) in (0..).zip(&view.scenes) {
        let left = GAP + index * (CELL_WIDTH + GAP);
        Rectangle::new(point(left, SCENES_TOP), Size::new(CELL_WIDTH, SCENE_HEIGHT))
            .into_styled(PrimitiveStyle::with_stroke(Gray4::BLACK, 2))
            .draw(display)?;
        draw_clipped(
            display,
            &scene.label,
            point(left + 16, SCENES_TOP + 38),
            black,
        )?;
    }

    Text::new(&view.page_label, point(GAP, HEIGHT - 16), black).draw(display)?;
    Text::new(
        &view.updated,
        point(right_aligned(&view.updated), HEIGHT - 16),
        black,
    )
    .draw(display)?;

    Ok(())
}

/// The largest number of characters on one line in a cell.
const CELL_CHARS: usize = ((CELL_WIDTH - 32) / CHAR_WIDTH) as usize;

/// Draws one line of text in a cell. If the text is too long for the cell,
/// draws the start of it and then `...`.
fn draw_clipped<D>(
    display: &mut D,
    text: &str,
    position: Point,
    style: MonoTextStyle<'_, Gray4>,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let (start, clipped) = clip(text, CELL_CHARS);
    let end = Text::new(start, position, style).draw(display)?;
    if clipped {
        Text::new("...", end, style).draw(display)?;
    }
    Ok(())
}

/// Returns the start of `text` that fits in `max_chars` characters with
/// `...` after it, and `true` if the text is too long.
fn clip(text: &str, max_chars: usize) -> (&str, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    let keep = max_chars.saturating_sub(3);
    let end = text
        .char_indices()
        .nth(keep)
        .map_or(text.len(), |(index, _)| index);
    (text[..end].trim_end(), true)
}

/// Returns the left edge of a text that ends at the right margin.
fn right_aligned(text: &str) -> u32 {
    let width = u32::try_from(text.chars().count()).unwrap_or(0) * CHAR_WIDTH;
    WIDTH.saturating_sub(GAP + width)
}

/// Converts a pixel position to a point.
fn point(x: u32, y: u32) -> Point {
    Point::new(
        i32::try_from(x).unwrap_or(i32::MAX),
        i32::try_from(y).unwrap_or(i32::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::{CELL_CHARS, clip};

    #[test]
    fn text_that_fits_is_not_clipped() {
        assert_eq!(clip("On 50%", CELL_CHARS), ("On 50%", false));
        let full = "a".repeat(CELL_CHARS);
        assert_eq!(clip(&full, CELL_CHARS), (full.as_str(), false));
    }

    #[test]
    fn long_text_is_clipped_with_room_for_the_ellipsis() {
        let (start, clipped) = clip("Thunder and rain, 12.5 °C and more", 20);
        assert!(clipped);
        assert_eq!(start, "Thunder and rain,");
        assert!(start.chars().count() + 3 <= 20);
    }

    #[test]
    fn clipping_keeps_multibyte_characters_whole() {
        assert_eq!(clip("°°°°°°", 5), ("°°", true));
    }
}
