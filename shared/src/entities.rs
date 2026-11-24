//! The entity states that the core gets from the Home Assistant state stream,
//! and the text that a tile shows for them.
//!
//! The Home Assistant `mqtt_statestream` integration publishes each entity on
//! retained topics of this form:
//!
//! - `homeassistant/statestream/<domain>/<object_id>/state`: the state as plain
//!   text, for example `on`.
//! - `homeassistant/statestream/<domain>/<object_id>/last_updated`: the time
//!   of the last update as plain ISO 8601 text, for example
//!   `2026-11-08T14:03:21.123456+00:00`. Home Assistant publishes it because
//!   the integration has `publish_timestamps: true`.
//! - `homeassistant/statestream/<domain>/<object_id>/<attribute>`: each
//!   attribute as JSON, for example `128` or `"°C"`.
//!
//! The core uses `last_updated` and not `last_changed`. Home Assistant moves
//! `last_changed` only when the state changes. The current temperature of a
//! thermostat, the temperature of the weather and the brightness of a light
//! are attributes, so a change to them does not move `last_changed`. A change
//! to the state or to an attribute moves `last_updated`.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};

use jiff::Timestamp;

use crate::layout::Page;

/// The topic prefix of the Home Assistant state stream.
pub const STATESTREAM: &str = "homeassistant/statestream";

/// The last part of the topic that gives the time of the last update of an
/// entity. Refer to the module documentation.
pub const LAST_UPDATED: &str = "last_updated";

/// The state of one entity, as far as the dashboard needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityState {
    /// The state, for example `on`, `off`, `21.5` or `unavailable`.
    pub state: Option<String>,
    /// The attributes that the tile shows, as text.
    pub attributes: BTreeMap<String, String>,
    /// The exact time at which Home Assistant last updated the state or an
    /// attribute. The core compares it with the time at a tap, to see that
    /// Home Assistant confirmed the tap. Refer to
    /// [`pending`](crate::features::pending). Only the minute shows on the
    /// screen ([`EntityState::last_updated_minute`]). Refer to the module
    /// documentation for the reason that the core uses `last_updated` and not
    /// `last_changed`.
    pub last_updated: Option<Timestamp>,
}

impl EntityState {
    /// Returns the attribute `name`, if the core has it.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }

    /// Returns the time of the last update at the start of its minute. Only
    /// the minute shows on the screen.
    #[must_use]
    pub fn last_updated_minute(&self) -> Option<Timestamp> {
        self.last_updated.map(start_of_minute)
    }
}

/// The entity states, by entity id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entities {
    states: BTreeMap<String, EntityState>,
}

impl Entities {
    /// Returns the state of `entity_id`, if the core has it.
    #[must_use]
    pub fn get(&self, entity_id: &str) -> Option<&EntityState> {
        self.states.get(entity_id)
    }

    /// Returns the state text of `entity_id`, for example `on`, or `None` if
    /// the core has no state for it.
    #[must_use]
    pub fn state(&self, entity_id: &str) -> Option<&str> {
        self.get(entity_id)?.state.as_deref()
    }

    /// Returns each entity id with the time of its last update at the start
    /// of its minute, for the entities that have one.
    pub fn last_updated_minutes(&self) -> impl Iterator<Item = (&str, Timestamp)> {
        self.states.iter().filter_map(|(entity_id, entity)| {
            Some((entity_id.as_str(), entity.last_updated_minute()?))
        })
    }

    /// Applies a state-stream message to the states.
    ///
    /// The core keeps only the entities on `pages`, the attributes that their
    /// tiles show, and the time of the last update. Returns the entity id if
    /// the message changed the state, an attribute or the minute of the last
    /// update. A change of the exact time in the same minute is not a change
    /// for the return value. The core ignores a time that is not valid ISO 8601 text.
    pub fn apply(&mut self, pages: &[Page], topic: &str, payload: &[u8]) -> Option<String> {
        let (entity_id, key) = parse_topic(topic)?;
        let kind = pages
            .iter()
            .flat_map(|page| page.tiles)
            .find(|tile| tile.entity_id == entity_id)?
            .kind;
        let is_state = key == "state";
        if !is_state && key != LAST_UPDATED && !kind.attributes().contains(&key) {
            return None;
        }
        let text = core::str::from_utf8(payload).ok()?.trim();

        if key == LAST_UPDATED {
            let exact = text.parse::<Timestamp>().ok()?;
            let entity = self.states.entry(entity_id.clone()).or_default();
            let changed = entity.last_updated_minute() != Some(start_of_minute(exact));
            entity.last_updated = Some(exact);
            return changed.then_some(entity_id);
        }

        let entity = self.states.entry(entity_id.clone()).or_default();
        let changed = if is_state {
            // The state is plain text, not JSON.
            let changed = entity.state.as_deref() != Some(text);
            entity.state = Some(text.to_string());
            changed
        } else {
            let value = json_text(text);
            let changed = entity.attribute(key) != value.as_deref();
            match value {
                Some(value) => entity.attributes.insert(key.to_string(), value),
                None => entity.attributes.remove(key),
            };
            changed
        };
        changed.then_some(entity_id)
    }
}

/// Returns the entity id of a state-stream topic, or `None` if the topic is
/// not a state-stream topic.
#[must_use]
pub fn entity_of(topic: &str) -> Option<String> {
    parse_topic(topic).map(|(entity_id, _)| entity_id)
}

/// Splits a state-stream topic into the entity id and the last part of the
/// topic (`state` or an attribute name).
fn parse_topic(topic: &str) -> Option<(String, &str)> {
    let rest = topic.strip_prefix(STATESTREAM)?.strip_prefix('/')?;
    let mut parts = rest.split('/');
    let (domain, object_id, key) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || domain.is_empty() || object_id.is_empty() {
        return None;
    }
    Some((format!("{domain}.{object_id}"), key))
}

/// Returns the start of the minute of `time`. The UTC offset of the UK is a
/// whole number of minutes, so the start of the minute shows the same
/// `HH:MM`.
fn start_of_minute(time: Timestamp) -> Timestamp {
    let seconds = time.as_second().div_euclid(60) * 60;
    Timestamp::from_second(seconds).unwrap_or(time)
}

/// Reads a JSON string or number as text. Returns `None` for `null` and for
/// text that is not a JSON string or number.
fn json_text(text: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(text).ok()? {
        serde_json::Value::String(value) => Some(value),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// The topics that subscribe to the entities on `pages`.
///
/// For each entity, there is one topic for the state, one topic for the time
/// of the last update (`last_updated`), and one topic for each attribute that
/// its tile shows. The core does not subscribe to other attributes or to
/// `last_changed`, so a wake on battery gets only the retained messages that
/// the dashboard uses.
#[must_use]
pub fn topics(pages: &[Page]) -> Vec<String> {
    let mut topics: Vec<String> = pages
        .iter()
        .flat_map(|page| page.tiles)
        .filter_map(|tile| {
            let (domain, object_id) = tile.entity_id.split_once('.')?;
            let base = format!("{STATESTREAM}/{domain}/{object_id}");
            let names = ["state", LAST_UPDATED]
                .into_iter()
                .chain(tile.kind.attributes().iter().copied());
            Some(names.map(move |name| format!("{base}/{name}")))
        })
        .flatten()
        .collect();
    topics.sort_unstable();
    topics.dedup();
    topics
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::String, vec};
    use jiff::tz::Offset;

    use super::{Entities, topics};
    use crate::{
        layout::{Page, TileKind, climate, home, light, sensor, weather},
        view::{TileState, describe},
    };

    const PAGES: &[Page] = &[home(
        "Test",
        &[
            light("Lamp", "light.lamp"),
            sensor("Temperature", "sensor.temperature"),
            climate("Thermostat", "climate.thermostat"),
            weather("Weather", "weather.home"),
        ],
        &[],
    )];

    fn text(entities: &Entities, kind: TileKind, entity_id: &str) -> (TileState, String) {
        let description = describe(kind, entities.get(entity_id));
        (description.state, description.text)
    }

    fn detail(entities: &Entities, kind: TileKind, entity_id: &str) -> String {
        describe(kind, entities.get(entity_id)).detail
    }

    #[test]
    fn a_state_message_sets_the_state() {
        let mut entities = Entities::default();
        let changed = entities.apply(PAGES, "homeassistant/statestream/light/lamp/state", b"on");
        assert_eq!(changed.as_deref(), Some("light.lamp"));
        assert_eq!(
            text(&entities, TileKind::Light, "light.lamp"),
            (TileState::On, "On".into())
        );
    }

    #[test]
    fn the_same_state_again_is_not_a_change() {
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/light/lamp/state";
        assert!(entities.apply(PAGES, topic, b"off").is_some());
        assert_eq!(entities.apply(PAGES, topic, b"off"), None);
    }

    #[test]
    fn a_light_shows_its_brightness_as_a_percentage() {
        let mut entities = Entities::default();
        entities.apply(PAGES, "homeassistant/statestream/light/lamp/state", b"on");
        entities.apply(
            PAGES,
            "homeassistant/statestream/light/lamp/brightness",
            b"128",
        );
        assert_eq!(
            text(&entities, TileKind::Light, "light.lamp"),
            (TileState::On, "On 50%".into())
        );

        entities.apply(PAGES, "homeassistant/statestream/light/lamp/state", b"off");
        assert_eq!(
            text(&entities, TileKind::Light, "light.lamp"),
            (TileState::Off, "Off".into())
        );
    }

    #[test]
    fn a_null_attribute_removes_the_attribute() {
        let mut entities = Entities::default();
        entities.apply(PAGES, "homeassistant/statestream/light/lamp/state", b"on");
        entities.apply(
            PAGES,
            "homeassistant/statestream/light/lamp/brightness",
            b"255",
        );
        let changed = entities.apply(
            PAGES,
            "homeassistant/statestream/light/lamp/brightness",
            b"null",
        );
        assert!(changed.is_some());
        assert_eq!(
            text(&entities, TileKind::Light, "light.lamp"),
            (TileState::On, "On".into())
        );
    }

    #[test]
    fn a_sensor_shows_its_unit_from_a_json_attribute() {
        let mut entities = Entities::default();
        let base = "homeassistant/statestream/sensor/temperature";
        entities.apply(PAGES, &alloc::format!("{base}/state"), b"21.5");
        entities.apply(
            PAGES,
            &alloc::format!("{base}/unit_of_measurement"),
            "\"°C\"".as_bytes(),
        );
        assert_eq!(
            text(&entities, TileKind::Sensor, "sensor.temperature"),
            (TileState::Value, "21.5 °C".into())
        );
    }

    #[test]
    fn a_climate_tile_shows_the_current_and_the_target_temperature() {
        let mut entities = Entities::default();
        let base = "homeassistant/statestream/climate/thermostat";
        entities.apply(PAGES, &alloc::format!("{base}/state"), b"heat");
        entities.apply(
            PAGES,
            &alloc::format!("{base}/current_temperature"),
            b"19.5",
        );
        entities.apply(PAGES, &alloc::format!("{base}/temperature"), b"21");
        assert_eq!(
            text(&entities, TileKind::Climate, "climate.thermostat"),
            (TileState::Value, "19.5°".into())
        );
        assert_eq!(
            detail(&entities, TileKind::Climate, "climate.thermostat"),
            "Set 21°"
        );
    }

    #[test]
    fn a_climate_tile_with_only_a_target_shows_the_target_as_the_detail() {
        let mut entities = Entities::default();
        let base = "homeassistant/statestream/climate/thermostat";
        entities.apply(PAGES, &alloc::format!("{base}/state"), b"heat");
        entities.apply(PAGES, &alloc::format!("{base}/temperature"), b"21");
        assert_eq!(
            text(&entities, TileKind::Climate, "climate.thermostat"),
            (TileState::Value, "-".into())
        );
        assert_eq!(
            detail(&entities, TileKind::Climate, "climate.thermostat"),
            "Set 21°"
        );
    }

    #[test]
    fn a_weather_tile_shows_the_condition_and_the_temperature() {
        let mut entities = Entities::default();
        let base = "homeassistant/statestream/weather/home";
        entities.apply(PAGES, &alloc::format!("{base}/state"), b"partlycloudy");
        entities.apply(PAGES, &alloc::format!("{base}/temperature"), b"12.3");
        entities.apply(
            PAGES,
            &alloc::format!("{base}/temperature_unit"),
            "\"°C\"".as_bytes(),
        );
        assert_eq!(
            text(&entities, TileKind::Weather, "weather.home"),
            (TileState::Value, "12.3 °C".into())
        );
        assert_eq!(
            detail(&entities, TileKind::Weather, "weather.home"),
            "Partly cloudy"
        );
    }

    #[test]
    fn unavailable_and_missing_states() {
        let mut entities = Entities::default();
        assert_eq!(
            text(&entities, TileKind::Light, "light.lamp"),
            (TileState::Unknown, "No data".into())
        );
        entities.apply(
            PAGES,
            "homeassistant/statestream/light/lamp/state",
            b"unavailable",
        );
        assert_eq!(
            text(&entities, TileKind::Light, "light.lamp"),
            (TileState::Unavailable, "Unavailable".into())
        );
    }

    #[test]
    fn messages_for_other_entities_attributes_and_topics_are_ignored() {
        let mut entities = Entities::default();
        let ignored = [
            ("homeassistant/statestream/light/other/state", "on"),
            (
                "homeassistant/statestream/light/lamp/friendly_name",
                "\"Lamp\"",
            ),
            ("homeassistant/statestream/light/lamp", "on"),
            ("homeassistant/statestream/light/lamp/state/extra", "on"),
            (
                "homeassistant/statestream/light/lamp/last_changed",
                "2026-11-08T14:03:21+00:00",
            ),
            (
                "homeassistant/statestream/light/other/last_updated",
                "2026-11-08T14:03:21+00:00",
            ),
            ("dashboard/hall/status", "online"),
        ];
        for (topic, payload) in ignored {
            assert_eq!(
                entities.apply(PAGES, topic, payload.as_bytes()),
                None,
                "{topic}"
            );
        }
        assert_eq!(entities, Entities::default());
    }

    #[test]
    fn topics_for_the_state_and_the_attributes_that_a_tile_shows() {
        let base = "homeassistant/statestream";
        assert_eq!(
            topics(PAGES),
            vec![
                format!("{base}/climate/thermostat/current_temperature"),
                format!("{base}/climate/thermostat/last_updated"),
                format!("{base}/climate/thermostat/state"),
                format!("{base}/climate/thermostat/temperature"),
                format!("{base}/light/lamp/brightness"),
                format!("{base}/light/lamp/last_updated"),
                format!("{base}/light/lamp/state"),
                format!("{base}/sensor/temperature/last_updated"),
                format!("{base}/sensor/temperature/state"),
                format!("{base}/sensor/temperature/unit_of_measurement"),
                format!("{base}/weather/home/last_updated"),
                format!("{base}/weather/home/state"),
                format!("{base}/weather/home/temperature"),
                format!("{base}/weather/home/temperature_unit"),
            ]
        );
    }

    #[test]
    fn a_state_payload_is_plain_text() {
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/sensor/temperature/state";
        let long = "Programme finished, the door can open now";
        for payload in ["21.50", "unavailable", "\"quoted\"", "null", long, " 7 "] {
            entities.apply(PAGES, topic, payload.as_bytes());
            let state = entities.get("sensor.temperature").unwrap().state.as_deref();
            assert_eq!(state, Some(payload.trim()), "{payload}");
        }
    }

    #[test]
    fn a_timestamp_sensor_shows_the_uk_time_at_that_time() {
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/sensor/temperature/state";
        let shown = |entities: &Entities| {
            describe(TileKind::Sensor, entities.get("sensor.temperature")).text
        };
        // In summer time, the UK time is UTC+1.
        entities.apply(PAGES, topic, b"2026-07-01T14:30:00+00:00");
        assert_eq!(shown(&entities), "15:30");
        // In winter, the UK time is UTC.
        entities.apply(PAGES, topic, b"2026-12-01T14:30:00+00:00");
        assert_eq!(shown(&entities), "14:30");
        // A time with another offset shows as UK time too.
        entities.apply(PAGES, topic, b"2026-07-01T16:30:00+02:00");
        assert_eq!(shown(&entities), "15:30");
    }

    /// Returns the time of the last update of the lamp, as RFC 3339 text.
    fn lamp_updated(entities: &Entities) -> Option<String> {
        entities
            .get("light.lamp")?
            .last_updated_minute()
            .map(|time| time.display_with_offset(Offset::UTC).to_string())
    }

    #[test]
    fn last_updated_keeps_the_start_of_the_minute() {
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/light/lamp/last_updated";
        // Python `isoformat()` gives microseconds, or none if they are 0.
        let changed = entities.apply(PAGES, topic, b"2026-11-08T14:03:21.123456+00:00");
        assert_eq!(changed.as_deref(), Some("light.lamp"));
        assert_eq!(
            lamp_updated(&entities).as_deref(),
            Some("2026-11-08T14:03:00+00:00")
        );

        // A later time in the same minute is not a change, but the core keeps
        // the exact time.
        assert_eq!(
            entities.apply(PAGES, topic, b"2026-11-08T14:03:59+00:00"),
            None
        );
        let exact = entities.get("light.lamp").unwrap().last_updated;
        assert_eq!(
            exact
                .map(|time| time.display_with_offset(Offset::UTC).to_string())
                .as_deref(),
            Some("2026-11-08T14:03:59+00:00")
        );
        // A time in the next minute is a change. Another offset is the same
        // instant in UTC.
        let changed = entities.apply(PAGES, topic, b"2026-11-08T15:04:00+01:00");
        assert_eq!(changed.as_deref(), Some("light.lamp"));
        assert_eq!(
            lamp_updated(&entities).as_deref(),
            Some("2026-11-08T14:04:00+00:00")
        );
    }

    #[test]
    fn a_last_updated_that_is_not_valid_is_ignored() {
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/light/lamp/last_updated";
        for payload in ["", "soon", "\"2026-11-08T14:03:21+00:00\"", "null"] {
            assert_eq!(
                entities.apply(PAGES, topic, payload.as_bytes()),
                None,
                "{payload}"
            );
        }
        assert_eq!(entities, Entities::default());

        entities.apply(PAGES, topic, b"2026-11-08T14:03:21+00:00");
        assert_eq!(entities.apply(PAGES, topic, b"2026-11-08T"), None);
        assert_eq!(
            lamp_updated(&entities).as_deref(),
            Some("2026-11-08T14:03:00+00:00")
        );
    }

    #[test]
    fn each_tile_kind_keeps_last_updated() {
        let mut entities = Entities::default();
        for entity_id in [
            "light.lamp",
            "sensor.temperature",
            "climate.thermostat",
            "weather.home",
        ] {
            let (domain, object_id) = entity_id.split_once('.').unwrap();
            let topic = format!("homeassistant/statestream/{domain}/{object_id}/last_updated");
            let changed = entities.apply(PAGES, &topic, b"2026-11-08T14:03:21+00:00");
            assert_eq!(changed.as_deref(), Some(entity_id));
        }
        assert_eq!(entities.last_updated_minutes().count(), 4);
    }
}
