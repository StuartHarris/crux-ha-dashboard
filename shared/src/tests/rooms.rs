//! Tests of the rooms feature: pages, routes and taps.

use crux_core::App;

use super::{
    connection::{drive_to_connected, drive_to_connecting},
    start_at,
};
use crate::{
    Dashboard, EffectTestExt, Event, Model, PowerSource, Route, capabilities::mqtt::Qos,
    features::connection::Link,
};

#[test]
fn next_page_and_previous_page_wrap_around() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    let count = model.pages.len();

    app.update(Event::PreviousPage, &mut model)
        .expect_only_render();
    let view = app.view(&model);
    assert_eq!(view.title, "Outside");
    assert_eq!(view.page_index, count - 1);
    assert_eq!(view.path, "/room/outside");
    assert_eq!(view.full_refresh_seq, 2);

    app.update(Event::NextPage, &mut model).expect_only_render();
    app.update(Event::NextPage, &mut model).expect_only_render();
    let view = app.view(&model);
    assert_eq!(view.title, "Living Room");
    assert_eq!(view.path, "/room/living_room");
    assert_eq!(view.page_index, 1);
    assert_eq!(view.page_count, count);
    assert_eq!(view.scenes.len(), 3);
    assert_eq!(view.scenes[0].label, "Bright");
}

#[test]
fn navigate_shows_the_page_of_a_route() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(Event::Navigate(Route::room("upstairs")), &mut model)
        .expect_only_render();
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

    app.update(Event::TileTapped(1), &mut model)
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
fn a_tap_on_a_switch_publishes_a_toggle() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("kitchen");

    app.update(Event::TileTapped(0), &mut model)
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

    app.update(Event::SceneTapped(2), &mut model)
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

    // The weather tile on the home page.
    app.update(Event::TileTapped(1), &mut model)
        .expect_no_effects();
    // The home page has no scenes.
    app.update(Event::SceneTapped(0), &mut model)
        .expect_no_effects();

    model.route = Route::room("dining_room");
    // The thermostat and the humidity sensor.
    app.update(Event::TileTapped(4), &mut model)
        .expect_no_effects();
    app.update(Event::TileTapped(5), &mut model)
        .expect_no_effects();
    app.update(Event::TileTapped(6), &mut model)
        .expect_no_effects();
}

#[test]
fn a_tap_before_the_connection_opens_waits_in_the_outbox() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connecting(PowerSource::Stand);

    // The connection is starting, so there is nothing more to ask for.
    app.update(Event::TileTapped(0), &mut model)
        .expect_no_effects();
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
