//! Draws the view model.

use embedded_graphics::{
    pixelcolor::Gray4,
    prelude::*,
    primitives::{
        Circle, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle,
        StrokeAlignment, Triangle,
    },
};
use shared::{SceneView, TileKind, TileState, TileView, ViewModel, layout::MAX_SCENES};

use crate::{
    layout::{self, HEADER_HEIGHT, MARGIN, WIDTH},
    text::{self, Anchor, BUTTON, DETAIL, LABEL, TITLE, VALUE_LARGE, VALUE_MEDIUM, VALUE_SMALL},
};

const BLACK: Gray4 = Gray4::BLACK;
const WHITE: Gray4 = Gray4::WHITE;

/// The radius of the corners of tiles and buttons.
const RADIUS: u32 = 12;
/// The space between the edge of a tile and its text.
const PADDING: u32 = 16;
/// The diameter of the on/off indicator on a light or switch tile.
const INDICATOR: u32 = 26;
/// The length of a dash on the border of an unavailable tile.
const DASH: u32 = 14;
/// The width of the status text in the header.
const STATUS_WIDTH: u32 = 400;

/// Draws the view model on the display.
///
/// The screen has these areas, from the top:
///
/// - The header: the page title on the left and the status on the right.
/// - The tiles, in a grid of three columns and two rows.
/// - The scene buttons, in one row.
/// - The footer: the previous, home and next buttons, the page number and
///   the time of the last update.
///
/// The screen is black and white, so a partial refresh of the panel is clean.
/// A light or a switch that is on is a black tile with white text. A tile
/// that Home Assistant cannot reach has a dashed border.
///
/// # Errors
///
/// Returns the error of the display if a draw operation fails.
pub fn draw<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    display.clear(WHITE)?;
    draw_header(view, display)?;
    for (index, tile) in view.tiles.iter().enumerate() {
        if let Some(area) = layout::tile(index) {
            draw_tile(tile, area, display)?;
        }
    }
    for (index, scene) in view.scenes.iter().enumerate().take(MAX_SCENES) {
        if let Some(area) = layout::scene(index) {
            draw_scene(scene, area, display)?;
        }
    }
    draw_footer(view, display)
}

fn draw_header<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let baseline = 42;
    let title_width = WIDTH - 3 * MARGIN - STATUS_WIDTH;
    text::draw_fitted(
        display,
        &[&TITLE],
        &view.title,
        Anchor::baseline_left(point(MARGIN, baseline)),
        title_width,
        BLACK,
    )?;
    text::draw_fitted(
        display,
        &[&LABEL, &DETAIL],
        &view.status,
        Anchor::baseline_right(point(WIDTH - MARGIN, baseline)),
        STATUS_WIDTH,
        BLACK,
    )?;
    Rectangle::new(
        point(MARGIN, HEADER_HEIGHT - 3),
        Size::new(WIDTH - 2 * MARGIN, 2),
    )
    .into_styled(PrimitiveStyle::with_fill(BLACK))
    .draw(display)
}

fn draw_tile<D>(tile: &TileView, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let toggle = matches!(tile.kind, TileKind::Light | TileKind::Switch);
    let on = toggle && tile.state == TileState::On;
    let ink = if on { WHITE } else { BLACK };
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));

    match tile.state {
        _ if on => shape
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(display)?,
        TileState::Unavailable => draw_dashed_border(area, display)?,
        _ => shape
            .into_styled(border(if toggle { 4 } else { 2 }))
            .draw(display)?,
    }

    let left = area.top_left.x.unsigned_abs() + PADDING;
    let top = area.top_left.y.unsigned_abs();
    let inner = area.size.width - 2 * PADDING;

    let label_width = if toggle {
        inner - INDICATOR - PADDING
    } else {
        inner
    };
    text::draw_fitted(
        display,
        &[&LABEL],
        &tile.label,
        Anchor::top_left(point(left, top + 12)),
        label_width,
        ink,
    )?;

    if toggle && tile.state != TileState::Unavailable {
        let indicator = Circle::new(point(left + inner - INDICATOR, top + 12), INDICATOR);
        let style = if on {
            PrimitiveStyle::with_fill(WHITE)
        } else {
            PrimitiveStyle::with_stroke(BLACK, 3)
        };
        indicator.into_styled(style).draw(display)?;
    }

    // A value with a detail line uses a smaller font, so both lines fit.
    let (fonts, value_baseline): (&[_], _) = if tile.state == TileState::Unavailable {
        (&[&LABEL], 112)
    } else if tile.detail.is_empty() {
        (&[&VALUE_LARGE, &VALUE_MEDIUM, &VALUE_SMALL], 112)
    } else {
        (&[&VALUE_MEDIUM, &VALUE_SMALL], 86)
    };
    text::draw_fitted(
        display,
        fonts,
        &tile.text,
        Anchor::baseline_left(point(left, top + value_baseline)),
        inner,
        ink,
    )?;
    if !tile.detail.is_empty() {
        text::draw_fitted(
            display,
            &[&LABEL, &DETAIL],
            &tile.detail,
            Anchor::baseline_left(point(left, top + 120)),
            inner,
            ink,
        )?;
    }
    Ok(())
}

/// Draws a dashed border on the edge of `area`.
fn draw_dashed_border<D>(area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let style = PrimitiveStyle::with_fill(BLACK);
    let thickness = 3;
    let left = area.top_left.x.unsigned_abs();
    let top = area.top_left.y.unsigned_abs();
    let right = left + area.size.width - thickness;
    let bottom = top + area.size.height - thickness;

    let mut x = left;
    while x < right {
        let length = DASH.min(right + thickness - x);
        for y in [top, bottom] {
            Rectangle::new(point(x, y), Size::new(length, thickness))
                .into_styled(style)
                .draw(display)?;
        }
        x += 2 * DASH;
    }
    let mut y = top;
    while y < bottom {
        let length = DASH.min(bottom + thickness - y);
        for x in [left, right] {
            Rectangle::new(point(x, y), Size::new(thickness, length))
                .into_styled(style)
                .draw(display)?;
        }
        y += 2 * DASH;
    }
    Ok(())
}

fn draw_scene<D>(scene: &SceneView, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    draw_button(&scene.label, area, false, display)
}

/// Draws a button with a border, and its text in the centre.
fn draw_button<D>(
    label: &str,
    area: Rectangle,
    filled: bool,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));
    let (style, ink) = if filled {
        (PrimitiveStyle::with_fill(BLACK), WHITE)
    } else {
        (border(3), BLACK)
    };
    shape.into_styled(style).draw(display)?;
    text::draw_fitted(
        display,
        &[&BUTTON, &DETAIL],
        label,
        Anchor::center(area.center()),
        area.size.width - 2 * PADDING,
        ink,
    )
}

fn draw_footer<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    draw_arrow_button(layout::previous(), false, display)?;
    // The home button is black on the home page.
    draw_button("Home", layout::home(), view.path == "/", display)?;
    draw_arrow_button(layout::next(), true, display)?;

    let area = layout::footer_text();
    let center = area.center();
    let quarter = area.size.height.cast_signed() / 4;
    let width = area.size.width;
    text::draw_fitted(
        display,
        &[&BUTTON],
        &view.page_label,
        Anchor::center(center - Point::new(0, quarter)),
        width,
        BLACK,
    )?;
    text::draw_fitted(
        display,
        &[&LABEL, &DETAIL],
        &view.updated,
        Anchor::center(center + Point::new(0, quarter)),
        width,
        BLACK,
    )
}

/// Draws a navigation button with an arrow: to the right if `forward` is
/// `true`, else to the left.
fn draw_arrow_button<D>(area: Rectangle, forward: bool, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS))
        .into_styled(border(3))
        .draw(display)?;
    let center = area.center();
    let (half_width, half_height) = (16, 20);
    let (tip, back) = if forward {
        (half_width, -half_width)
    } else {
        (-half_width, half_width)
    };
    Triangle::new(
        center + Point::new(tip, 0),
        center + Point::new(back, -half_height),
        center + Point::new(back, half_height),
    )
    .into_styled(PrimitiveStyle::with_fill(BLACK))
    .draw(display)
}

/// Returns a black border of `width` pixels inside the shape.
const fn border(width: u32) -> PrimitiveStyle<Gray4> {
    PrimitiveStyleBuilder::new()
        .stroke_color(BLACK)
        .stroke_width(width)
        .stroke_alignment(StrokeAlignment::Inside)
        .fill_color(WHITE)
        .build()
}

/// Converts a pixel position to a point. All positions are less than 1000.
const fn point(x: u32, y: u32) -> Point {
    Point::new(x.cast_signed(), y.cast_signed())
}

#[cfg(test)]
pub mod tests {
    use embedded_graphics::{pixelcolor::Gray4, prelude::*};
    use shared::{Connection, PowerSource, SceneView, TileKind, TileState, TileView, ViewModel};

    use crate::{Framebuffer, draw, layout};

    fn tile(label: &str, kind: TileKind, state: TileState, text: &str) -> TileView {
        TileView {
            label: label.into(),
            entity_id: "light.test".into(),
            kind,
            state,
            text: text.into(),
            detail: String::new(),
        }
    }

    pub fn view() -> ViewModel {
        let mut climate = tile("Thermostat", TileKind::Climate, TileState::Value, "19.5°");
        climate.detail = "Set 21°".into();
        ViewModel {
            path: "/room/living_room".into(),
            title: "Living Room".into(),
            page_index: 1,
            page_count: 10,
            page_label: "2 / 10".into(),
            tiles: vec![
                tile("Lights", TileKind::Light, TileState::On, "On 75%"),
                tile("Rocket lamp", TileKind::Light, TileState::Off, "Off"),
                tile(
                    "Standard lamp",
                    TileKind::Light,
                    TileState::Unavailable,
                    "Unavailable",
                ),
                tile("Temperature", TileKind::Sensor, TileState::Value, "21.4 °C"),
                climate,
                tile(
                    "A label that is much too long for the tile",
                    TileKind::Sensor,
                    TileState::Value,
                    "A value that is much too long for the tile",
                ),
            ],
            scenes: vec![SceneView {
                label: "Bright".into(),
                entity_id: "scene.bright".into(),
            }],
            connection: Connection::Online,
            power: PowerSource::Stand,
            status: "Online, on stand".into(),
            asleep: false,
            last_sync: None,
            updated: "Live".into(),
            full_refresh_seq: 1,
        }
    }

    /// Returns a point inside `area`, `inset` pixels from its top left.
    fn inside(area: embedded_graphics::primitives::Rectangle, inset: i32) -> Point {
        area.top_left + Point::new(inset, inset)
    }

    #[test]
    fn a_full_view_draws_in_black_and_white() {
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view(), &mut frame);

        // A light that is on is a black tile. A light that is off is white
        // inside its border.
        let on = layout::tile(0).unwrap();
        let off = layout::tile(1).unwrap();
        assert_eq!(
            frame.pixel(on.bottom_right().unwrap() - Point::new(10, 10)),
            Some(Gray4::BLACK)
        );
        assert_eq!(
            frame.pixel(Point::new(off.center().x, off.top_left.y + 2)),
            Some(Gray4::BLACK)
        );
        assert_eq!(frame.pixel(inside(off, 8)), Some(Gray4::WHITE));

        // The tile that Home Assistant cannot reach has a dashed border.
        let unavailable = layout::tile(2).unwrap();
        let top = unavailable.top_left.y + 1;
        let left = unavailable.top_left.x;
        assert_eq!(frame.pixel(Point::new(left + 4, top)), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(Point::new(left + 20, top)), Some(Gray4::WHITE));

        // The empty scene cells and the space between the areas are white.
        let empty = layout::scene(2).unwrap();
        assert_eq!(frame.pixel(empty.center()), Some(Gray4::WHITE));
        assert_eq!(frame.pixel(Point::new(5, 300)), Some(Gray4::WHITE));

        // The header has text, and the screen has no grey.
        let header = layout::header();
        assert!(
            header
                .points()
                .any(|point| frame.pixel(point) == Some(Gray4::BLACK))
        );
        assert!(frame.lumas().all(|luma| luma == 0 || luma == 15));
    }

    #[test]
    fn long_text_stays_in_its_tile() {
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view(), &mut frame);
        let area = layout::tile(5).unwrap();
        // The text of the last tile clips before the padding at the right.
        let right = area.top_left.x + area.size.width.cast_signed() - 8;
        for y in area.top_left.y + 4..area.top_left.y + area.size.height.cast_signed() - 4 {
            assert_eq!(
                frame.pixel(Point::new(right, y)),
                Some(Gray4::WHITE),
                "y = {y}"
            );
        }
    }

    #[test]
    fn the_home_button_is_black_on_the_home_page() {
        let mut view = view();
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view, &mut frame);
        let point = inside(layout::home(), 8);
        assert_eq!(frame.pixel(point), Some(Gray4::WHITE));

        view.path = "/".into();
        let Ok(()) = draw(&view, &mut frame);
        assert_eq!(frame.pixel(point), Some(Gray4::BLACK));
    }
}
