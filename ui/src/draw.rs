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
    IndexView, LightState, PageView, PowerSource, RoomSummary, SceneView, TileKind, TileState,
    TileView, ViewModel,
    layout::{MAX_LIGHTS, MAX_SCENES},
};

use crate::{
    layout::{self, MARGIN, WIDTH},
    pattern,
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
/// The diameter of the dot in the indicator of a toggle that waits for Home
/// Assistant.
const PENDING_DOT: u32 = 8;
/// The length of a dash on the border of an unavailable tile.
const DASH: u32 = 14;
/// The width of the right part of the header: the status text, the
/// percent of the battery and the power icon. The page title has the rest
/// of the width. A test checks that each page title and the usual status
/// texts fit. A rare long status text is clipped.
const STATUS_AREA: u32 = 400;
/// The width of the page title in the header.
const TITLE_WIDTH: u32 = WIDTH - 3 * MARGIN - STATUS_AREA;
/// The width of the box of the power icon. The battery and the plug have
/// the same box, so the percent does not move when the power source changes.
const POWER_ICON_WIDTH: u32 = 46;
/// The height of the box of the power icon.
const POWER_ICON_HEIGHT: u32 = 24;
/// The width of the box of the percent of the battery. The widest text,
/// `100%`, fits in it. The box keeps its width when the percent is not
/// known, so the status text does not move.
const PERCENT_WIDTH: u32 = 74;
/// The space between the status text, the percent and the power icon.
const POWER_GAP: u32 = 6;
/// The width of the status text in the header.
const STATUS_WIDTH: u32 = STATUS_AREA - POWER_ICON_WIDTH - PERCENT_WIDTH - 2 * POWER_GAP;
/// The width of the body of the battery icon, without the terminal.
const BATTERY_BODY: u32 = 42;
/// The thickness of the outline of the battery icon.
const BATTERY_STROKE: u32 = 2;
/// The space between the outline of the battery icon and its fill.
const BATTERY_INSET: u32 = BATTERY_STROKE + 2;
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
/// - The footer: the previous and next buttons, and between them the page
///   number and the time of the last update. The touch key on the front of
///   the device shows the index page.
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
/// A tile with steps (`TileView::steps`) has its label at the top. Below the
/// label is a strip with a − button at the left end and a + button at the
/// right end. On a dimmable light tile, the strip has the brightness and the
/// on/off indicator between the buttons. On a climate tile, the current
/// temperature is at the right end of the line of the label, and the strip
/// has the target temperature between the buttons.
///
/// The screen is black and white, so a partial refresh of the panel is clean.
/// A light or a switch that is on is a black tile with white text. A tile
/// that Home Assistant cannot reach has a dashed border. A tile or a scene
/// button that waits for Home Assistant (`TileState::Pending` and
/// `SceneView::pending`) has a pattern of black and white pixels, which
/// looks light grey. Its text is black with a white outline. The All lights
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
                    && let (Some(minus), Some(plus)) =
                        (layout::tile_minus(index), layout::tile_plus(index))
                {
                    draw_steps(tile, minus, plus, display)?;
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
///
/// From the right margin, the status has the power icon, the percent of the
/// battery and the status text. The icon and the percent have boxes at
/// fixed positions, so a change of the percent changes only a small area of
/// the panel.
fn draw_header<D>(view: &ViewModel, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let baseline = 42;
    text::draw_fitted(
        display,
        &[&TITLE],
        &view.title,
        Anchor::baseline_left(point(MARGIN, baseline)),
        TITLE_WIDTH,
        BLACK,
    )?;
    text::draw_fitted(
        display,
        &[&LABEL, &DETAIL],
        &view.status,
        Anchor::baseline_right(point(STATUS_RIGHT, baseline)),
        STATUS_WIDTH,
        BLACK,
    )?;
    if let Some(level) = view.battery {
        text::draw(
            display,
            &LABEL,
            format_args!("{}%", level.min(100)),
            Anchor::baseline_right(point(PERCENT_RIGHT, baseline)),
            BLACK,
        )?;
    }
    let icon = power_icon_area();
    match view.power {
        PowerSource::Battery => draw_battery(view.battery, icon, display)?,
        PowerSource::Stand => draw_plug(icon, display)?,
    }
    Rectangle::new(
        point(MARGIN, area.size.height - 3),
        Size::new(WIDTH - 2 * MARGIN, 2),
    )
    .into_styled(PrimitiveStyle::with_fill(BLACK))
    .draw(display)
}

/// The right edge of the percent box in the header.
const PERCENT_RIGHT: u32 = WIDTH - MARGIN - POWER_ICON_WIDTH - POWER_GAP;
/// The right edge of the status text in the header.
const STATUS_RIGHT: u32 = PERCENT_RIGHT - PERCENT_WIDTH - POWER_GAP;

/// Returns the box of the power icon in the header. The middle of the box
/// is at the middle of the status text.
const fn power_icon_area() -> Rectangle {
    Rectangle::new(
        point(WIDTH - MARGIN - POWER_ICON_WIDTH, 20),
        Size::new(POWER_ICON_WIDTH, POWER_ICON_HEIGHT),
    )
}

/// Draws a battery in `area`: an outline with rounded corners and a
/// terminal on the right. A black fill inside the outline shows `level`
/// percent from the left. If the level is not known, the outline is empty.
fn draw_battery<D>(level: Option<u8>, area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let left = area.top_left.x.unsigned_abs();
    let top = area.top_left.y.unsigned_abs();
    let height = area.size.height;
    let body = Rectangle::new(point(left, top), Size::new(BATTERY_BODY, height));
    RoundedRectangle::with_equal_corners(body, Size::new(4, 4))
        .into_styled(border(BATTERY_STROKE))
        .draw(display)?;
    let terminal_height = height / 2;
    Rectangle::new(
        point(left + BATTERY_BODY, top + (height - terminal_height) / 2),
        Size::new(area.size.width - BATTERY_BODY, terminal_height),
    )
    .into_styled(PrimitiveStyle::with_fill(BLACK))
    .draw(display)?;
    let inner = BATTERY_BODY - 2 * BATTERY_INSET;
    let filled = level.map_or(0, |level| u32::from(level.min(100)) * inner / 100);
    if filled == 0 {
        return Ok(());
    }
    Rectangle::new(
        point(left + BATTERY_INSET, top + BATTERY_INSET),
        Size::new(filled, height - 2 * BATTERY_INSET),
    )
    .into_styled(PrimitiveStyle::with_fill(BLACK))
    .draw(display)
}

/// Draws a mains plug in `area`, from the side: a cord on the left, the
/// body in the middle and two prongs on the right.
fn draw_plug<D>(area: Rectangle, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let left = area.top_left.x.unsigned_abs();
    let top = area.top_left.y.unsigned_abs();
    let middle = top + area.size.height / 2;
    let fill = PrimitiveStyle::with_fill(BLACK);
    // The cord.
    Rectangle::new(point(left, middle - 2), Size::new(14, 4))
        .into_styled(fill)
        .draw(display)?;
    // The body, which is narrower at the cord.
    Rectangle::new(point(left + 12, middle - 6), Size::new(4, 12))
        .into_styled(fill)
        .draw(display)?;
    RoundedRectangle::with_equal_corners(
        Rectangle::new(
            point(left + 16, top + 1),
            Size::new(16, area.size.height - 2),
        ),
        Size::new(4, 4),
    )
    .into_styled(fill)
    .draw(display)?;
    // The prongs.
    for y in [middle - 7, middle + 3] {
        Rectangle::new(point(left + 32, y), Size::new(14, 4))
            .into_styled(fill)
            .draw(display)?;
    }
    Ok(())
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
    let pending = toggle.state == TileState::Pending;
    let outline = pending_outline(pending);
    let ink = if on { WHITE } else { BLACK };
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));
    match toggle.state {
        TileState::On => shape
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(display)?,
        TileState::Pending => draw_pending_fill(shape, 4, display)?,
        TileState::Unavailable => draw_dashed_border(area, display)?,
        TileState::Unknown => shape.into_styled(border(2)).draw(display)?,
        _ => shape.into_styled(border(4)).draw(display)?,
    }

    let left = area.top_left.x.unsigned_abs() + PADDING;
    let inner = area.size.width - 2 * PADDING;
    let middle = area.center().y.unsigned_abs();
    text::draw_fitted_outlined(
        display,
        &[&BUTTON, &DETAIL],
        &toggle.label,
        Anchor::center_left(point(left, middle)),
        inner - INDICATOR - PADDING,
        ink,
        outline,
    )?;
    let indicator = Circle::new(
        point(left + inner - INDICATOR, middle - INDICATOR / 2),
        INDICATOR,
    );
    draw_indicator(toggle.state, indicator, display)
}

/// Draws the indicator of a toggle in `circle`: a white dot if the toggle is
/// on, a ring if it is off, and a cross if its state is unknown or
/// unavailable. The toggle is black when it is on, so the dot is white. A
/// toggle that waits for Home Assistant has a ring with a black dot in the
/// centre, on white.
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
        TileState::Pending => {
            circle
                .into_styled(
                    PrimitiveStyleBuilder::new()
                        .fill_color(WHITE)
                        .stroke_color(BLACK)
                        .stroke_width(3)
                        .build(),
                )
                .draw(display)?;
            Circle::with_center(circle.center(), PENDING_DOT)
                .into_styled(PrimitiveStyle::with_fill(BLACK))
                .draw(display)
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
    let pending = tile.state == TileState::Pending;
    let outline = pending_outline(pending);
    let ink = if on { WHITE } else { BLACK };
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));

    match tile.state {
        _ if on => shape
            .into_styled(PrimitiveStyle::with_fill(BLACK))
            .draw(display)?,
        TileState::Pending => draw_pending_fill(shape, 4, display)?,
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

    // A light tile with steps shows its indicator and its brightness in the
    // strip with the − and + buttons. A climate tile with steps shows its
    // value at the right end of the line of the label, and its detail in the
    // strip. Refer to `draw_steps`.
    let value_width = if tile.steps && !toggle {
        text::width(&VALUE_SMALL, &tile.text).min(inner / 2)
    } else {
        0
    };
    let label_width = if toggle && !tile.steps {
        inner - INDICATOR - PADDING
    } else if value_width > 0 {
        inner - value_width - PADDING
    } else {
        inner
    };
    text::draw_fitted_outlined(
        display,
        &[&LABEL],
        &tile.label,
        Anchor::top_left(point(left, top + 12)),
        label_width,
        ink,
        outline,
    )?;
    if tile.steps {
        if value_width > 0 {
            // `VALUE_SMALL` has the same height as `LABEL`, so the two texts
            // have the same top and the same baseline.
            text::draw_fitted_outlined(
                display,
                &[&VALUE_SMALL],
                &tile.text,
                Anchor::top_right(point(left + inner, top + 12)),
                value_width,
                ink,
                outline,
            )?;
        }
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
    text::draw_fitted_outlined(
        display,
        fonts,
        &tile.text,
        Anchor::baseline_left(point(left, top + value_baseline)),
        inner,
        ink,
        outline,
    )?;
    if !tile.detail.is_empty() {
        text::draw_fitted_outlined(
            display,
            &[&LABEL, &DETAIL],
            &tile.detail,
            Anchor::baseline_left(point(left, top + 120)),
            inner,
            ink,
            outline,
        )?;
    }
    Ok(())
}

/// Draws the strip of a tile with steps: the − button in `minus`, the +
/// button in `plus`, and the detail in the middle. The detail of a light is
/// its brightness, and the on/off indicator follows it. The detail of a
/// thermostat is its target temperature, with no indicator. The tile is
/// black when the light is on, so the strip uses the ink of the tile.
fn draw_steps<D>(
    tile: &TileView,
    minus: Rectangle,
    plus: Rectangle,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let on = tile.state == TileState::On;
    let ink = if on { WHITE } else { BLACK };
    let outline = pending_outline(tile.state == TileState::Pending);
    let fill = outline;
    draw_step_button(minus, false, ink, fill, display)?;
    draw_step_button(plus, true, ink, fill, display)?;

    // The middle of the strip, between the two buttons. The borders of the
    // buttons are inside their touch areas, so the text can use all the
    // space between the touch areas.
    let middle_left = minus.top_left.x.unsigned_abs() + layout::STEP_WIDTH;
    let center_x = middle_left + STRIP_MIDDLE / 2;
    let center_y = minus.center().y.unsigned_abs();

    // Only a light or a switch has the on/off indicator, with a gap before
    // it.
    let indicator = matches!(tile.kind, TileKind::Light | TileKind::Switch);
    let (indicator_width, gap) = if indicator {
        (INDICATOR, STRIP_GAP)
    } else {
        (0, 0)
    };
    // The target of a thermostat, for example `Set 21.5°`, does not fit in
    // `BUTTON`. It always uses `DETAIL`, so the font does not change from one
    // step to the next.
    let fonts: &[_] = if indicator {
        &[&BUTTON, &DETAIL]
    } else {
        &[&DETAIL]
    };
    let text_width = if tile.detail.is_empty() {
        0
    } else {
        let max_width = STRIP_MIDDLE - indicator_width - gap;
        fonts
            .iter()
            .map(|font| text::width(font, &tile.detail))
            .find(|width| *width <= max_width)
            .unwrap_or(max_width)
    };
    let gap = if text_width > 0 { gap } else { 0 };
    let start = center_x - (text_width + gap + indicator_width) / 2;
    if text_width > 0 {
        text::draw_fitted_outlined(
            display,
            fonts,
            &tile.detail,
            Anchor::center_left(point(start, center_y)),
            text_width,
            ink,
            outline,
        )?;
    }
    if !indicator {
        return Ok(());
    }
    let indicator = Circle::new(
        point(start + text_width + gap, center_y - INDICATOR / 2),
        INDICATOR,
    );
    draw_indicator(tile.state, indicator, display)
}

/// Draws a − or (if `plus` is `true`) a + button in `area`, with a thin
/// border in `ink`. The border is inside the touch area. If `fill` has a
/// colour, the button has a fill of that colour, for example white on the
/// pattern of a tile that waits for Home Assistant.
fn draw_step_button<D>(
    area: Rectangle,
    plus: bool,
    ink: Gray4,
    fill: Option<Gray4>,
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
    let mut style = PrimitiveStyleBuilder::new()
        .stroke_color(ink)
        .stroke_width(2)
        .stroke_alignment(StrokeAlignment::Inside);
    if let Some(fill) = fill {
        style = style.fill_color(fill);
    }
    RoundedRectangle::with_equal_corners(shape, Size::new(STEP_RADIUS, STEP_RADIUS))
        .into_styled(style.build())
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
    let fill = if scene.pending {
        Fill::Pending
    } else {
        Fill::White
    };
    draw_button(&scene.label, area, fill, display)
}

/// The fill of a scene button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// A white button with a border and black text.
    White,
    /// The pattern of a button that waits for Home Assistant, with a border
    /// and black text. Refer to [`draw_pending_fill`].
    Pending,
}

/// Draws a button with the fill `fill`, and its text in the centre.
fn draw_button<D>(label: &str, area: Rectangle, fill: Fill, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    let shape = RoundedRectangle::with_equal_corners(area, Size::new(RADIUS, RADIUS));
    let outline = pending_outline(fill == Fill::Pending);
    let ink = match fill {
        Fill::White => {
            shape.into_styled(border(3)).draw(display)?;
            BLACK
        }
        Fill::Pending => {
            draw_pending_fill(shape, 3, display)?;
            BLACK
        }
    };
    text::draw_fitted_outlined(
        display,
        &[&BUTTON, &DETAIL],
        label,
        Anchor::center(area.center()),
        area.size.width - 2 * PADDING,
        ink,
        outline,
    )
}

/// Draws the pattern of a tile or a button that waits for Home Assistant in
/// `shape`, and a black border of `width` on its edge.
///
/// The pattern is a grid of black dots, so 25% of the pixels are black. Thus
/// the pattern looks light grey. Refer to [`pattern`]. It is clearly
/// different from the black of a light that is on and from the white of a
/// light that is off. The text on the pattern is black, with a white outline
/// of 1 pixel around each glyph, so it stays clear. Refer to
/// [`pending_outline`].
///
/// The panel has only black and white pixels, so the change to the pattern
/// is a partial refresh. A grey level of `Gray4` needs a full refresh, which
/// flashes. Refer to `Refresher`.
fn draw_pending_fill<D>(
    shape: RoundedRectangle,
    width: u32,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    shape
        .into_styled(PrimitiveStyle::with_fill(BLACK))
        .draw(&mut Dots(display))?;
    shape
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(BLACK)
                .stroke_width(width)
                .stroke_alignment(StrokeAlignment::Inside)
                .build(),
        )
        .draw(display)
}

/// Returns the colour of the outline of the text on a tile or a button:
/// white if it waits for Home Assistant, or no outline. Refer to
/// [`draw_pending_fill`].
const fn pending_outline(pending: bool) -> Option<Gray4> {
    if pending { Some(WHITE) } else { None }
}

/// A draw target that draws only the pixels of the grid of the pending
/// pattern, and white in place of the other pixels. Refer to
/// [`draw_pending_fill`].
struct Dots<'a, D>(&'a mut D);

impl<D> Dimensions for Dots<'_, D>
where
    D: DrawTarget<Color = Gray4>,
{
    fn bounding_box(&self) -> Rectangle {
        self.0.bounding_box()
    }
}

impl<D> DrawTarget for Dots<'_, D>
where
    D: DrawTarget<Color = Gray4>,
{
    type Color = Gray4;
    type Error = D::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Gray4>>,
    {
        self.0
            .draw_iter(pixels.into_iter().map(|Pixel(point, color)| {
                Pixel(
                    point,
                    if pattern::is_black(point) {
                        color
                    } else {
                        WHITE
                    },
                )
            }))
    }
}

fn draw_footer<D>(view: &ViewModel, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Gray4>,
{
    draw_arrow_button(layout::previous(), false, display)?;
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
        Connection, FullRefresh, IndexView, LightState,
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
        let mut climate = tile("Thermostat", TileKind::Climate, TileState::Value, "20.3°");
        climate.detail = "Set 19.5°".into();
        climate.steps = true;
        ViewModel {
            path: "/room/living_room".into(),
            title: "Living room".into(),
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
                    pending: false,
                }],
            }),
            connection: Connection::Online,
            power: PowerSource::Stand,
            battery: Some(78),
            status: "Online".into(),
            asleep: false,
            read_only: false,
            last_sync: None,
            updated: "Last changed 09:41".into(),
            full_refresh: FullRefresh::NONE,
        }
    }

    /// Returns the view of an index page with nine rooms. The All lights
    /// toggle is on.
    pub fn index_view() -> ViewModel {
        let mut weather = tile("Weather", TileKind::Weather, TileState::Value, "12.3 °C");
        weather.detail = "Partly cloudy".into();
        let rooms: [(&str, &[LightState], Option<&str>); 9] = [
            ("Living room", &[On, Off, Unavailable], Some("20.4°")),
            ("Dining room", &[Off, Off, Off], Some("19.5°")),
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
            let minus = layout::tile_minus(index).unwrap();
            let plus = layout::tile_plus(index).unwrap();
            // Both signs have a bar across the centre. Only the + has a bar
            // above the centre.
            let above = Point::new(0, 10);
            assert_eq!(frame.pixel(minus.center()), Some(ink), "{index}");
            assert_eq!(frame.pixel(plus.center()), Some(ink), "{index}");
            assert_eq!(frame.pixel(minus.center() - above), Some(paper), "{index}");
            assert_eq!(frame.pixel(plus.center() - above), Some(ink), "{index}");
            // The border of each button is inside its touch area.
            let border = |area: embedded_graphics::primitives::Rectangle| {
                Point::new(area.center().x, area.top_left.y + 9)
            };
            assert_eq!(frame.pixel(border(minus)), Some(ink), "{index}");
            assert_eq!(frame.pixel(border(plus)), Some(ink), "{index}");
            assert_eq!(
                frame.pixel(Point::new(minus.center().x, minus.top_left.y + 4)),
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
        let strip_middle = layout::tile_minus(0).unwrap().center().y;
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

    /// Counts the black pixels of `frame` in `area`.
    fn black_in(frame: &Framebuffer, area: embedded_graphics::primitives::Rectangle) -> usize {
        area.points()
            .filter(|point| frame.pixel(*point) == Some(Gray4::BLACK))
            .count()
    }

    #[test]
    fn the_widest_percent_fits_in_its_box() {
        assert!(crate::text::width(&crate::text::LABEL, "100%") <= super::PERCENT_WIDTH);
    }

    #[test]
    fn the_text_of_each_climate_tile_with_steps_fits() {
        use crate::text::{DETAIL, LABEL, VALUE_SMALL, width};
        let inner = layout::CELL_WIDTH - 2 * super::PADDING;
        // The widest usual values: a current temperature with one decimal
        // place, and a target with one decimal place.
        let value = width(&VALUE_SMALL, "88.8°");
        let tiles = shared::pages::PAGES
            .iter()
            .flat_map(|page| page.tiles)
            .filter(|tile| tile.kind == TileKind::Climate);
        for tile in tiles {
            assert!(
                width(&LABEL, tile.label) + super::PADDING + value <= inner,
                "{}",
                tile.label
            );
        }
        assert!(value <= inner / 2);
        assert!(width(&DETAIL, "Set 88.8°") <= super::STRIP_MIDDLE);
    }

    #[test]
    fn each_page_title_and_the_usual_status_texts_fit_in_the_header() {
        for page in shared::pages::PAGES {
            assert!(
                crate::text::width(&crate::text::TITLE, page.title) <= super::TITLE_WIDTH,
                "{}",
                page.title
            );
        }
        // A text can fall back to the smaller font, but it is not clipped.
        // A rare long text, for example `Dry run · Connecting, 99 taps not
        // sent`, is clipped.
        for status in [
            "Online",
            "Connecting",
            "Offline",
            "Asleep",
            "Dry run · Online",
            "Online, 1 tap not sent",
            "Asleep, 1 tap not sent",
        ] {
            assert!(
                crate::text::width(&crate::text::DETAIL, status) <= super::STATUS_WIDTH,
                "{status}"
            );
        }
    }

    #[test]
    fn the_battery_fills_in_proportion_to_the_charge() {
        let icon = super::power_icon_area();
        let mut fills = Vec::new();
        for level in [None, Some(0), Some(12), Some(50), Some(100)] {
            let mut frame = Framebuffer::new();
            let view = ViewModel {
                power: PowerSource::Battery,
                battery: level,
                ..view()
            };
            let Ok(()) = draw(&view, &mut frame);
            fills.push(black_in(&frame, icon));
        }
        // No level and 0% give the same empty outline. Each larger level
        // gives more black pixels.
        assert_eq!(fills[0], fills[1]);
        assert!(
            fills.windows(2).skip(1).all(|pair| pair[0] < pair[1]),
            "{fills:?}"
        );
    }

    #[test]
    fn the_stand_shows_a_plug_and_no_percent_when_the_charge_is_not_known() {
        let icon = super::power_icon_area();
        let percent = embedded_graphics::primitives::Rectangle::new(
            Point::new(
                (super::PERCENT_RIGHT - super::PERCENT_WIDTH).cast_signed(),
                10,
            ),
            Size::new(super::PERCENT_WIDTH, 40),
        );
        let mut frame = Framebuffer::new();
        let Ok(()) = draw(&view(), &mut frame);
        assert!(black_in(&frame, icon) > 0);
        assert!(black_in(&frame, percent) > 0);

        let mut frame = Framebuffer::new();
        let Ok(()) = draw(
            &ViewModel {
                battery: None,
                ..view()
            },
            &mut frame,
        );
        assert!(black_in(&frame, icon) > 0);
        assert_eq!(black_in(&frame, percent), 0);
    }
}
