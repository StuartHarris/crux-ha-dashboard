//! The view model and the presentation decisions: the text and the state of
//! each tile, the connection indicator and the "Updated HH:MM" text. The `ui`
//! crate only draws what this module gives it.

use alloc::{format, string::String, vec::Vec};

use chrono::{DateTime, FixedOffset, Timelike, Utc};
use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::{
    Model, capabilities::power::PowerSource, entities::EntityState, features::connection::Link,
    layout::TileKind,
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
    /// The tiles on the current page, in reading order.
    pub tiles: Vec<TileView>,
    /// The scene buttons on the current page, in reading order.
    pub scenes: Vec<SceneView>,
    /// The connection to Home Assistant.
    pub connection: Connection,
    /// The power source.
    pub power: PowerSource,
    /// The connection and the power source as text, for example
    /// `Online, on stand`. If the core dropped taps before it slept, the text
    /// also gives their number, for example `Asleep, on battery, 1 tap not
    /// sent`.
    pub status: String,
    /// The device is asleep. The panel keeps this image until the device wakes.
    pub asleep: bool,
    /// The time of the last update, in seconds since the Unix epoch. The
    /// shell keeps it during a deep sleep and gives it back in `Start`.
    pub last_sync: Option<u64>,
    /// `Live` while the dashboard is connected, then `Updated HH:MM` with the
    /// local time of the last connection. The text is empty before the first
    /// connection.
    pub updated: String,
    /// A number that the core increases each time that the panel needs a
    /// full refresh, to clear ghost images.
    ///
    /// The core asks for a full refresh when the whole screen changes: after
    /// a start or a wake, on a page change, and before sleep. The shell keeps
    /// the number of its last full refresh. If the number in the view is
    /// different, the shell does a full refresh and keeps the new number.
    /// This works also when the shell draws once for many `Render` requests.
    /// The shell can also do a full refresh after a number of partial
    /// refreshes.
    pub full_refresh_seq: u32,
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
    /// The state as text, for example `On 60%` or `21.5 °C`.
    pub text: String,
}

/// What a renderer needs to draw one scene button.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SceneView {
    /// The text on the button.
    pub label: String,
    /// The Home Assistant entity id.
    pub entity_id: String,
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

/// Makes the view model from the model.
#[must_use]
pub fn view(model: &Model) -> ViewModel {
    let index = model.page_index();
    let page = model.pages.get(index);
    let tiles = page.map_or(&[][..], |page| page.tiles);
    let scenes = page.map_or(&[][..], |page| page.scenes);

    let connection = if model.connection.mqtt == Link::Up {
        Connection::Online
    } else if model.wants_online() {
        Connection::Connecting
    } else {
        Connection::Offline
    };
    let updated = match (connection, model.last_sync) {
        (Connection::Online, _) => "Live".into(),
        (_, Some(time)) => format!("Updated {}", local_time(time, model.utc_offset_minutes)),
        (_, None) => String::new(),
    };

    ViewModel {
        path: model.route.to_path(),
        title: page.map(|page| page.title).unwrap_or_default().into(),
        page_index: index,
        page_count: model.pages.len(),
        page_label: format!("{} / {}", index + 1, model.pages.len()),
        tiles: tiles
            .iter()
            .map(|tile| {
                let (state, text) = describe(
                    tile.kind,
                    model.entities.get(tile.entity_id),
                    model.utc_offset_minutes,
                );
                TileView {
                    label: tile.label.into(),
                    entity_id: tile.entity_id.into(),
                    kind: tile.kind,
                    state,
                    text,
                }
            })
            .collect(),
        scenes: scenes
            .iter()
            .map(|scene| SceneView {
                label: scene.label.into(),
                entity_id: scene.entity_id.into(),
            })
            .collect(),
        connection,
        power: model.power.source,
        status: status(
            connection,
            model.power.source,
            model.power.asleep,
            model.unsent,
        ),
        last_sync: model
            .last_sync
            .and_then(|time| u64::try_from(time.timestamp()).ok()),
        asleep: model.power.asleep,
        updated,
        full_refresh_seq: model.full_refresh_seq,
    }
}

/// Returns the local time of day of `time` as `HH:MM`.
fn local_time(time: DateTime<Utc>, utc_offset_minutes: i16) -> String {
    let local = FixedOffset::east_opt(i32::from(utc_offset_minutes) * 60).map_or_else(
        || time.naive_utc(),
        |offset| time.with_timezone(&offset).naive_local(),
    );
    format!("{:02}:{:02}", local.hour(), local.minute())
}

/// Returns the status text: the connection, the power source and the number
/// of taps that the core did not send.
fn status(connection: Connection, power: PowerSource, asleep: bool, unsent: usize) -> String {
    let connection = match connection {
        _ if asleep => "Asleep",
        Connection::Online => "Online",
        Connection::Connecting => "Connecting",
        Connection::Offline => "Offline",
    };
    let power = match power {
        PowerSource::Stand => "on stand",
        PowerSource::Battery => "on battery",
    };
    match unsent {
        0 => format!("{connection}, {power}"),
        1 => format!("{connection}, {power}, 1 tap not sent"),
        _ => format!("{connection}, {power}, {unsent} taps not sent"),
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
}

/// Returns how a tile of `kind` shows `entity`, and the text on the tile.
///
/// The core shows a timestamp state, for example the finish time of a
/// dishwasher, as a local time. `utc_offset_minutes` is the local offset.
#[must_use]
pub fn describe(
    kind: TileKind,
    entity: Option<&EntityState>,
    utc_offset_minutes: i16,
) -> (TileState, String) {
    let Some(entity) = entity else {
        return (TileState::Unknown, "-".into());
    };
    let Some(state) = entity.state.as_deref() else {
        return (TileState::Unknown, "-".into());
    };
    match state {
        "unavailable" => return (TileState::Unavailable, "Unavailable".into()),
        "unknown" => return (TileState::Unknown, "-".into()),
        _ => {}
    }

    match kind {
        TileKind::Light | TileKind::Switch => match state {
            "on" => {
                let percent = entity
                    .attribute("brightness")
                    .and_then(|value| value.parse::<u32>().ok())
                    .map(brightness_percent);
                let text = percent.map_or_else(|| "On".into(), |percent| format!("On {percent}%"));
                (TileState::On, text)
            }
            "off" => (TileState::Off, "Off".into()),
            other => (TileState::Value, capitalise(other)),
        },
        TileKind::Sensor => {
            let text = match entity.attribute("unit_of_measurement") {
                Some(unit) if !unit.is_empty() => format!("{state} {unit}"),
                _ => DateTime::parse_from_rfc3339(state).map_or_else(
                    |_| capitalise(state),
                    |time| local_time(time.with_timezone(&Utc), utc_offset_minutes),
                ),
            };
            (TileState::Value, text)
        }
        TileKind::Climate => {
            let current = entity.attribute("current_temperature");
            let target = entity.attribute("temperature");
            let text = match (current, target) {
                (Some(current), Some(target)) => format!("{current}° (set {target}°)"),
                (Some(current), None) => format!("{current}°"),
                (None, Some(target)) => format!("Set {target}°"),
                (None, None) => capitalise(state),
            };
            let tile_state = if state == "off" {
                TileState::Off
            } else {
                TileState::Value
            };
            (tile_state, text)
        }
        TileKind::Weather => {
            let condition = weather_condition(state);
            let text = match entity.attribute("temperature") {
                Some(temperature) => {
                    let unit = entity.attribute("temperature_unit").unwrap_or("°");
                    format!("{condition}, {temperature} {unit}")
                }
                None => condition,
            };
            (TileState::Value, text)
        }
    }
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
