//! Draws the view model.

use embedded_graphics::{
    pixelcolor::Gray4,
    prelude::*,
    primitives::{
        Circle, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle,
        StrokeAlignment, Triangle,
    },
};
use shared::{
    IndexView, LightState, PageView, RoomSummary, SceneView, TileKind, TileState, TileView,
    ViewModel,
    layout::{MAX_LIGHTS, MAX_SCENES},
};

use crate::{
    layout::{self, MARGIN, WIDTH},
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
/// The diameter of a light dot on a room button.
const DOT: u32 = 18;
/// The distance between the centres of two light dots.
const DOT_STEP: u32 = DOT + 10;
/// The largest number of light dots on a room button. A test in `shared`
/// checks that no room in the layout has more. If a room has more, the
/// button shows `+N` for the rest.
const MAX_DOTS: usize = MAX_LIGHTS;
/// The distance from the bottom of a room button to the baseline of its
/// second line.
const ROOM_BASELINE: u32 = 18;
/// The distance from the bottom of a room button to the centre of its
/// light dots.
const DOT_MIDDLE: u32 = 26;
/// The space between the touch area of a − or + button and the border that
/// the button shows. The touch area is larger than the border, so a tap
/// near the border still hits the button.
const STEP_INSET: u32 = 8;
/// The radius of the corners of a − or + button.
const STEP_RADIUS: u32 = 8;
/// The length of the bars of the − and + signs.
const SIGN_LENGTH: u32 = 28;
/// The thickness of the bars of the − and + signs.
const SIGN_THICKNESS: u32 = 4;
/// The space between the brightness and the indicator in the strip of a
/// tile with steps.
const STRIP_GAP: u32 = 8;
/// The width of the middle of the strip of a tile with steps: the space
/// between the touch areas of the − and the + button.
const STRIP_MIDDLE: u32 = layout::CELL_WIDTH - 2 * layout::STEP_WIDTH;

// The middle of the strip has space for the indicator and the gap before
// it. `draw_steps` subtracts both from the width of the middle.
const _: () = assert!(STRIP_MIDDLE >= INDICATOR + STRIP_GAP);
// The border and the sign of a − or + button fit in its touch area.
const _: () = assert!(layout::STEP_WIDTH >= 2 * STEP_INSET + SIGN_LENGTH);
const _: () = assert!(layout::STEP_HEIGHT >= 2 * STEP_INSET + SIGN_LENGTH);

/// Draws the view model on the display.
///
/// A room page has these areas, from the top:
///
/// - The header: the page title on the left and the status on the right.
/// - The tiles, in a grid of three columns and two rows.
/// - The scene buttons, in one row.
/// - The footer: the previous, home and next buttons, the page number and
///   the time of the last update.
///
/// The index page has these areas, from the top:
///
/// - The header: the page title and the status, as on a room page. Below
///   them are the weather on the left and the All lights toggle on the
///   right.
/// - The room buttons, in a grid of three columns and three rows. Each
///   button has the name of the room and its summary.
/// - The footer, as on a room page.
///
/// A dimmable light tile that is on or off (`TileView::steps`) has its label
/// at the top. Below the label is a
/// strip with a − button at the left end, a + button at the right end, and
/// the brightness and the on/off indicator between them.
///
/// The screen is black and white, so a partial refresh of the panel is clean.
/// A light or a switch that is on is a black tile with white text. A tile
/// that Home Assistant cannot reach has a dashed border. The All lights
/// toggle uses the same rules.
///
/// # Errors
///
/// Returns the error of the display if a draw operation fails.
pub fn draw<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    display.clear(WHITE)?;
    match &view.page {
        PageView::Index(index) => {
            draw_header(view, layout::index_header(), display)?;
            draw_index(index, display)?;
        }
        PageView::Room(room) => {
            draw_header(view, layout::header(), display)?;
            for (index, tile) in room.tiles.iter().enumerate() {
                if let Some(area) = layout::tile(index) {
                    draw_tile(tile, area, display)?;
                }
                if tile.steps
                    && let (Some(dimmer), Some(brighter)) =
                        (layout::tile_dimmer(index), layout::tile_brighter(index))
                {
                    draw_steps(tile, dimmer, brighter, display)?;
                }
            }
            for (index, scene) in room.scenes.iter().enumerate().take(MAX_SCENES) {
                if let Some(area) = layout::scene(index) {
                    draw_scene(scene, area, display)?;
                }
            }
        }
    }
    draw_footer(view, display)
}

/// Draws the title and the status at the top of `area`, and a line at the
/// bottom of `area`.
fn draw_header<D>(view: &ViewModel, area: Rectangle, display: &mut D) -> Result<(), D::Error>
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
        point(MARGIN, area.size.height - 3),
        Size::new(WIDTH - 2 * MARGIN, 2),
    )
    .into_styled(PrimitiveStyle::with_fill(BLACK))
    .draw(display)
}

/// Draws the weather, the All lights toggle and the room buttons of the
/// index page.
fn draw_index<D>(index: &IndexView, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    if let Some(weather) = &index.weather {
        draw_weather(weather, layout::weather(), display)?;
    }
    if let Some(all_lights) = &index.all_lights {
        draw_toggle(all_lights, layout::all_lights(), display)?;
    }
    for (number, room) in index.rooms.iter().enumerate() {
        if let Some(area) = layout::room(number) {
            draw_room(room, area, display)?;
        }
    }
    Ok(())
}

/// Draws the temperature in a large font, and the condition after it.
fn draw_weather<D>(weather: &TileView, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let left = area.top_left.x.unsigned_abs();
    let baseline = area.center().y.unsigned_abs() + 14;
    let width = area.size.width;
    let fonts: &[_] = if weather.state == TileState::Value {
        &[&VALUE_MEDIUM, &VALUE_SMALL]
    } else {
        &[&LABEL]
    };
    text::draw_fitted(
        display,
        fonts,
        &weather.text,
        Anchor::baseline_left(point(left, baseline)),
        width,
        BLACK,
    )?;
    if weather.detail.is_empty() {
        return Ok(());
    }
    // The width of the text in the font that `draw_fitted` used.
    let font = fonts
        .iter()
        .find(|font| text::width(font, &weather.text) <= width)
        .or_else(|| fonts.last())
        .copied();
    let used = font.map_or(width, |font| text::width(font, &weather.text).min(width)) + PADDING;
    text::draw_fitted(
        display,
        &[&LABEL, &DETAIL],
        &weather.detail,
        Anchor::baseline_left(point(left + used, baseline)),
        width.saturating_sub(used),
        BLACK,
    )
}

/// Draws a toggle button: the label on the left and the on/off indicator on
/// the right. The button is black when the light is on, and has a dashed
/// border when Home Assistant cannot reach the light. A light with no state
/// yet has a thin border.
fn draw_toggle<D>(toggle: &TileView, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let on = toggle.state == TileState::On;
    let ink = if on { WHITE } else { BLACK };
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));
    match toggle.state {
        TileState::On => shape
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(display)?,
        TileState::Unavailable => draw_dashed_border(area, display)?,
        TileState::Unknown => shape.into_styled(border(2)).draw(display)?,
        _ => shape.into_styled(border(4)).draw(display)?,
    }

    let left = area.top_left.x.unsigned_abs() + PADDING;
    let inner = area.size.width - 2 * PADDING;
    let middle = area.center().y.unsigned_abs();
    text::draw_fitted(
        display,
        &[&BUTTON, &DETAIL],
        &toggle.label,
        Anchor::center_left(point(left, middle)),
        inner - INDICATOR - PADDING,
        ink,
    )?;
    let indicator = Circle::new(
        point(left + inner - INDICATOR, middle - INDICATOR / 2),
        INDICATOR,
    );
    draw_indicator(toggle.state, indicator, display)
}

/// Draws the indicator of a toggle in `circle`: a white dot if the toggle is
/// on, a ring if it is off, and a cross if its state is unknown or
/// unavailable. The toggle is black when it is on, so the dot is white.
fn draw_indicator<D>(state: TileState, circle: Circle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    match state {
        TileState::On => circle
            .into_styled(PrimitiveStyle::with_fill(WHITE))
            .draw(display),
        TileState::Unknown | TileState::Unavailable => {
            draw_cross(circle.center(), circle.diameter, BLACK, display)
        }
        _ => circle
            .into_styled(PrimitiveStyle::with_stroke(BLACK, 3))
            .draw(display),
    }
}

/// Draws a cross in `ink` that fits in a circle of `diameter` at `center`.
fn draw_cross<D>(center: Point, diameter: u32, ink: Gray4, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let half = (diameter / 2 - 2).cast_signed();
    let style = PrimitiveStyle::with_stroke(ink, 3);
    Line::new(
        center - Point::new(half, half),
        center + Point::new(half, half),
    )
    .into_styled(style)
    .draw(display)?;
    Line::new(
        center + Point::new(-half, half),
        center + Point::new(half, -half),
    )
    .into_styled(style)
    .draw(display)
}

/// Draws the button of a room.
///
/// The name of the room is at the top. Below it are a dot for each light
/// and switch on the left, and the temperature on the right. A dot is
/// filled if the light is on, and empty if it is off. A light that is
/// unavailable is a cross. If at least one light is on, the button is black
/// with white text and dots, as a light tile that is on.
fn draw_room<D>(room: &RoomSummary, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));
    let ink = if room.any_on {
        shape
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(display)?;
        WHITE
    } else {
        shape.into_styled(border(3)).draw(display)?;
        BLACK
    };
    let left = area.top_left.x.unsigned_abs() + PADDING;
    let top = area.top_left.y.unsigned_abs();
    let inner = area.size.width - 2 * PADDING;
    let baseline = top + area.size.height - ROOM_BASELINE;
    text::draw_fitted(
        display,
        &[&BUTTON],
        &room.title,
        Anchor::top_left(point(left, top + 12)),
        inner,
        ink,
    )?;

    let (shown, more) = dots(room.lights.len());
    for (index, light) in room.lights.iter().take(shown).enumerate() {
        draw_dot(*light, dot_center(area, index), ink, display)?;
    }
    let mut used = u32::try_from(shown).map_or(inner, |shown| shown * DOT_STEP);
    if more > 0 {
        let extra = alloc::format!("+{more}");
        let start = point(left + used, baseline);
        text::draw(
            display,
            &LABEL,
            extra.as_str(),
            Anchor::baseline_left(start),
            ink,
        )?;
        used += text::width(&LABEL, &extra) + PADDING;
    }
    if let Some(temperature) = &room.temperature {
        text::draw_fitted(
            display,
            &[&LABEL, &DETAIL],
            temperature,
            Anchor::baseline_right(point(left + inner, baseline)),
            inner.saturating_sub(used),
            ink,
        )?;
    }
    Ok(())
}

/// Returns the number of light dots to draw for `count` lights, and the
/// number of lights that have no dot.
///
/// If the lights do not fit in [`MAX_DOTS`] dots, the last dot gives its
/// place to the text `+N`, where `N` is the number of lights with no dot.
const fn dots(count: usize) -> (usize, usize) {
    if count <= MAX_DOTS {
        (count, 0)
    } else {
        (MAX_DOTS - 1, count - (MAX_DOTS - 1))
    }
}

/// Returns the centre of the light dot with `index` on the room button in
/// `area`.
fn dot_center(area: Rectangle, index: usize) -> Point {
    let index = u32::try_from(index.min(MAX_DOTS)).unwrap_or(0);
    let x = area.top_left.x.unsigned_abs() + PADDING + DOT / 2 + index * DOT_STEP;
    let y = area.top_left.y.unsigned_abs() + area.size.height - DOT_MIDDLE;
    point(x, y)
}

/// Draws one light dot in `ink`: filled if the light is on, a ring if it is
/// off, and a cross if it is unavailable.
fn draw_dot<D>(
    light: LightState,
    center: Point,
    ink: Gray4,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let circle = Circle::with_center(center, DOT);
    match light {
        LightState::On => circle
            .into_styled(PrimitiveStyle::with_fill(ink))
            .draw(display),
        LightState::Off => circle
            .into_styled(PrimitiveStyle::with_stroke(ink, 3))
            .draw(display),
        LightState::Unavailable => draw_cross(center, DOT, ink, display),
    }
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
            .into_styled(border(if toggle && tile.state != TileState::Unknown {
                4
            } else {
                2
            }))
            .draw(display)?,
    }

    let left = area.top_left.x.unsigned_abs() + PADDING;
    let top = area.top_left.y.unsigned_abs();
    let inner = area.size.width - 2 * PADDING;

    // A tile with steps shows its indicator and its brightness in the strip
    // with the − and + buttons. Refer to `draw_steps`.
    let label_width = if toggle && !tile.steps {
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
    if tile.steps {
        return Ok(());
    }

    if toggle {
        let indicator = Circle::new(point(left + inner - INDICATOR, top + 12), INDICATOR);
        draw_indicator(tile.state, indicator, display)?;
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

/// Draws the strip of a tile with steps: the − button in `dimmer`, the +
/// button in `brighter`, and the brightness and the on/off indicator in the
/// middle. The tile is black when the light is on, so the strip uses the ink
/// of the tile.
fn draw_steps<D>(
    tile: &TileView,
    dimmer: Rectangle,
    brighter: Rectangle,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let on = tile.state == TileState::On;
    let ink = if on { WHITE } else { BLACK };
    draw_step_button(dimmer, false, ink, display)?;
    draw_step_button(brighter, true, ink, display)?;

    // The middle of the strip, between the two buttons. The borders of the
    // buttons are inside their touch areas, so the text can use all the
    // space between the touch areas.
    let middle_left = dimmer.top_left.x.unsigned_abs() + layout::STEP_WIDTH;
    let center_x = middle_left + STRIP_MIDDLE / 2;
    let center_y = dimmer.center().y.unsigned_abs();

    let fonts = [&BUTTON, &DETAIL];
    let text_width = if tile.detail.is_empty() {
        0
    } else {
        let max_width = STRIP_MIDDLE - INDICATOR - STRIP_GAP;
        let width = fonts
            .iter()
            .map(|font| text::width(font, &tile.detail))
            .find(|width| *width <= max_width)
            .unwrap_or(max_width);
        width + STRIP_GAP
    };
    let start = center_x - u32::midpoint(text_width, INDICATOR);
    if text_width > 0 {
        text::draw_fitted(
            display,
            &fonts,
            &tile.detail,
            Anchor::center_left(point(start, center_y)),
            text_width - STRIP_GAP,
            ink,
        )?;
    }
    let indicator = Circle::new(
        point(start + text_width, center_y - INDICATOR / 2),
        INDICATOR,
    );
    draw_indicator(tile.state, indicator, display)
}

/// Draws a − or (if `plus` is `true`) a + button in `area`, with a thin
/// border in `ink`. The border is inside the touch area.
fn draw_step_button<D>(
    area: Rectangle,
    plus: bool,
    ink: Gray4,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let inset = STEP_INSET.cast_signed();
    let shape = Rectangle::new(
        area.top_left + Point::new(inset, inset),
        area.size - Size::new(2 * STEP_INSET, 2 * STEP_INSET),
    );
    RoundedRectangle::with_equal_corners(shape, Size::new(STEP_RADIUS, STEP_RADIUS))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(ink)
                .stroke_width(2)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(display)?;
    let center = shape.center();
    let fill = PrimitiveStyle::with_fill(ink);
    Rectangle::with_center(center, Size::new(SIGN_LENGTH, SIGN_THICKNESS))
        .into_styled(fill)
        .draw(display)?;
    if plus {
        Rectangle::with_center(center, Size::new(SIGN_THICKNESS, SIGN_LENGTH))
            .into_styled(fill)
            .draw(display)?;
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
    use shared::{
        Connection, IndexView, LightState,
        LightState::{Off, On, Unavailable},
        PageView, PowerSource, RoomSummary, RoomView, SceneView, TileKind, TileState, TileView,
        ViewModel,
    };

    use super::{dot_center, dots};
    use crate::{Framebuffer, draw, layout};

    fn tile(label: &str, kind: TileKind, state: TileState, text: &str) -> TileView {
        TileView {
            label: label.into(),
            entity_id: "light.test".into(),
            kind,
            state,
            text: text.into(),
            detail: String::new(),
            steps: false,
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
            page: PageView::Room(RoomView {
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
            }),
            connection: Connection::Online,
            power: PowerSource::Stand,
            status: "Online, on stand".into(),
            asleep: false,
            read_only: false,
            last_sync: None,
            updated: "Last changed 09:41".into(),
            full_refresh_seq: 1,
        }
    }

    /// Returns the view of an index page with nine rooms. The All lights
    /// toggle is on.
    pub fn index_view() -> ViewModel {
        let mut weather = tile("Weather", TileKind::Weather, TileState::Value, "12.3 °C");
        weather.detail = "Partly cloudy".into();
        let rooms: [(&str, &[LightState], Option<&str>); 9] = [
            ("Living Room", &[On, Off, Unavailable], Some("20.4°")),
            ("Dining Room", &[Off, Off, Off], Some("19.5°")),
            ("Kitchen", &[Off], None),
            ("Hall", &[Off, On], None),
            ("Landing", &[On, On], None),
            ("Main bedroom", &[Off, On], Some("19.5°")),
            ("Guest bedroom", &[], Some("18°")),
            ("Loft", &[Unavailable; 3], None),
            (
                "Outside",
                &[Off; 6],
                Some("A temperature that is much too long"),
            ),
        ];
        ViewModel {
            path: "/".into(),
            title: "Home".into(),
            page_index: 0,
            page_label: "1 / 10".into(),
            page: PageView::Index(IndexView {
                weather: Some(weather),
                all_lights: Some(tile("All lights", TileKind::Light, TileState::On, "On")),
                rooms: rooms
                    .iter()
                    .map(|(title, lights, temperature)| RoomSummary {
                        title: (*title).into(),
                        lights: lights.to_vec(),
                        temperature: temperature.map(String::from),
                        any_on: lights.contains(&On),
                    })
                    .collect(),
            }),
            ..view()
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

    /// Returns the center of the indicator of the toggle tile in `area`.
    fn indicator_center(area: embedded_graphics::primitives::Rectangle) -> Point {
        Point::new(
            area.top_left.x + area.size.width.cast_signed()
                - (super::PADDING + super::INDICATOR / 2).cast_signed(),
            area.top_left.y + 12 + (super::INDICATOR / 2).cast_signed(),
        )
    }

    /// Draws a room with an off, a no-state and an unavailable light, in
    /// that order.
    fn toggle_frame() -> Framebuffer {
        let mut view = view();
        let PageView::Room(room) = &mut view.page else {
            unreachable!()
        };
        room.tiles = vec![
            tile("Off", TileKind::Light, TileState::Off, "Off"),
            tile("None", TileKind::Light, TileState::Unknown, "No data"),
            tile(
                "Gone",
                TileKind::Switch,
                TileState::Unavailable,
                "Unavailable",
            ),
        ];
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view, &mut frame);
        frame
    }

    #[test]
    fn a_toggle_with_no_state_has_a_thin_border_and_a_cross() {
        let frame = toggle_frame();
        let off = layout::tile(0).unwrap();
        let unknown = layout::tile(1).unwrap();
        let at = |area: embedded_graphics::primitives::Rectangle, dx: i32| {
            frame.pixel(Point::new(area.center().x, area.top_left.y + dx))
        };
        // The border of an off toggle is thick, and the border of a toggle
        // with no state is thin.
        assert_eq!(at(off, 3), Some(Gray4::BLACK));
        assert_eq!(at(unknown, 1), Some(Gray4::BLACK));
        assert_eq!(at(unknown, 3), Some(Gray4::WHITE));
        // The center of a ring is white, and the center of a cross is black.
        assert_eq!(frame.pixel(indicator_center(off)), Some(Gray4::WHITE));
        assert_eq!(frame.pixel(indicator_center(unknown)), Some(Gray4::BLACK));
    }

    #[test]
    fn an_unavailable_toggle_has_a_cross() {
        let frame = toggle_frame();
        let unavailable = layout::tile(2).unwrap();
        assert_eq!(
            frame.pixel(indicator_center(unavailable)),
            Some(Gray4::BLACK)
        );
    }

    /// Draws a room with a dimmable light that is on at 50%, and one that is
    /// off, in that order.
    fn dimmable_frame() -> Framebuffer {
        let mut view = view();
        let PageView::Room(room) = &mut view.page else {
            unreachable!()
        };
        let mut on = tile("On", TileKind::Light, TileState::On, "On 50%");
        on.detail = "50%".into();
        on.steps = true;
        let mut off = tile("Off", TileKind::Light, TileState::Off, "Off");
        off.steps = true;
        room.tiles = vec![on, off];
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view, &mut frame);
        frame
    }

    #[test]
    fn a_dimmable_tile_has_a_minus_and_a_plus_button_in_its_ink() {
        let frame = dimmable_frame();
        for (index, ink, paper) in [
            (0, Gray4::WHITE, Gray4::BLACK),
            (1, Gray4::BLACK, Gray4::WHITE),
        ] {
            let dimmer = layout::tile_dimmer(index).unwrap();
            let brighter = layout::tile_brighter(index).unwrap();
            // Both signs have a bar across the centre. Only the + has a bar
            // above the centre.
            let above = Point::new(0, 10);
            assert_eq!(frame.pixel(dimmer.center()), Some(ink), "{index}");
            assert_eq!(frame.pixel(brighter.center()), Some(ink), "{index}");
            assert_eq!(frame.pixel(dimmer.center() - above), Some(paper), "{index}");
            assert_eq!(frame.pixel(brighter.center() - above), Some(ink), "{index}");
            // The border of each button is inside its touch area.
            let border = |area: embedded_graphics::primitives::Rectangle| {
                Point::new(area.center().x, area.top_left.y + 9)
            };
            assert_eq!(frame.pixel(border(dimmer)), Some(ink), "{index}");
            assert_eq!(frame.pixel(border(brighter)), Some(ink), "{index}");
            assert_eq!(
                frame.pixel(Point::new(dimmer.center().x, dimmer.top_left.y + 4)),
                Some(paper),
                "{index}"
            );
        }
        assert!(frame.lumas().all(|luma| luma == 0 || luma == 15));
    }

    #[test]
    fn a_dimmable_tile_shows_its_indicator_in_the_strip_not_in_the_label_row() {
        let frame = dimmable_frame();
        let on = layout::tile(0).unwrap();
        let off = layout::tile(1).unwrap();
        // The label row has no indicator: the tile colour is there.
        assert_eq!(frame.pixel(indicator_center(on)), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(indicator_center(off)), Some(Gray4::WHITE));

        // The strip of the tile that is off has a ring in its centre. The
        // strip of the tile that is on has the brightness and a white dot.
        let strip_middle = layout::tile_dimmer(0).unwrap().center().y;
        let middle = |area: embedded_graphics::primitives::Rectangle| {
            Point::new(area.center().x, strip_middle)
        };
        let ring = middle(off) - Point::new(super::INDICATOR.cast_signed() / 2 - 1, 0);
        assert_eq!(frame.pixel(ring), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(middle(off)), Some(Gray4::WHITE));
        let strip =
            embedded_graphics::primitives::Rectangle::with_center(middle(on), Size::new(96, 40));
        let white = strip
            .points()
            .filter(|point| frame.pixel(*point) == Some(Gray4::WHITE))
            .count();
        // The text and the dot together are larger than the dot alone.
        let dot = super::INDICATOR * super::INDICATOR / 2;
        assert!(white > dot as usize, "{white}");
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

    #[test]
    fn the_index_draws_in_black_and_white() {
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&index_view(), &mut frame);

        // The All lights toggle is on, so it is black.
        let toggle = layout::all_lights();
        assert_eq!(frame.pixel(inside(toggle, 8)), Some(Gray4::BLACK));
        // A room with all its lights off has a border and is white inside.
        let room = layout::room(1).unwrap();
        assert_eq!(
            frame.pixel(Point::new(room.center().x, room.top_left.y + 1)),
            Some(Gray4::BLACK)
        );
        assert_eq!(frame.pixel(inside(room, 6)), Some(Gray4::WHITE));
        // A room with a light on is black.
        assert_eq!(
            frame.pixel(inside(layout::room(0).unwrap(), 6)),
            Some(Gray4::BLACK)
        );
        // The line at the bottom of the header is below the toggle.
        let line = layout::index_header().size.height.cast_signed() - 2;
        assert_eq!(frame.pixel(Point::new(400, line)), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(Point::new(400, 57)), Some(Gray4::WHITE));
        assert!(frame.lumas().all(|luma| luma == 0 || luma == 15));
    }

    #[test]
    fn a_dot_for_each_light() {
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&index_view(), &mut frame);

        // On a white button: the centre of a dot that is on is black, of a
        // dot that is off is white, and of a cross is black.
        let room = layout::room(7).unwrap();
        let center = dot_center(room, 0);
        assert_eq!(frame.pixel(center), Some(Gray4::BLACK));
        // A cross has white between its arms.
        assert_eq!(frame.pixel(center + Point::new(6, 0)), Some(Gray4::WHITE));
        let room = layout::room(3).unwrap();
        assert_eq!(frame.pixel(dot_center(room, 0)), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(dot_center(room, 1)), Some(Gray4::WHITE));
        // The ring of a dot that is off.
        assert_eq!(
            frame.pixel(dot_center(room, 1) - Point::new(8, 0)),
            Some(Gray4::WHITE)
        );

        // On a black button, the dots are white.
        let room = layout::room(0).unwrap();
        assert_eq!(frame.pixel(dot_center(room, 0)), Some(Gray4::WHITE));
        assert_eq!(frame.pixel(dot_center(room, 1)), Some(Gray4::BLACK));
        assert_eq!(frame.pixel(dot_center(room, 2)), Some(Gray4::WHITE));
        // No dot after the last light.
        assert_eq!(frame.pixel(dot_center(room, 3)), Some(Gray4::BLACK));
    }

    #[test]
    fn more_lights_than_dots_shows_the_rest_as_a_number() {
        assert_eq!(dots(0), (0, 0));
        assert_eq!(dots(4), (4, 0));
        assert_eq!(dots(5), (3, 2));
        assert_eq!(dots(6), (3, 3));

        // The Outside button has 6 lights: 3 dots, then `+3`.
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&index_view(), &mut frame);
        let room = layout::room(8).unwrap();
        for index in 0..3 {
            let ring = dot_center(room, index) - Point::new(8, 0);
            assert_eq!(frame.pixel(ring), Some(Gray4::BLACK), "dot {index}");
        }
        // The place of the fourth dot has the text, not a ring.
        let text = embedded_graphics::primitives::Rectangle::new(
            dot_center(room, 3) - Point::new(9, 12),
            Size::new(40, 24),
        );
        assert!(
            text.points()
                .any(|point| frame.pixel(point) == Some(Gray4::BLACK))
        );
        assert_eq!(
            frame.pixel(dot_center(room, 3) - Point::new(8, 0)),
            Some(Gray4::WHITE)
        );
    }

    #[test]
    fn a_long_temperature_stays_in_its_room_button() {
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&index_view(), &mut frame);
        let area = layout::room(8).unwrap();
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
    fn an_all_lights_toggle_that_is_off_is_white() {
        let mut view = index_view();
        if let PageView::Index(index) = &mut view.page {
            let toggle = index.all_lights.as_mut().unwrap();
            toggle.state = TileState::Off;
        }
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view, &mut frame);
        assert_eq!(
            frame.pixel(inside(layout::all_lights(), 8)),
            Some(Gray4::WHITE)
        );
    }
}
