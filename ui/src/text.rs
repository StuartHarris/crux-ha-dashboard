//! The fonts, and the functions that measure, fit and draw text.
//!
//! The fonts are `FreeUniversal` (`fur` regular and `fub` bold) from U8g2,
//! under the SIL Open Font License. The fonts have one bit for each pixel,
//! so the text is pure black or pure white. A partial refresh of the panel
//! shows such text cleanly. The `_tf` fonts have all the Latin-1 characters,
//! which include `°`.

use embedded_graphics::{pixelcolor::Gray4, prelude::*};
use u8g2_fonts::{
    Content, Error, FontRenderer, fonts,
    types::{FontColor, HorizontalAlignment, VerticalPosition},
};

/// The page title.
pub const TITLE: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fub25_tf>().with_ignore_unknown_chars(true);
/// The label of a tile, and the status text.
pub const LABEL: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fur20_tf>().with_ignore_unknown_chars(true);
/// A large value on a tile.
pub const VALUE_LARGE: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fub42_tf>().with_ignore_unknown_chars(true);
/// A value on a tile that is too long for [`VALUE_LARGE`].
pub const VALUE_MEDIUM: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fub30_tf>().with_ignore_unknown_chars(true);
/// A value on a tile that is too long for [`VALUE_MEDIUM`].
pub const VALUE_SMALL: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fub20_tf>().with_ignore_unknown_chars(true);
/// The detail on a tile and the time of the last update.
pub const DETAIL: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fur17_tf>().with_ignore_unknown_chars(true);
/// The text on a button.
pub const BUTTON: FontRenderer =
    FontRenderer::new::<fonts::u8g2_font_fub20_tf>().with_ignore_unknown_chars(true);

/// The text after a clipped text.
const ELLIPSIS: &str = "...";

/// Where to put a text, and how to align it.
#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    /// The point of the text that [`Anchor::vertical`] and
    /// [`Anchor::horizontal`] give.
    pub point: Point,
    /// The vertical position of the point in the text.
    pub vertical: VerticalPosition,
    /// The horizontal position of the point in the text.
    pub horizontal: HorizontalAlignment,
}

impl Anchor {
    /// The point is on the baseline at the left of the text.
    pub const fn baseline_left(point: Point) -> Self {
        Self {
            point,
            vertical: VerticalPosition::Baseline,
            horizontal: HorizontalAlignment::Left,
        }
    }

    /// The point is at the top left of the text.
    pub const fn top_left(point: Point) -> Self {
        Self {
            point,
            vertical: VerticalPosition::Top,
            horizontal: HorizontalAlignment::Left,
        }
    }

    /// The point is at the centre of the text.
    pub const fn center(point: Point) -> Self {
        Self {
            point,
            vertical: VerticalPosition::Center,
            horizontal: HorizontalAlignment::Center,
        }
    }

    /// The point is on the baseline at the right of the text.
    pub const fn baseline_right(point: Point) -> Self {
        Self {
            point,
            vertical: VerticalPosition::Baseline,
            horizontal: HorizontalAlignment::Right,
        }
    }
}

/// Returns the width of `text` in pixels.
pub fn width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions(text, Point::zero(), VerticalPosition::Baseline)
        .map_or(0, |dimensions| dimensions.advance.x.unsigned_abs())
}

/// Draws `text` with the first of `fonts` in which it fits in `max_width`.
/// If it does not fit in the last font, draws the start of it and then
/// `...`.
pub fn draw_fitted<D>(
    display: &mut D,
    fonts: &[&FontRenderer],
    text: &str,
    anchor: Anchor,
    max_width: u32,
    color: Gray4,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let Some((last, others)) = fonts.split_last() else {
        return Ok(());
    };
    if let Some(font) = others.iter().find(|font| width(font, text) <= max_width) {
        return draw(display, font, text, anchor, color);
    }
    let (start, clipped) = clip(text, max_width, |character| char_width(last, character));
    if clipped {
        draw(
            display,
            last,
            format_args!("{start}{ELLIPSIS}"),
            anchor,
            color,
        )
    } else {
        draw(display, last, start, anchor, color)
    }
}

/// Draws `content` with `font` at `anchor`.
pub fn draw<D>(
    display: &mut D,
    font: &FontRenderer,
    content: impl Content,
    anchor: Anchor,
    color: Gray4,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    match font.render_aligned(
        content,
        anchor.point,
        anchor.vertical,
        anchor.horizontal,
        FontColor::Transparent(color),
        display,
    ) {
        Err(Error::DisplayError(error)) => Err(error),
        // The fonts ignore unknown characters and draw no background, so the
        // other errors do not occur.
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Returns the start of `text` that fits in `max_width` pixels with `...`
/// after it, and `true` if the text is too long. `char_width` gives the
/// width of one character.
///
/// If the text fits, returns all of it and `false`. The function measures
/// each character once, and stops at the first character that does not fit.
/// The fonts have no kerning, so the width of a text is the sum of the
/// widths of its characters.
pub fn clip(text: &str, max_width: u32, char_width: impl Fn(char) -> u32) -> (&str, bool) {
    let room = max_width.saturating_sub(ELLIPSIS.chars().map(&char_width).sum());
    let mut total = 0_u32;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        total = total.saturating_add(char_width(character));
        if total > max_width {
            return (text[..end].trim_end(), true);
        }
        if total <= room {
            end = index + character.len_utf8();
        }
    }
    (text, false)
}

/// Returns the width of `character` in pixels.
fn char_width(font: &FontRenderer, character: char) -> u32 {
    font.get_rendered_dimensions(character, Point::zero(), VerticalPosition::Baseline)
        .map_or(0, |dimensions| dimensions.advance.x.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measures 10 pixels for each character.
    const fn mono(_: char) -> u32 {
        10
    }

    /// Returns the width of `text` with [`mono`].
    fn mono_width(text: &str) -> u32 {
        text.chars().map(mono).sum()
    }

    #[test]
    fn text_that_fits_is_not_clipped() {
        assert_eq!(clip("On 50%", 60, mono), ("On 50%", false));
        assert_eq!(clip("", 0, mono), ("", false));
    }

    #[test]
    fn long_text_is_clipped_with_room_for_the_ellipsis() {
        let (start, clipped) = clip("Thunder and rain, 12.5 °C", 190, mono);
        assert!(clipped);
        assert_eq!(start, "Thunder and rain");
        assert!(mono_width(start) + mono_width(ELLIPSIS) <= 190);
    }

    #[test]
    fn clipping_keeps_multibyte_characters_whole() {
        assert_eq!(clip("°°°°°°", 50, mono), ("°°", true));
    }

    #[test]
    fn a_width_too_small_for_the_ellipsis_keeps_nothing() {
        assert_eq!(clip("Kitchen", 20, mono), ("", true));
    }

    #[test]
    fn clipping_measures_each_character_at_most_once() {
        let calls = core::cell::Cell::new(0);
        let text = "a".repeat(10_000);
        let (start, clipped) = clip(&text, 100, |character| {
            calls.set(calls.get() + 1);
            mono(character)
        });
        assert!(clipped);
        assert_eq!(start.len(), 7);
        // Three calls for the ellipsis, and the 11 characters up to the
        // first one that does not fit.
        assert_eq!(calls.get(), 14);
    }

    #[test]
    fn the_width_of_a_text_is_the_sum_of_its_characters() {
        let text = "Thermostat 19.5 °C";
        let sum: u32 = text.chars().map(|c| char_width(&VALUE_LARGE, c)).sum();
        assert_eq!(sum, width(&VALUE_LARGE, text));
    }

    #[test]
    fn a_bold_font_is_wider_than_a_regular_font_of_the_same_size() {
        assert!(width(&VALUE_SMALL, "Kitchen") > width(&DETAIL, "Kitchen"));
        assert!(width(&VALUE_LARGE, "21.5 °C") > width(&VALUE_MEDIUM, "21.5 °C"));
    }
}
