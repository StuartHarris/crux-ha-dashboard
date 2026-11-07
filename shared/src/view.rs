//! The view model and the presentation decisions.
//!
//! The decisions are the text and the state of each tile, the connection
//! indicator and the "Live · HH:MM" or "Updated HH:MM" text. The `ui` crate
//! only draws what this module gives it.

use alloc::{format, string::String, vec::Vec};

use chrono::{DateTime, Timelike, Utc};
use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::{
    Model, capabilities::power::PowerSource, entities::EntityState, features::connection::Link,
    layout::TileKind, local_time::uk_time,
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
    /// sent`. In a dry run, the text starts with `Dry run · `, so the panel
    /// shows why a tap does not change a light.
    pub status: String,
    /// The device is asleep. The panel keeps this image until the device wakes.
    pub asleep: bool,
    /// The dashboard does a dry run: a tap on a tile or a scene sends no
    /// service call. The status text shows it on the panel. A shell can
    /// also show it. For example, the simulator draws a red border round the
    /// panel when the dry run stops.
    pub read_only: bool,
    /// The time at which the last state message arrived, in seconds since
    /// the Unix epoch. The shell keeps it during a deep sleep and gives it
    /// back in `Start`.
    pub last_sync: Option<u64>,
    /// The freshness of the data, with the UK time of the last state message
    /// as `HH:MM`.
    ///
    /// - While the dashboard is connected, the text is `Live · HH:MM`, or
    ///   `Live` if the time is not known.
    /// - Without a connection, the text is `Updated HH:MM`, or empty if the
    ///   time is not known.
    ///
    /// The time is not a clock. It changes only when a message arrives in a
    /// new minute, so the panel does not refresh each minute.
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
    /// the condition here, for example `Partly cloudy`. A renderer shows it in
    /// a smaller font.
    pub detail: String,
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
        (Connection::Online, Some(time)) => {
            format!("Live · {}", local_time(time))
        }
        (Connection::Online, None) => "Live".into(),
        (_, Some(time)) => format!("Updated {}", local_time(time)),
        (_, None) => String::new(),
    };

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
        tiles: tiles
            .iter()
            .map(|tile| {
                let Description {
                    state,
                    text,
                    detail,
                } = describe(tile.kind, model.entities.get(tile.entity_id));
                TileView {
                    label: tile.label.into(),
                    entity_id: tile.entity_id.into(),
                    kind: tile.kind,
                    state,
                    text,
                    detail,
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
            read_only,
        ),
        last_sync: model
            .last_sync
            .and_then(|time| u64::try_from(time.timestamp()).ok()),
        asleep: model.power.asleep,
        read_only,
        updated,
        full_refresh_seq: model.full_refresh_seq,
    }
}

/// Returns the UK time of day of `time` as `HH:MM`. Refer to
/// [`uk_time`](crate::local_time::uk_time).
fn local_time(time: DateTime<Utc>) -> String {
    let local = uk_time(time);
    format!("{:02}:{:02}", local.hour(), local.minute())
}

/// Returns the status text: the dry run, the connection, the power source
/// and the number of taps that the core did not send.
fn status(
    connection: Connection,
    power: PowerSource,
    asleep: bool,
    unsent: usize,
    read_only: bool,
) -> String {
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
    let text = match unsent {
        0 => format!("{connection}, {power}"),
        1 => format!("{connection}, {power}, 1 tap not sent"),
        _ => format!("{connection}, {power}, {unsent} taps not sent"),
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
        return Description::new(TileState::Unknown, "-");
    };
    match state {
        "unavailable" => return Description::new(TileState::Unavailable, "Unavailable"),
        "unknown" => return Description::new(TileState::Unknown, "-"),
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
