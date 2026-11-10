//! Tests of the rooms feature: pages, routes and taps.

use alloc::vec::Vec;

use crux_core::App;

use super::{
    ExpectFullRefreshTimer,
    connection::{drive_to_connected, drive_to_connecting},
    set_state, start_at, tap_all_lights, tap_brighter, tap_dimmer, tap_room, tap_scene, tap_tile,
};
use crate::{
    Control, Dashboard, EffectTestExt, Event, Model, PowerSource, Route, capabilities::mqtt::Qos,
    features::connection::Link, view::LightState,
};

#[test]
fn next_page_and_previous_page_wrap_around() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    let count = model.pages.len();

    app.update(Event::PreviousPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    let view = app.view(&model);
    assert_eq!(view.title, "Outside");
    assert_eq!(view.page_index, count - 1);
    assert_eq!(view.path, "/room/outside");
    assert_eq!(view.full_refresh_seq, 2);

    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    let view = app.view(&model);
    assert_eq!(view.title, "Living room");
    assert_eq!(view.path, "/room/living_room");
    assert_eq!(view.page_index, 1);
    assert_eq!(view.page_count, count);
    assert_eq!(view.scenes().len(), 3);
    assert_eq!(view.scenes()[0].label, "Bright");
}

#[test]
fn navigate_shows_the_page_of_a_route() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(Event::Navigate(Route::room("upstairs")), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).title, "Landing");

    // The same route again does not draw.
    app.update(Event::Navigate(Route::room("upstairs")), &mut model)
        .expect_no_effects();

    // A route without a page does nothing.
    app.update(Event::Navigate(Route::room("office")), &mut model)
        .expect_no_effects();
    assert_eq!(model.route, Route::room("upstairs"));
}

#[test]
fn start_shows_the_page_of_the_path_that_the_shell_kept() {
    let app = Dashboard;

    let mut model = Model::default();
    let _ = app.update(
        start_at(PowerSource::Battery, Some("/room/kitchen")),
        &mut model,
    );
    assert_eq!(app.view(&model).title, "Kitchen");

    for path in ["/room/office", "not a path"] {
        let mut model = Model::default();
        let _ = app.update(start_at(PowerSource::Battery, Some(path)), &mut model);
        assert_eq!(app.view(&model).path, "/", "{path}");
    }
}

#[test]
fn a_tap_on_a_light_publishes_a_toggle() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    app.update(tap_tile(1), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/cmd");
            assert_eq!(
                publish.payload,
                br#"{"service":"light.toggle","entity_id":"light.living_room_rocket_lamp"}"#
            );
            assert_eq!(publish.qos, Qos::AtLeastOnce);
            assert!(!publish.retain);
        });
}

#[test]
fn the_plus_and_minus_buttons_of_a_dimmable_light_publish_a_brightness_step() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, "light.living_room_lights", "on");
    set_state(&mut model, "light.living_room_rocket_lamp", "on");

    app.update(tap_brighter(1), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/cmd");
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":25}"#
            );
            assert_eq!(publish.qos, Qos::AtLeastOnce);
            assert!(!publish.retain);
        });
    app.update(tap_dimmer(1), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":-25}"#
            );
        });
    // A light group is dimmable too.
    app.update(tap_dimmer(0), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_lights","brightness_step_pct":-25}"#
            );
        });
}

#[test]
fn a_brightness_step_on_a_tile_that_is_not_a_dimmable_light_does_nothing() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    // Each entity is on, so only the kind of the tile can stop the step.
    for entity_id in [
        "light.all_lights",
        "light.dining_room_candle_light_left",
        "light.dining_room_candle_light_right",
        "climate.dining_room_thermostat",
        "sensor.dining_room_thermostat_humidity",
        "light.hall_salt_lamp",
        "sensor.hall_salt_lamp_power",
        "switch.kitchen_coffee_machine_socket",
    ] {
        set_state(&mut model, entity_id, "on");
    }

    // The index page has no tiles.
    app.update(tap_brighter(0), &mut model).expect_no_effects();

    // The candle lights are on/off lights. The thermostat and the humidity
    // sensor are not lights, and the page has no tile 6.
    model.route = Route::room("dining_room");
    for index in [2, 3, 4, 5, 6] {
        app.update(tap_brighter(index), &mut model)
            .expect_no_effects();
        app.update(tap_dimmer(index), &mut model)
            .expect_no_effects();
    }

    // The salt lamp is an on/off light. The power sensor is not a light.
    model.route = Route::room("hall");
    for index in [2, 3] {
        app.update(tap_brighter(index), &mut model)
            .expect_no_effects();
    }

    // The coffee machine is a switch.
    model.route = Route::room("kitchen");
    app.update(tap_brighter(0), &mut model).expect_no_effects();
    app.update(tap_dimmer(0), &mut model).expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn a_minus_step_on_a_light_that_is_off_does_nothing_and_a_plus_step_turns_it_on() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, "light.living_room_rocket_lamp", "off");

    app.update(tap_dimmer(1), &mut model).expect_no_effects();
    app.update(tap_brighter(1), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":25}"#
            );
        });
}

#[test]
fn a_brightness_step_on_a_light_with_no_usable_state_does_nothing() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    // The core has no state for the rocket lamp yet.
    app.update(tap_brighter(1), &mut model).expect_no_effects();
    app.update(tap_dimmer(1), &mut model).expect_no_effects();

    for state in ["unavailable", "unknown"] {
        set_state(&mut model, "light.living_room_rocket_lamp", state);
        app.update(tap_brighter(1), &mut model).expect_no_effects();
        app.update(tap_dimmer(1), &mut model).expect_no_effects();
    }
    assert_eq!(model.connection.outbox.len(), 0);

    // A tap on the tile still sends the toggle, as for an on/off light.
    app.update(tap_tile(1), &mut model)
        .expect_only_mqtt_publish();
}

#[test]
fn a_toggle_has_no_brightness_step() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    app.update(tap_tile(1), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            let payload = core::str::from_utf8(&publish.payload).unwrap();
            assert!(!payload.contains("brightness_step_pct"), "{payload}");
        });
}

#[test]
fn a_tap_on_a_switch_publishes_a_toggle() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("kitchen");

    app.update(tap_tile(0), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"switch.toggle","entity_id":"switch.kitchen_coffee_machine_socket"}"#
            );
        });
}

#[test]
fn a_tap_on_a_scene_publishes_scene_turn_on() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("hall");

    app.update(tap_scene(2), &mut model)
        .expect_only_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/cmd");
            assert_eq!(
                publish.payload,
                br#"{"service":"scene.turn_on","entity_id":"scene.hall_hall_nightlight"}"#
            );
        });
}

#[test]
fn a_tap_on_a_sensor_climate_or_weather_tile_or_outside_the_grid_does_nothing() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    // The index page has no tiles and no scenes.
    app.update(tap_tile(0), &mut model).expect_no_effects();
    app.update(tap_tile(1), &mut model).expect_no_effects();
    app.update(tap_scene(0), &mut model).expect_no_effects();

    model.route = Route::room("dining_room");
    // The thermostat and the humidity sensor.
    app.update(tap_tile(4), &mut model).expect_no_effects();
    app.update(tap_tile(5), &mut model).expect_no_effects();
    app.update(tap_tile(6), &mut model).expect_no_effects();
}

#[test]
fn a_tap_before_the_connection_opens_waits_in_the_outbox() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connecting(PowerSource::Stand);

    // The connection is starting, so there is nothing more to ask for.
    app.update(tap_all_lights(), &mut model).expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 1);

    request
        .resolve(crate::capabilities::mqtt::MqttEvent::Connected)
        .unwrap();
    let event = cmd.expect_one_event();
    app.update(event, &mut model)
        .expect_mqtt_subscribe()
        .expect_mqtt_publish_with(|publish| assert_eq!(publish.payload, b"online"))
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.toggle","entity_id":"light.all_lights"}"#
            );
        })
        .expect_only_render();
    assert_eq!(model.connection.outbox.len(), 0);
    assert_eq!(model.connection.mqtt, Link::Up);
}

/// Drives the core to an open connection on the stand, in a dry run.
fn connected_in_a_dry_run() -> Model {
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    if let Some(settings) = &mut model.settings {
        settings.read_only = true;
    }
    model
}

#[test]
fn a_tap_in_a_dry_run_sends_no_service_call() {
    let app = Dashboard;
    let mut model = connected_in_a_dry_run();
    model.route = Route::room("hall");
    // The hall light is on, so only the dry run stops its steps.
    set_state(&mut model, "light.hall_hall_light", "on");
    assert!(app.view(&model).read_only);

    app.update(tap_tile(0), &mut model).expect_no_effects();
    app.update(tap_scene(2), &mut model).expect_no_effects();
    app.update(tap_brighter(1), &mut model).expect_no_effects();
    app.update(tap_dimmer(1), &mut model).expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn a_tap_in_a_dry_run_before_the_connection_opens_is_not_kept() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connecting(PowerSource::Stand);
    let _ = app.update(Event::SetReadOnly(true), &mut model);

    app.update(tap_all_lights(), &mut model).expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn set_read_only_changes_the_view_and_the_next_tap() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    assert!(!app.view(&model).read_only);

    app.update(Event::SetReadOnly(true), &mut model)
        .expect_only_render();
    assert!(app.view(&model).read_only);
    // The same value again changes nothing.
    app.update(Event::SetReadOnly(true), &mut model)
        .expect_no_effects();
    app.update(tap_all_lights(), &mut model).expect_no_effects();

    app.update(Event::SetReadOnly(false), &mut model)
        .expect_only_render();
    assert!(!app.view(&model).read_only);
    app.update(tap_all_lights(), &mut model)
        .expect_only_mqtt_publish();
}

#[test]
fn a_dry_run_before_the_start_is_kept() {
    let app = Dashboard;
    let mut model = Model::default();
    app.update(Event::SetReadOnly(true), &mut model)
        .expect_no_effects();
    assert!(app.view(&model).read_only);

    // The settings do not ask for a dry run, but the request wins.
    let _ = app.update(start_at(PowerSource::Stand, None), &mut model);
    assert!(app.view(&model).read_only);
    assert!(model.settings.as_ref().unwrap().read_only);
}

#[test]
fn a_request_to_stop_the_dry_run_before_the_start_does_not_stop_it() {
    let app = Dashboard;
    let mut model = Model::default();
    app.update(Event::SetReadOnly(false), &mut model)
        .expect_no_effects();

    let Event::Start(mut start) = start_at(PowerSource::Stand, None) else {
        unreachable!()
    };
    start.settings.read_only = true;
    let _ = app.update(Event::Start(start), &mut model);
    assert!(app.view(&model).read_only);
}

#[test]
fn set_read_only_while_asleep_does_not_draw() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    assert!(model.power.asleep);

    app.update(Event::SetReadOnly(true), &mut model)
        .expect_no_effects();
    assert!(app.view(&model).read_only);
    assert!(app.view(&model).status.starts_with("Dry run · Asleep"));
}

#[test]
fn the_status_shows_the_dry_run() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    assert_eq!(app.view(&model).status, "Online, on stand");
    let _ = app.update(Event::SetReadOnly(true), &mut model);
    assert_eq!(app.view(&model).status, "Dry run · Online, on stand");
}

#[test]
fn a_tap_on_a_navigation_button_turns_the_page() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    let count = model.pages.len();

    app.update(Event::Tapped(Some(Control::NextPage)), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).page_index, 1);
    app.update(Event::Tapped(Some(Control::PreviousPage)), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    app.update(Event::Tapped(Some(Control::PreviousPage)), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).page_index, count - 1);
    app.update(Event::Tapped(Some(Control::Home)), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).path, "/");
}

#[test]
fn a_tap_on_no_control_changes_nothing_while_awake() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    app.update(Event::Tapped(None), &mut model)
        .expect_no_effects();
}

#[test]
fn a_tap_on_a_room_button_shows_the_room() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    // The rooms are the pages after the home page, in order.
    app.update(tap_room(7), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    let view = app.view(&model);
    assert_eq!(view.path, "/room/loft");
    assert_eq!(view.title, "Loft");
    assert_eq!(view.full_refresh_seq, 2);
    assert_eq!(view.index(), None);
    assert_eq!(view.tiles().len(), 4);
}

#[test]
fn a_room_button_that_the_index_does_not_have_does_nothing() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    app.update(tap_room(9), &mut model).expect_no_effects();
    assert_eq!(model.route, Route::Home);
}

#[test]
fn a_tap_on_all_lights_publishes_a_toggle() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    app.update(tap_all_lights(), &mut model)
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/cmd");
            assert_eq!(
                publish.payload,
                br#"{"service":"light.toggle","entity_id":"light.all_lights"}"#
            );
        })
        .expect_no_effects();
}

#[test]
fn the_index_shows_each_room_and_the_header_of_the_home_page() {
    let app = Dashboard;
    let model = Model::default();
    let view = app.view(&model);
    let index = view.index().unwrap();
    let titles: Vec<&str> = index.rooms.iter().map(|room| room.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Living room",
            "Dining room",
            "Kitchen",
            "Hall",
            "Landing",
            "Main bedroom",
            "Guest bedroom",
            "Loft",
            "Outside",
        ]
    );
    assert_eq!(
        index.all_lights.as_ref().unwrap().entity_id,
        "light.all_lights"
    );
    assert_eq!(index.weather.as_ref().unwrap().entity_id, "weather.home");
    // With no state, each light is unavailable and no room is on.
    assert!(index.rooms.iter().all(|room| {
        !room.any_on
            && room
                .lights
                .iter()
                .all(|light| *light == LightState::Unavailable)
    }));
    assert_eq!(view.tiles(), []);
    assert_eq!(view.scenes(), []);
}

#[test]
fn the_all_lights_toggle_shows_and_toggles_the_same_tile() {
    use crate::layout::{Page, home, light, room, switch, weather};
    const PAGES: &[Page] = &[
        home(
            "Home",
            &[
                weather("Weather", "weather.home"),
                switch("Fan", "switch.fan"),
                light("Lamp", "light.lamp"),
            ],
            &[],
        ),
        room("hall", "Hall", &[light("Hall", "light.hall")], &[]),
    ];
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.pages = PAGES;
    let view = app.view(&model);
    let shown = &view.index().unwrap().all_lights.as_ref().unwrap().entity_id;
    assert_eq!(shown, "switch.fan");
    app.update(tap_all_lights(), &mut model)
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"switch.toggle","entity_id":"switch.fan"}"#
            );
        })
        .expect_no_effects();
}
