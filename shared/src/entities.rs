//! The entity states that the core gets from the Home Assistant state stream,
//! and the text that a tile shows for them.
//!
//! The Home Assistant `mqtt_statestream` integration publishes each entity on
//! retained topics of this form:
//!
//! - `homeassistant/statestream/<domain>/<object_id>/state`: the state as plain
//!   text, for example `on`.
//! - `homeassistant/statestream/<domain>/<object_id>/<attribute>`: each
//!   attribute as JSON, for example `128` or `"°C"`.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::layout::Page;

/// The topic prefix of the Home Assistant state stream.
pub const STATESTREAM: &str = "homeassistant/statestream";

/// The state of one entity, as far as the dashboard needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityState {
    /// The state, for example `on`, `off`, `21.5` or `unavailable`.
    pub state: Option<String>,
    /// The attributes that the tile shows, as text.
    pub attributes: BTreeMap<String, String>,
}

impl EntityState {
    /// Returns the attribute `name`, if the core has it.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
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

    /// Applies a state-stream message to the states.
    ///
    /// The core keeps only the entities on `pages` and the attributes that
    /// their tiles show. Returns the entity id if the message changed a state.
    pub fn apply(&mut self, pages: &[Page], topic: &str, payload: &[u8]) -> Option<String> {
        let (entity_id, key) = parse_topic(topic)?;
        let kind = pages
            .iter()
            .flat_map(|page| page.tiles)
            .find(|tile| tile.entity_id == entity_id)?
            .kind;
        let is_state = key == "state";
        if !is_state && !kind.attributes().contains(&key) {
            return None;
        }
        let text = core::str::from_utf8(payload).ok()?.trim();

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
/// For each entity, there is one topic for the state and one topic for each
/// attribute that its tile shows. The core does not subscribe to other
/// attributes, so a wake on battery gets only the retained messages that the
/// dashboard uses.
#[must_use]
pub fn topics(pages: &[Page]) -> Vec<String> {
    let mut topics: Vec<String> = pages
        .iter()
        .flat_map(|page| page.tiles)
        .filter_map(|tile| {
            let (domain, object_id) = tile.entity_id.split_once('.')?;
            let base = format!("{STATESTREAM}/{domain}/{object_id}");
            let names = core::iter::once("state").chain(tile.kind.attributes().iter().copied());
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
        let description = describe(kind, entities.get(entity_id), 0);
        (description.state, description.text)
    }

    fn detail(entities: &Entities, kind: TileKind, entity_id: &str) -> String {
        describe(kind, entities.get(entity_id), 0).detail
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
            (TileState::Unknown, "-".into())
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
                format!("{base}/climate/thermostat/state"),
                format!("{base}/climate/thermostat/temperature"),
                format!("{base}/light/lamp/brightness"),
                format!("{base}/light/lamp/state"),
                format!("{base}/sensor/temperature/state"),
                format!("{base}/sensor/temperature/unit_of_measurement"),
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
    fn a_timestamp_sensor_shows_the_local_time() {
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/sensor/temperature/state";
        entities.apply(PAGES, topic, b"2026-07-01T14:30:00+00:00");
        assert_eq!(
            describe(TileKind::Sensor, entities.get("sensor.temperature"), 60).text,
            "15:30"
        );
    }
}
