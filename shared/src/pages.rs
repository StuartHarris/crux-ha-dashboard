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

use crate::layout::{
    Page, climate, home, light, light_group, room, scene, sensor, switch, weather,
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
        "Living Room",
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
        "Dining Room",
        &[
            light_group("Lights", "light.dining_room_lights"),
            light("Tall lamp", "light.dining_room_tall_lamp"),
            light("Candle left", "light.dining_room_candle_light_left"),
            light("Candle right", "light.dining_room_candle_light_right"),
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
            light("Salt lamp", "light.hall_salt_lamp"),
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
            light("Sean's spotlight", "light.main_bedroom_sean_spotlight"),
            light("Stu's spotlight", "light.main_bedroom_stu_spotlight"),
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

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use super::PAGES;
    use crate::{
        layout::{MAX_LIGHTS, MAX_ROOMS, MAX_SCENES, MAX_TILES, TileKind},
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
    fn a_tile_entity_occurs_once_on_a_page() {
        for page in PAGES {
            let ids: BTreeSet<_> = page.tiles.iter().map(|tile| tile.entity_id).collect();
            assert_eq!(ids.len(), page.tiles.len(), "{}", page.title);
        }
    }
}
