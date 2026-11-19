//! The view model and the presentation decisions.
//!
//! The decisions are the text and the state of each tile, the summary of
//! each room on the index page, the connection indicator and the
//! "Last changed HH:MM" text. The `ui` crate only draws what this module
//! gives it.

use alloc::{format, string::String, vec::Vec};

use chrono::{DateTime, Timelike, Utc};
use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::{
    Model,
    capabilities::power::PowerSource,
    entities::EntityState,
    features::{
        connection::{Link, page_time},
        pending::is_pending,
    },
    layout::{Page, Tile, TileKind},
    local_time::uk_time,
    summary::summarise,
};

/// The data that a shell needs to draw the screen.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ViewModel {
    /// The path of the current route, for example `/room/kitchen`. The shell
    /// keeps it during a deep sleep and gives it back in `Start`.
    pub path: String,
    /// The title of the current page.
    pub title: String,
    /// The index of the current page, from 0.
    pub page_index: usize,
    /// The number of pages.
    pub page_count: usize,
    /// The page number as text, for example `2 / 10`.
    pub page_label: String,
    /// The content of the current page: the index or a room.
    pub page: PageView,
    /// The connection to Home Assistant.
    pub connection: Connection,
    /// The power source. A renderer shows it as an icon after the status
    /// text: a plug on the stand, and a battery on battery.
    pub power: PowerSource,
    /// The charge of the battery in percent, rounded to the nearest
    /// [`BATTERY_STEP`], or `None` if the shell did not read it. A renderer
    /// shows the percent before the power icon, and the battery icon fills
    /// in proportion to the charge. If the charge is not known, a renderer
    /// shows only the icon.
    ///
    /// The rounding prevents a partial refresh for each change of 1%.
    pub battery: Option<u8>,
    /// The connection as text, for example `Online`. If the core dropped
    /// taps before it slept, the text also gives their number, for example
    /// `Asleep, 1 tap not sent`. In a dry run, the text starts with
    /// `Dry run · `, so the panel shows why a tap does not change a light.
    /// The text does not give the power source. Refer to
    /// [`power`](Self::power) and [`battery`](Self::battery).
    pub status: String,
    /// The device is asleep. The panel keeps this image until the device wakes.
    pub asleep: bool,
    /// The dashboard does a dry run: a tap on a tile or a scene sends no
    /// service call. The status text shows it on the panel. A shell can
    /// also show it. For example, the simulator draws a red border round the
    /// panel when the dry run stops.
    pub read_only: bool,
    /// The footer time of the current page, in seconds since the Unix epoch.
    /// This is the latest `last_updated` time from Home Assistant of the
    /// entities that the page shows. Refer to [`page_time`](crate::features::connection::page_time). The
    /// shell keeps it during a deep sleep and gives it back in `Start`.
    ///
    /// Each page has its own time. After a page turn, the value is the time
    /// of the new page.
    pub last_sync: Option<u64>,
    /// The freshness of the data: `Last changed HH:MM`, with the UK time of
    /// [`last_sync`](Self::last_sync). If the time is not known, the text is
    /// empty.
    ///
    /// The text is the same for each connection state. The status text shows
    /// the connection. Refer to [`status`](Self::status).
    ///
    /// The time is not a clock. It changes only when Home Assistant updates
    /// an entity that the page shows in a new minute, or on a page turn. A
    /// change that the page does not show does not change it, so the panel
    /// does not refresh each minute.
    pub updated: String,
    /// A number that the core increases each time that the panel needs a
    /// full refresh, to clear ghost images.
    ///
    /// The core asks for a full refresh when the whole screen changes, after
    /// a number of partial renders, and after a time. Refer to
    /// [`features::refresh`](crate::features::refresh). The shell keeps the
    /// number of its last full refresh. If the number in the view is
    /// different, the shell does a full refresh and keeps the new number.
    /// This works also when the shell draws once for many `Render` requests.
    ///
    /// The shell does not count refreshes. It does a full refresh for
    /// another reason only if the panel cannot draw the change with a
    /// partial refresh, for example grey pixels. Refer to `ui::Refresher`.
    pub full_refresh_seq: u32,
}

impl ViewModel {
    /// Returns the tiles of a room page, or no tiles on the index page.
    #[must_use]
    pub fn tiles(&self) -> &[TileView] {
        match &self.page {
            PageView::Index(_) => &[],
            PageView::Room(room) => &room.tiles,
        }
    }

    /// Returns the scene buttons of a room page, or no buttons on the index
    /// page.
    #[must_use]
    pub fn scenes(&self) -> &[SceneView] {
        match &self.page {
            PageView::Index(_) => &[],
            PageView::Room(room) => &room.scenes,
        }
    }

    /// Returns the index page, or `None` on a room page.
    #[must_use]
    pub const fn index(&self) -> Option<&IndexView> {
        match &self.page {
            PageView::Index(index) => Some(index),
            PageView::Room(_) => None,
        }
    }
}

/// The content of a page.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum PageView {
    /// The home page: the index of the rooms.
    Index(IndexView),
    /// The page of one room.
    Room(RoomView),
}

/// What a renderer needs to draw the index page.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IndexView {
    /// The weather, from the first weather tile of the home page, or `None`
    /// if the home page has no weather tile. The text is the temperature and
    /// the detail is the condition.
    pub weather: Option<TileView>,
    /// The All lights toggle, from the first light or switch tile of the
    /// home page, or `None` if the home page has no such tile. A tap on it
    /// sends `Control::AllLights`.
    pub all_lights: Option<TileView>,
    /// A button for each room, in the order of the pages. A tap on a button
    /// sends `Control::Room` with its index in this list.
    pub rooms: Vec<RoomSummary>,
}

/// What a renderer needs to draw the button of one room on the index page.
/// Refer to [`summarise`](crate::summary::summarise).
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoomSummary {
    /// The name of the room.
    pub title: String,
    /// The state of each light and switch of the room, in the order of its
    /// tiles. A renderer draws one dot for each. The list does not have a
    /// group light if the room has other lights or switches. Refer to
    /// [`lights`](crate::summary::lights).
    pub lights: Vec<LightState>,
    /// The temperature of the room, for example `20.4°`, or `None` if the
    /// room has no temperature. Refer to
    /// [`temperature`](crate::summary::temperature).
    pub temperature: Option<String>,
    /// At least one light or switch of the room is on. A renderer draws the
    /// button black, as a light tile that is on.
    pub any_on: bool,
}

/// The state of one light or switch in the summary of a room.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum LightState {
    /// The light or switch is on.
    On,
    /// The light or switch is off.
    Off,
    /// The state is not `on` or `off`: for example, Home Assistant cannot
    /// reach the light, or the core has no state for it yet.
    Unavailable,
}

/// What a renderer needs to draw the page of one room.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RoomView {
    /// The tiles on the page, in reading order.
    pub tiles: Vec<TileView>,
    /// The scene buttons on the page, in reading order.
    pub scenes: Vec<SceneView>,
}

/// What a renderer needs to draw one tile.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TileView {
    /// The text on the tile.
    pub label: String,
    /// The Home Assistant entity id.
    pub entity_id: String,
    /// The kind of entity.
    pub kind: TileKind,
    /// How the tile shows the entity.
    pub state: TileState,
    /// The main value as text, for example `On 60%` or `21.5 °C`. A renderer
    /// shows it in a large font.
    pub text: String,
    /// More text for the value, or an empty string. A climate tile shows the
    /// target temperature here, for example `Set 21°`. A weather tile shows
    /// the condition here, for example `Partly cloudy`. A light tile with
    /// steps (refer to [`TileView::steps`]) that is on shows its brightness
    /// here, for example `50%`. A renderer shows
    /// it in a smaller font.
    pub detail: String,
    /// The tile has the − and + buttons: it is a dimmable light that is on
    /// or off. Refer to [`Tile::has_steps`]. On a room page, a renderer
    /// draws the buttons, and a tap on them sends `Control::Dimmer` and
    /// `Control::Brighter`. If `steps` is `false`, a renderer draws the tile
    /// as the tile of an on/off light.
    pub steps: bool,
}

/// What a renderer needs to draw one scene button.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SceneView {
    /// The text on the button.
    pub label: String,
    /// The Home Assistant entity id.
    pub entity_id: String,
    /// The core sent the service call of the scene and waits a short time.
    /// A renderer draws the button as a pending tile. Refer to
    /// [`TileState::Pending`].
    pub pending: bool,
}

/// The connection to Home Assistant, as the dashboard shows it.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Connection {
    /// The dashboard has no connection and does not try to connect.
    Offline,
    /// The dashboard tries to connect.
    Connecting,
    /// The dashboard is connected and gets live updates.
    Online,
}

/// The step of the charge of the battery in the view, in percent. Refer to
/// [`ViewModel::battery`].
pub const BATTERY_STEP: u8 = 5;

/// Returns `level` rounded to the nearest [`BATTERY_STEP`], at most 100.
#[must_use]
pub const fn rounded_level(level: u8) -> u8 {
    let level = if level > 100 { 100 } else { level };
    (level + BATTERY_STEP / 2) / BATTERY_STEP * BATTERY_STEP
}

/// Makes the view model from the model.
#[must_use]
pub fn view(model: &Model) -> ViewModel {
    let index = model.page_index();
    let page = model.pages.get(index);

    let connection = if model.connection.mqtt == Link::Up {
        Connection::Online
    } else if model.wants_online() {
        Connection::Connecting
    } else {
        Connection::Offline
    };
    let time = page_time(model);
    let updated = time.map_or_else(String::new, |time| {
        format!("Last changed {}", local_time(time))
    });

    let read_only = model
        .settings
        .as_ref()
        .map_or(model.read_only_requested, |settings| settings.read_only);

    ViewModel {
        path: model.route.to_path(),
        title: page.map(|page| page.title).unwrap_or_default().into(),
        page_index: index,
        page_count: model.pages.len(),
        page_label: format!("{} / {}", index + 1, model.pages.len()),
        page: page.map_or_else(
            || {
                PageView::Room(RoomView {
                    tiles: Vec::new(),
                    scenes: Vec::new(),
                })
            },
            |page| page_view(page, model),
        ),
        connection,
        power: model.power.source,
        battery: model.power.battery.map(rounded_level),
        status: status(connection, model.power.asleep, model.unsent, read_only),
        last_sync: time.and_then(|time| u64::try_from(time.timestamp()).ok()),
        asleep: model.power.asleep,
        read_only,
        updated,
        full_refresh_seq: model.full_refresh_seq,
    }
}

/// Makes the content of `page`. The home page is the index of the rooms.
fn page_view(page: &Page, model: &Model) -> PageView {
    let entities = &model.entities;
    if page.is_room() {
        return PageView::Room(RoomView {
            tiles: page
                .tiles
                .iter()
                .map(|tile| tile_view(tile, model))
                .collect(),
            scenes: page
                .scenes
                .iter()
                .map(|scene| SceneView {
                    label: scene.label.into(),
                    entity_id: scene.entity_id.into(),
                    pending: is_pending(model, scene.entity_id),
                })
                .collect(),
        });
    }
    PageView::Index(IndexView {
        weather: page.weather_tile().map(|tile| tile_view(tile, model)),
        all_lights: page.all_lights_tile().map(|tile| tile_view(tile, model)),
        rooms: model
            .rooms()
            .map(|page| summarise(page, entities))
            .collect(),
    })
}

/// Makes the view of one tile.
///
/// A tile that is on or off and has a mark has the state
/// [`TileState::Pending`]. It keeps its text and its detail, so the layout
/// does not move. A tile in another state keeps that state, because the
/// renderer draws it with a different layout.
fn tile_view(tile: &Tile, model: &Model) -> TileView {
    let entity = model.entities.get(tile.entity_id);
    let Description {
        state,
        text,
        detail,
    } = describe(tile.kind, entity);
    let steps = tile.has_steps(entity.and_then(|entity| entity.state.as_deref()));
    let detail = match entity.and_then(light_percent) {
        Some(percent) if steps && state == TileState::On => format!("{percent}%"),
        _ => detail,
    };
    let on_or_off = matches!(state, TileState::On | TileState::Off);
    let state = if on_or_off && is_pending(model, tile.entity_id) {
        TileState::Pending
    } else {
        state
    };
    TileView {
        label: tile.label.into(),
        entity_id: tile.entity_id.into(),
        kind: tile.kind,
        state,
        text,
        detail,
        steps,
    }
}

/// Returns the UK time of day of `time` as `HH:MM`. Refer to
/// [`uk_time`](crate::local_time::uk_time).
fn local_time(time: DateTime<Utc>) -> String {
    let local = uk_time(time);
    format!("{:02}:{:02}", local.hour(), local.minute())
}

/// Returns the status text: the dry run, the connection and the number of
/// taps that the core did not send.
fn status(connection: Connection, asleep: bool, unsent: usize, read_only: bool) -> String {
    let connection = match connection {
        _ if asleep => "Asleep",
        Connection::Online => "Online",
        Connection::Connecting => "Connecting",
        Connection::Offline => "Offline",
    };
    let text = match unsent {
        0 => String::from(connection),
        1 => format!("{connection}, 1 tap not sent"),
        _ => format!("{connection}, {unsent} taps not sent"),
    };
    if read_only {
        format!("Dry run · {text}")
    } else {
        text
    }
}

/// How a tile shows its entity.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum TileState {
    /// The light or switch is on.
    On,
    /// The light or switch is off.
    Off,
    /// The tile shows a value, for example a temperature.
    Value,
    /// Home Assistant cannot reach the entity.
    Unavailable,
    /// The core has no state for the entity yet, or the state is `unknown`.
    Unknown,
    /// The light or switch was on or off, and the core sent a service call
    /// for it that Home Assistant did not confirm yet. The tile keeps the
    /// text of the last state. Refer to [`pending`](crate::features::pending).
    Pending,
}

/// How a tile shows its entity, and the text on the tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    /// How the tile shows the entity.
    pub state: TileState,
    /// The main value, which a renderer shows in a large font.
    pub text: String,
    /// More text for the value, or an empty string.
    pub detail: String,
}

impl Description {
    fn new(state: TileState, text: impl Into<String>) -> Self {
        Self {
            state,
            text: text.into(),
            detail: String::new(),
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// Returns how a tile of `kind` shows `entity`, and the text on the tile.
///
/// The core shows a timestamp state, for example the finish time of a
/// dishwasher, as a UK time, with the offset from UTC at that time.
#[must_use]
pub fn describe(kind: TileKind, entity: Option<&EntityState>) -> Description {
    let Some((entity, state)) =
        entity.and_then(|entity| entity.state.as_deref().map(|state| (entity, state)))
    else {
        return Description::new(TileState::Unknown, "No data");
    };
    match state {
        "unavailable" => return Description::new(TileState::Unavailable, "Unavailable"),
        "unknown" => return Description::new(TileState::Unknown, "No data"),
        _ => {}
    }

    match kind {
        TileKind::Light | TileKind::Switch => match state {
            "on" => {
                let percent = light_percent(entity);
                let text = percent.map_or_else(|| "On".into(), |percent| format!("On {percent}%"));
                Description::new(TileState::On, text)
            }
            "off" => Description::new(TileState::Off, "Off"),
            other => Description::new(TileState::Value, capitalise(other)),
        },
        TileKind::Sensor => {
            let text = match entity.attribute("unit_of_measurement") {
                Some(unit) if !unit.is_empty() => format!("{state} {unit}"),
                _ => DateTime::parse_from_rfc3339(state).map_or_else(
                    |_| capitalise(state),
                    |time| local_time(time.with_timezone(&Utc)),
                ),
            };
            Description::new(TileState::Value, text)
        }
        TileKind::Climate => {
            let tile_state = if state == "off" {
                TileState::Off
            } else {
                TileState::Value
            };
            let current = entity.attribute("current_temperature");
            let target = entity.attribute("temperature");
            match (current, target) {
                (Some(current), Some(target)) => {
                    Description::new(tile_state, format!("{current}°"))
                        .with_detail(format!("Set {target}°"))
                }
                (Some(current), None) => Description::new(tile_state, format!("{current}°")),
                (None, Some(target)) => {
                    Description::new(tile_state, "-").with_detail(format!("Set {target}°"))
                }
                (None, None) => Description::new(tile_state, capitalise(state)),
            }
        }
        TileKind::Weather => {
            let condition = weather_condition(state);
            match entity.attribute("temperature") {
                Some(temperature) => {
                    let unit = entity.attribute("temperature_unit").unwrap_or("°");
                    Description::new(TileState::Value, format!("{temperature} {unit}"))
                        .with_detail(condition)
                }
                None => Description::new(TileState::Value, condition),
            }
        }
    }
}

/// Returns the brightness of a light as a percentage, or `None` if the
/// entity has no valid `brightness` attribute.
pub(crate) fn light_percent(entity: &EntityState) -> Option<u32> {
    entity
        .attribute("brightness")
        .and_then(|value| value.parse::<u32>().ok())
        .map(brightness_percent)
}

/// Converts a Home Assistant brightness (0 to 255) to a percentage.
/// A light that is on shows at least 1%.
fn brightness_percent(brightness: u32) -> u32 {
    ((brightness.min(255) * 100 + 127) / 255).max(1)
}

/// Turns a Home Assistant weather condition into text, for example
/// `partlycloudy` into `Partly cloudy`.
fn weather_condition(state: &str) -> String {
    let text = match state {
        "clear-night" => "Clear",
        "partlycloudy" => "Partly cloudy",
        "lightning-rainy" => "Thunder and rain",
        "snowy-rainy" => "Sleet",
        "windy-variant" => "Windy",
        "exceptional" => "Unusual weather",
        other => return capitalise(&other.replace('-', " ")),
    };
    text.into()
}

/// Makes the first letter of `text` a capital letter.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}
