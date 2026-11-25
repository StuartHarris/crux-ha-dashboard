//! The dashboard layout: the pages in order, with their tiles and scenes.
//!
//! The first page is the home page, which is the index of the rooms. Its
//! header shows the weather and an All lights toggle from its tiles. Refer
//! to [`Page`]. Each other page is a room.
//!
//! Edit this file to change the dashboard. Each page has at most
//! [`MAX_TILES`](crate::layout::MAX_TILES) tiles and
//! [`MAX_SCENES`](crate::layout::MAX_SCENES) scenes. There are at most
//! [`MAX_ROOMS`](crate::layout::MAX_ROOMS) rooms, and the summary of a room
//! shows at most [`MAX_LIGHTS`](crate::layout::MAX_LIGHTS) lights and
//! switches. A test checks these limits
//! and the entity ids.
//!
//! Use [`light`] for a light that can change its brightness, and
//! [`onoff_light`] for a light that can only be on or off. Home Assistant
//! gives the color modes of a light in its `supported_color_modes` attribute.
//! A light with only `onoff` is an on/off light.

use alloc::vec::Vec;

use crate::layout::{
    Page, climate, home, light, light_group, onoff_light, room, scene, sensor, switch, weather,
};

/// The pages of the dashboard, in the order that the pages show.
pub const PAGES: &[Page] = &[
    home(
        "Home",
        &[
            light_group("All lights", "light.all_lights"),
            weather("Weather", "weather.home"),
        ],
        &[],
    ),
    room(
        "living_room",
        "Living room",
        &[
            light_group("Lights", "light.living_room_lights"),
            light("Rocket lamp", "light.living_room_rocket_lamp"),
            light("Standard lamp", "light.living_room_standard_lamp"),
            sensor("Temperature", "sensor.living_room_thermometer_temperature"),
            sensor("Humidity", "sensor.living_room_thermometer_humidity"),
        ],
        &[
            scene("Bright", "scene.living_room_living_room_bright"),
            scene("Dimmed", "scene.living_room_living_room_dimmed"),
            scene("Nightlight", "scene.living_room_living_room_nightlight"),
        ],
    ),
    room(
        "dining_room",
        "Dining room",
        &[
            light_group("Lights", "light.dining_room_lights"),
            light("Tall lamp", "light.dining_room_tall_lamp"),
            onoff_light("Candle left", "light.dining_room_candle_light_left"),
            onoff_light("Candle right", "light.dining_room_candle_light_right"),
            climate("Thermostat", "climate.dining_room_thermostat"),
            sensor("Humidity", "sensor.dining_room_thermostat_humidity"),
        ],
        &[
            scene("Bright", "scene.dining_room_dining_room_bright"),
            scene("Dimmed", "scene.dining_room_dining_room_dimmed"),
            scene("Nightlight", "scene.dining_room_dining_room_nightlight"),
        ],
    ),
    room(
        "kitchen",
        "Kitchen",
        &[
            // A Home Assistant group of the seven ceiling lights. The page does
            // not show the members, so the tile is not a `light_group`. Thus
            // the summary of the room shows it next to the coffee machine.
            light("Lights", "light.kitchen_lights"),
            switch("Coffee machine", "switch.kitchen_coffee_machine_socket"),
            sensor("Coffee power", "sensor.kitchen_coffee_machine_power"),
            sensor("Dishwasher", "sensor.kitchen_dishwasher_operation_state"),
            sensor("Progress", "sensor.kitchen_dishwasher_programme_progress"),
            sensor(
                "Finishes",
                "sensor.kitchen_dishwasher_programme_finish_time",
            ),
        ],
        &[],
    ),
    room(
        "hall",
        "Hall",
        &[
            light_group("Lights", "light.hall_lights"),
            light("Hall light", "light.hall_hall_light"),
            onoff_light("Salt lamp", "light.hall_salt_lamp"),
            sensor("Salt lamp power", "sensor.hall_salt_lamp_power"),
        ],
        &[
            scene("Bright", "scene.hall_hall_bright"),
            scene("Dimmed", "scene.hall_hall_dimmed"),
            scene("Nightlight", "scene.hall_hall_nightlight"),
        ],
    ),
    room(
        "upstairs",
        "Landing",
        &[
            light_group("Lights", "light.landing_lights"),
            light("Corridor", "light.landing_corridor"),
            light("Landing light", "light.landing_landing_light"),
        ],
        &[
            scene("Bright", "scene.upstairs_upstairs_bright"),
            scene("Dimmed", "scene.upstairs_upstairs_dimmed"),
            scene("Nightlight", "scene.upstairs_upstairs_nightlight"),
        ],
    ),
    room(
        "bedroom",
        "Main bedroom",
        &[
            light_group("Lights", "light.main_bedroom_lights"),
            light("Left spotlight", "light.main_bedroom_left_spotlight"),
            light("Right spotlight", "light.main_bedroom_right_spotlight"),
            sensor("Temperature", "sensor.main_bedroom_thermometer_temperature"),
            sensor("Humidity", "sensor.main_bedroom_thermometer_humidity"),
        ],
        &[],
    ),
    room(
        "guest_bedroom",
        "Guest bedroom",
        &[light("Lamp", "light.guest_bedroom_lamp")],
        &[
            scene("Bright", "scene.guest_bedroom_guest_bedroom_bright"),
            scene("Dimmed", "scene.guest_bedroom_guest_bedroom_dimmed"),
            scene("Nightlight", "scene.guest_bedroom_guest_bedroom_nightlight"),
        ],
    ),
    room(
        "loft",
        "Loft",
        &[
            light_group("Lights", "light.loft_lights"),
            light("Loft 1", "light.loft_loft_1"),
            light("Loft 2", "light.loft_loft_2"),
            light("Loft 3", "light.loft_loft_3"),
        ],
        &[
            scene("Bright", "scene.loft_loft_bright"),
            scene("Dimmed", "scene.loft_loft_dimmed"),
            scene("Nightlight", "scene.loft_loft_nightlight"),
        ],
    ),
    room(
        "outside",
        "Outside",
        &[
            light("Porch", "light.porch_light"),
            light("Garden", "light.back_garden_garden_light"),
        ],
        &[],
    ),
];

/// Returns the entity ids of the tiles and scenes of all the pages.
///
/// The ids are in page order, with the tiles of a page before its scenes. Each id
/// occurs once, at its first position. Use the list for the `include` list of
/// `mqtt_statestream` on the Pi.
#[must_use]
pub fn entity_ids(pages: &[Page]) -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = Vec::new();
    for page in pages {
        let tiles = page.tiles.iter().map(|tile| tile.entity_id);
        let scenes = page.scenes.iter().map(|scene| scene.entity_id);
        for id in tiles.chain(scenes) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Returns the entity attributes that the tiles of all the pages show.
///
/// The attributes are in the order in which they first occur, and each occurs once.
#[must_use]
pub fn attributes(pages: &[Page]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for tile in pages.iter().flat_map(|page| page.tiles) {
        for name in tile.kind.attributes() {
            if !names.contains(name) {
                names.push(name);
            }
        }
    }
    names
}

/// Returns the entity ids of the dashboard, from [`PAGES`].
#[must_use]
pub fn dashboard_entity_ids() -> Vec<&'static str> {
    entity_ids(PAGES)
}

/// Returns the entity attributes of the dashboard, from [`PAGES`].
#[must_use]
pub fn dashboard_attributes() -> Vec<&'static str> {
    attributes(PAGES)
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeSet, format, vec::Vec};

    use super::{PAGES, attributes, dashboard_attributes, dashboard_entity_ids, entity_ids};
    use crate::{
        entities::{STATESTREAM, topics},
        layout::{
            MAX_LIGHTS, MAX_ROOMS, MAX_SCENES, MAX_TILES, Page, TileKind, home, light_group, room,
            scene, sensor,
        },
        route::Route,
    };

    #[test]
    fn each_page_fits_the_grid() {
        for page in PAGES {
            assert_ne!(page.title, "");
            assert!(
                page.tiles.len() <= MAX_TILES,
                "{} has too many tiles",
                page.title
            );
            assert!(
                page.scenes.len() <= MAX_SCENES,
                "{} has too many scenes",
                page.title
            );
        }
    }

    #[test]
    fn each_entity_id_matches_its_kind() {
        for page in PAGES {
            for tile in page.tiles {
                let domain = match tile.kind {
                    TileKind::Light => "light",
                    TileKind::Switch => "switch",
                    TileKind::Sensor => "sensor",
                    TileKind::Climate => "climate",
                    TileKind::Weather => "weather",
                };
                let (actual, object_id) = tile.entity_id.split_once('.').unwrap();
                assert_eq!(actual, domain, "{}", tile.entity_id);
                assert!(!object_id.is_empty() && !object_id.contains(['.', '/', '#', '+']));
            }
            for scene in page.scenes {
                assert!(scene.entity_id.starts_with("scene."), "{}", scene.entity_id);
            }
        }
    }

    #[test]
    fn the_first_page_is_home_and_each_route_is_unique() {
        assert_eq!(PAGES[0].route(), Route::Home);
        let routes: BTreeSet<_> = PAGES.iter().map(|page| page.route().to_path()).collect();
        assert_eq!(routes.len(), PAGES.len());
        for page in PAGES {
            assert!(page.is_at(&page.route()));
            assert_eq!(
                Route::from_path(&page.route().to_path()),
                Some(page.route())
            );
        }
    }

    #[test]
    fn the_home_page_has_the_weather_and_all_lights_and_the_rooms_fit_the_index() {
        let home = &PAGES[0];
        let count = |kind| home.tiles.iter().filter(|tile| tile.kind == kind).count();
        assert_eq!(count(TileKind::Weather), 1);
        assert_eq!(count(TileKind::Light), 1);
        assert_eq!(home.tiles.len(), 2);
        assert_eq!(home.scenes, []);
        let rooms = PAGES.iter().filter(|page| page.is_room()).count();
        assert_eq!(rooms, PAGES.len() - 1);
        assert!(rooms <= MAX_ROOMS);
    }

    #[test]
    fn the_summary_of_each_room_fits_its_button() {
        for page in PAGES.iter().filter(|page| page.is_room()) {
            let lights = crate::summary::counted(page).count();
            assert!(lights <= MAX_LIGHTS, "{} has too many lights", page.title);
        }
    }

    #[test]
    fn only_a_light_is_a_group() {
        for tile in PAGES.iter().flat_map(|page| page.tiles) {
            assert!(
                !tile.group || tile.kind == TileKind::Light,
                "{}",
                tile.entity_id
            );
        }
    }

    #[test]
    fn only_a_light_is_dimmable() {
        for tile in PAGES.iter().flat_map(|page| page.tiles) {
            assert!(
                !tile.dimmable || tile.kind == TileKind::Light,
                "{}",
                tile.entity_id
            );
        }
    }

    #[test]
    fn each_light_is_dimmable_except_the_onoff_lights() {
        // Home Assistant gives only the `onoff` color mode for these lights.
        const ONOFF: [&str; 3] = [
            "light.dining_room_candle_light_left",
            "light.dining_room_candle_light_right",
            "light.hall_salt_lamp",
        ];
        let lights = PAGES
            .iter()
            .flat_map(|page| page.tiles)
            .filter(|tile| tile.kind == TileKind::Light);
        for tile in lights {
            let onoff = ONOFF.contains(&tile.entity_id);
            assert_eq!(tile.dimmable, !onoff, "{}", tile.entity_id);
        }
    }

    #[test]
    fn a_tile_entity_occurs_once_on_a_page() {
        for page in PAGES {
            let ids: BTreeSet<_> = page.tiles.iter().map(|tile| tile.entity_id).collect();
            assert_eq!(ids.len(), page.tiles.len(), "{}", page.title);
        }
    }

    #[test]
    fn the_entity_ids_have_no_repeats() {
        let ids = dashboard_entity_ids();
        let unique: BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn the_entity_ids_have_each_tile_and_scene() {
        let ids = dashboard_entity_ids();
        for page in PAGES {
            for tile in page.tiles {
                assert!(ids.contains(&tile.entity_id), "{}", tile.entity_id);
            }
            for scene in page.scenes {
                assert!(ids.contains(&scene.entity_id), "{}", scene.entity_id);
            }
        }
    }

    #[test]
    fn the_first_entity_id_is_all_lights() {
        assert_eq!(dashboard_entity_ids()[0], "light.all_lights");
    }

    #[test]
    fn the_attributes_are_the_five_that_the_tiles_show() {
        assert_eq!(
            dashboard_attributes(),
            [
                "brightness",
                "temperature",
                "temperature_unit",
                "unit_of_measurement",
                "current_temperature"
            ]
        );
    }

    #[test]
    fn each_topic_of_the_core_is_on_the_lists_for_the_pi() {
        let ids = dashboard_entity_ids();
        let names = dashboard_attributes();
        for topic in topics(PAGES) {
            let rest = topic.strip_prefix(STATESTREAM).unwrap();
            let [domain, object_id, name] = rest[1..].split('/').collect::<Vec<_>>()[..] else {
                panic!("{topic}");
            };
            let id = format!("{domain}.{object_id}");
            assert!(ids.contains(&id.as_str()), "{topic}");
            let fixed = name == "state" || name == crate::entities::LAST_UPDATED;
            assert!(fixed || names.contains(&name), "{topic}");
        }
    }

    #[test]
    fn an_entity_on_two_pages_is_listed_once_and_tiles_come_before_scenes() {
        const FIXTURE: &[Page] = &[
            home("Home", &[light_group("All", "light.all")], &[]),
            room(
                "a",
                "A",
                &[light_group("All", "light.all"), sensor("T", "sensor.t")],
                &[scene("S", "scene.s")],
            ),
        ];
        assert_eq!(entity_ids(FIXTURE), ["light.all", "sensor.t", "scene.s"]);
        assert_eq!(attributes(FIXTURE), ["brightness", "unit_of_measurement"]);
    }
}
