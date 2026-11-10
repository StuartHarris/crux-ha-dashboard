//! The summary of a room: what the button of the room on the index page
//! shows.

use alloc::{format, string::String, vec::Vec};

use crate::{
    entities::{Entities, EntityState},
    layout::{Page, Tile, TileKind},
    view::{LightState, RoomSummary},
};

/// Returns the summary of the room `page`, from the states in `entities`.
///
/// The summary has the title of the room, the state of each light and
/// switch, the temperature, and `any_on`. Refer to [`lights`] and
/// [`temperature`].
#[must_use]
pub fn summarise(page: &Page, entities: &Entities) -> RoomSummary {
    let lights = lights(page, entities);
    RoomSummary {
        title: page.title.into(),
        any_on: lights.contains(&LightState::On),
        lights,
        temperature: temperature(page, entities),
    }
}

/// Returns the state of each light and switch of the room `page`, in the
/// order of its tiles.
///
/// If the room has a light or a switch that is not a group, the list does
/// not have the groups. Refer to [`Tile::group`](crate::layout::Tile::group).
/// A tile with a state that is not `on` or `off` is
/// [`LightState::Unavailable`]. Examples are `unavailable`, `unknown` and no
/// state yet.
#[must_use]
pub fn lights(page: &Page, entities: &Entities) -> Vec<LightState> {
    counted(page)
        .map(|tile| {
            let state = entities
                .get(tile.entity_id)
                .and_then(|entity| entity.state.as_deref());
            match state {
                Some("on") => LightState::On,
                Some("off") => LightState::Off,
                _ => LightState::Unavailable,
            }
        })
        .collect()
}

/// Returns the light and switch tiles of the room `page` that its summary
/// shows: each one, without the groups if the room has a light or a switch
/// that is not a group.
pub fn counted(page: &Page) -> impl Iterator<Item = &'static Tile> + use<> {
    let tiles: &'static [Tile] = page.tiles;
    let toggles = move || {
        tiles
            .iter()
            .filter(|tile| matches!(tile.kind, TileKind::Light | TileKind::Switch))
    };
    let has_members = toggles().any(|tile| !tile.group);
    toggles().filter(move |tile| !(has_members && tile.group))
}

/// Returns `true` if the index page shows the entity `entity_id`, with the
/// states in `entities` now.
///
/// The index page shows these entities:
///
/// - The All lights tile and the weather tile of the home page.
/// - The lights and switches that the summary of a room shows. Refer to
///   [`counted`].
/// - Each climate tile of a room, and each sensor tile of a room with a unit
///   that starts with `°`. These can give the temperature of the room.
///
/// The core uses this check before it makes the view, so a message for
/// another entity, for example a power sensor, does not make the view.
#[must_use]
pub fn index_shows(pages: &[Page], entities: &Entities, entity_id: &str) -> bool {
    let is = |tile: &&Tile| tile.entity_id == entity_id;
    pages.iter().any(|page| {
        if page.is_home() {
            return [page.all_lights_tile(), page.weather_tile()]
                .into_iter()
                .flatten()
                .any(|tile| is(&tile));
        }
        counted(page).any(|tile| is(&tile))
            || page.tiles.iter().filter(is).any(|tile| match tile.kind {
                TileKind::Climate => true,
                TileKind::Sensor => entities.get(tile.entity_id).is_some_and(has_degrees),
                _ => false,
            })
    })
}

/// Returns `true` if the unit of the sensor starts with `°`.
fn has_degrees(entity: &EntityState) -> bool {
    entity
        .attribute("unit_of_measurement")
        .is_some_and(|unit| unit.starts_with('°'))
}

/// Returns the temperature of the room `page` as text, for example `20.4°`,
/// or `None` if no tile of the room gives a temperature.
///
/// The temperature comes from the first tile that gives one. A climate tile
/// gives its current temperature. A sensor tile gives its state if its unit
/// starts with `°`. A tile that has no finite number gives no temperature.
/// The text has one decimal place, for example `21.0°` for the state `21`.
#[must_use]
pub fn temperature(page: &Page, entities: &Entities) -> Option<String> {
    page.tiles.iter().find_map(|tile| {
        let entity = entities.get(tile.entity_id)?;
        let value = match tile.kind {
            TileKind::Climate => entity.attribute("current_temperature")?,
            TileKind::Sensor if has_degrees(entity) => entity.state.as_deref()?,
            _ => return None,
        };
        let value = value
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())?;
        Some(format!("{value:.1}°"))
    })
}

#[cfg(test)]
mod tests {
    use alloc::{format, vec, vec::Vec};

    use super::{index_shows, summarise};
    use crate::{
        entities::Entities,
        layout::{Page, climate, home, light, light_group, room, sensor, switch, weather},
        view::{
            LightState::{Off, On, Unavailable},
            RoomSummary,
        },
    };

    const LIVING_ROOM: Page = room(
        "living_room",
        "Living room",
        &[
            light_group("Lights", "light.living_room_lights"),
            light("Rocket lamp", "light.rocket_lamp"),
            light("Standard lamp", "light.standard_lamp"),
            light("Reading lamp", "light.reading_lamp"),
            sensor("Humidity", "sensor.humidity"),
            sensor("Temperature", "sensor.temperature"),
        ],
        &[],
    );

    const DINING_ROOM: Page = room(
        "dining_room",
        "Dining room",
        &[
            light("Tall lamp", "light.tall_lamp"),
            climate("Thermostat", "climate.thermostat"),
            sensor("Temperature", "sensor.dining_temperature"),
        ],
        &[],
    );

    const KITCHEN: Page = room(
        "kitchen",
        "Kitchen",
        &[
            switch("Coffee machine", "switch.coffee"),
            sensor("Coffee power", "sensor.coffee_power"),
        ],
        &[],
    );

    const GROUP_ONLY: Page = room(
        "loft",
        "Loft",
        &[light_group("Lights", "light.loft_lights")],
        &[],
    );

    const SENSORS_ONLY: Page = room(
        "cellar",
        "Cellar",
        &[sensor("Temperature", "sensor.cellar_temperature")],
        &[],
    );

    const PAGES: &[Page] = &[LIVING_ROOM, DINING_ROOM, KITCHEN, GROUP_ONLY, SENSORS_ONLY];

    /// Applies the state of each entity, and the attributes after `;`, for
    /// example `("sensor.temperature", "20.4;unit_of_measurement=\"°C\"")`.
    fn entities(states: &[(&str, &str)]) -> Entities {
        let mut entities = Entities::default();
        for (entity_id, value) in states {
            let (domain, object) = entity_id.split_once('.').unwrap();
            let base = format!("homeassistant/statestream/{domain}/{object}");
            let mut parts = value.split(';');
            let state = parts.next().unwrap();
            if !state.is_empty() {
                entities.apply(PAGES, &format!("{base}/state"), state.as_bytes());
            }
            for attribute in parts {
                let (name, json) = attribute.split_once('=').unwrap();
                entities.apply(PAGES, &format!("{base}/{name}"), json.as_bytes());
            }
        }
        entities
    }

    fn summary(page: &Page, states: &[(&str, &str)]) -> RoomSummary {
        summarise(page, &entities(states))
    }

    /// Returns the states of the three lamps of the Living Room. The group
    /// of the lamps is on.
    fn lamps(states: [&'static str; 3]) -> Vec<(&'static str, &'static str)> {
        vec![
            ("light.living_room_lights", "on"),
            ("light.rocket_lamp", states[0]),
            ("light.standard_lamp", states[1]),
            ("light.reading_lamp", states[2]),
        ]
    }

    #[test]
    fn all_on() {
        let room = summary(&LIVING_ROOM, &lamps(["on", "on", "on"]));
        assert_eq!(room.title, "Living room");
        assert_eq!(room.lights, [On, On, On]);
        assert!(room.any_on);
    }

    #[test]
    fn all_off() {
        let room = summary(&LIVING_ROOM, &lamps(["off", "off", "off"]));
        assert_eq!(room.lights, [Off, Off, Off]);
        assert!(!room.any_on);
    }

    #[test]
    fn mixed_in_the_order_of_the_tiles() {
        let room = summary(&LIVING_ROOM, &lamps(["off", "on", "off"]));
        assert_eq!(room.lights, [Off, On, Off]);
        assert!(room.any_on);
    }

    #[test]
    fn a_light_that_is_unavailable_or_has_no_state() {
        let room = summary(&LIVING_ROOM, &lamps(["unavailable", "unknown", "off"]));
        assert_eq!(room.lights, [Unavailable, Unavailable, Off]);
        assert!(!room.any_on);
        assert_eq!(summary(&LIVING_ROOM, &[]).lights, [Unavailable; 3]);
    }

    #[test]
    fn a_group_is_left_out_when_the_room_has_its_members() {
        // The group is on, but each member is off.
        let room = summary(&LIVING_ROOM, &lamps(["off", "off", "off"]));
        assert_eq!(room.lights.len(), 3);
        assert!(!room.any_on);
    }

    #[test]
    fn a_group_is_kept_when_the_room_has_only_groups() {
        let room = summary(&GROUP_ONLY, &[("light.loft_lights", "on")]);
        assert_eq!(room.lights, [On]);
        assert!(room.any_on);
    }

    #[test]
    fn a_switch_is_a_light() {
        let states = [
            ("switch.coffee", "on"),
            ("sensor.coffee_power", "1.2;unit_of_measurement=\"W\""),
        ];
        let room = summary(&KITCHEN, &states);
        assert_eq!(room.lights, [On]);
        // A power sensor does not give a temperature.
        assert_eq!(room.temperature, None);
    }

    #[test]
    fn a_temperature_from_a_sensor() {
        let mut states = lamps(["off", "off", "off"]);
        states.push(("sensor.humidity", "48;unit_of_measurement=\"%\""));
        states.push(("sensor.temperature", "20.4;unit_of_measurement=\"°C\""));
        let room = summary(&LIVING_ROOM, &states);
        assert_eq!(room.temperature.as_deref(), Some("20.4°"));
    }

    #[test]
    fn the_first_tile_with_a_temperature_gives_the_temperature() {
        let states = [
            ("climate.thermostat", "heat;current_temperature=19.5"),
            ("sensor.dining_temperature", "21;unit_of_measurement=\"°C\""),
        ];
        let room = summary(&DINING_ROOM, &states);
        assert_eq!(room.temperature.as_deref(), Some("19.5°"));

        // A thermostat with no current temperature gives none, so the
        // sensor after it gives the temperature.
        let states = [
            ("climate.thermostat", "heat"),
            ("sensor.dining_temperature", "21;unit_of_measurement=\"°C\""),
        ];
        let room = summary(&DINING_ROOM, &states);
        assert_eq!(room.temperature.as_deref(), Some("21.0°"));
    }

    #[test]
    fn a_temperature_that_is_not_a_number_is_absent() {
        let states = [(
            "sensor.temperature",
            "unavailable;unit_of_measurement=\"°C\"",
        )];
        assert_eq!(summary(&LIVING_ROOM, &states).temperature, None);
        assert_eq!(summary(&LIVING_ROOM, &[]).temperature, None);
    }

    #[test]
    fn a_room_with_no_lights() {
        let states = [("sensor.cellar_temperature", "16;unit_of_measurement=\"°C\"")];
        let room = summary(&SENSORS_ONLY, &states);
        assert_eq!(room.lights, []);
        assert!(!room.any_on);
        assert_eq!(room.temperature.as_deref(), Some("16.0°"));
    }

    #[test]
    fn a_temperature_that_is_not_finite_is_absent() {
        for state in ["nan", "NaN", "inf", "-inf", "1e999"] {
            let states = [(
                "sensor.temperature",
                alloc::format!("{state};unit_of_measurement=\"°C\""),
            )];
            let states: Vec<(&str, &str)> =
                states.iter().map(|(id, v)| (*id, v.as_str())).collect();
            assert_eq!(summary(&LIVING_ROOM, &states).temperature, None, "{state}");
        }
        let states = [("sensor.temperature", "1e3;unit_of_measurement=\"°C\"")];
        assert_eq!(
            summary(&LIVING_ROOM, &states).temperature.as_deref(),
            Some("1000.0°")
        );
    }

    #[test]
    fn the_entities_that_the_index_shows() {
        const INDEX: &[Page] = &[
            home(
                "Home",
                &[
                    weather("Weather", "weather.home"),
                    light_group("All lights", "light.all_lights"),
                    light("Second", "light.second"),
                ],
                &[],
            ),
            LIVING_ROOM,
            DINING_ROOM,
            KITCHEN,
        ];
        let mut entities = Entities::default();
        let topic = "homeassistant/statestream/sensor/temperature/unit_of_measurement";
        entities.apply(INDEX, topic, "\"°C\"".as_bytes());
        let topic = "homeassistant/statestream/sensor/humidity/unit_of_measurement";
        entities.apply(INDEX, topic, b"\"%\"");
        let shows = |entity_id| index_shows(INDEX, &entities, entity_id);

        // The header shows the weather and the first light of the home page.
        assert!(shows("weather.home"));
        assert!(shows("light.all_lights"));
        assert!(!shows("light.second"));
        // The summaries show the lights but not a group with members.
        assert!(shows("light.rocket_lamp"));
        assert!(shows("switch.coffee"));
        assert!(!shows("light.living_room_lights"));
        // A thermostat and a sensor in degrees can give the temperature.
        assert!(shows("climate.thermostat"));
        assert!(shows("sensor.temperature"));
        assert!(!shows("sensor.humidity"));
        assert!(!shows("sensor.coffee_power"));
        assert!(!shows("light.other"));
    }
}
