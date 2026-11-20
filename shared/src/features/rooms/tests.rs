//! Tests of the rooms feature: pages, routes and taps.

use crate::features::power;
use alloc::vec::Vec;
use core::time::Duration;

use crux_core::App;

use crate::tests::{
    ExpectFullRefreshTimer, ExpectLight, ExpectServiceCall,
    broker::publish_of,
    drive_to_connected, drive_to_connecting, set_attribute, set_state, start_at, tap_all_lights,
    tap_minus, tap_plus, tap_room, tap_scene, tap_tile,
    wire::{ExpectWrite, Wire},
};
use crate::{
    Control, Dashboard, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    features::{connection::Link, light::STAND_LEVEL, pending::QUEUED_TIMEOUT},
    mqtt::{Qos, codec::server::Packet},
    view::LightState,
};

#[test]
fn next_page_and_previous_page_wrap_around() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    app.update(tap_tile(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, "light.living_room_lights", "on");
    set_state(&mut model, "light.living_room_rocket_lamp", "on");

    app.update(tap_plus(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(publish.topic, "dashboard/hall/cmd");
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":25}"#
            );
            assert_eq!(publish.qos, Qos::AtLeastOnce);
            assert!(!publish.retain);
        });
    // The tile already has a mark, so the view does not change and the core
    // does not draw. The call still goes, with a new timer.
    app.update(tap_minus(1), &mut model)
        .expect_light_timer()
        .expect_publish_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":-25}"#
            );
        })
        .expect_pending_timer()
        .expect_session_clear(&wire)
        .expect_no_effects();
    // A light group is dimmable too.
    app.update(tap_minus(0), &mut model)
        .expect_light_timer()
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_lights","brightness_step_pct":-25}"#
            );
        });
}

#[test]
fn a_brightness_step_on_a_tile_that_is_not_a_dimmable_light_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

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
    app.update(tap_plus(0), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();

    // The candle lights are on/off lights. The thermostat has no target
    // temperature, the humidity sensor is not a light, and the page has no
    // tile 6.
    model.route = Route::room("dining_room");
    for index in [2, 3, 4, 5, 6] {
        app.update(tap_plus(index), &mut model)
            .expect_light_timer()
            .expect_no_effects();
        app.update(tap_minus(index), &mut model)
            .expect_light_timer()
            .expect_no_effects();
    }

    // The salt lamp is an on/off light. The power sensor is not a light.
    model.route = Route::room("hall");
    for index in [2, 3] {
        app.update(tap_plus(index), &mut model)
            .expect_light_timer()
            .expect_no_effects();
    }

    // The coffee machine is a switch.
    model.route = Route::room("kitchen");
    app.update(tap_plus(0), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    app.update(tap_minus(0), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn a_plus_step_on_a_light_at_full_brightness_does_nothing_and_a_minus_step_dims_it() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, "light.living_room_rocket_lamp", "on");
    let pages = model.pages;
    model.entities.apply(
        pages,
        "homeassistant/statestream/light/living_room_rocket_lamp/brightness",
        b"255",
    );
    assert_eq!(app.view(&model).tiles()[1].text, "On 100%");

    // Home Assistant does nothing for the step, so the core sends no call
    // and gives no mark.
    app.update(tap_plus(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    assert!(model.pending.marks.is_empty());
    assert_eq!(app.view(&model).tiles()[1].state, TileState::On);

    app.update(tap_minus(1), &mut model)
        .expect_light_timer()
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":-25}"#
            );
        });
}

#[test]
fn a_minus_step_on_a_light_that_is_off_does_nothing_and_a_plus_step_turns_it_on() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, "light.living_room_rocket_lamp", "off");

    app.update(tap_minus(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    app.update(tap_plus(1), &mut model)
        .expect_light_timer()
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.turn_on","entity_id":"light.living_room_rocket_lamp","brightness_step_pct":25}"#
            );
        });
}

#[test]
fn a_brightness_step_on_a_light_with_no_usable_state_does_nothing() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    // The core has no state for the rocket lamp yet.
    app.update(tap_plus(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    app.update(tap_minus(1), &mut model)
        .expect_light_timer()
        .expect_no_effects();

    for state in ["unavailable", "unknown"] {
        set_state(&mut model, "light.living_room_rocket_lamp", state);
        app.update(tap_plus(1), &mut model)
            .expect_light_timer()
            .expect_no_effects();
        app.update(tap_minus(1), &mut model)
            .expect_light_timer()
            .expect_no_effects();
    }
    assert_eq!(model.connection.outbox.len(), 0);

    // A tap on the tile still sends the toggle, as for an on/off light. The
    // tile is not on or off, so it does not show the mark.
    app.update(tap_tile(1), &mut model)
        .expect_light_timer()
        .expect_publish(&mut wire, &mut model)
        .expect_pending_timer();
    assert_eq!(app.view(&model).tiles()[1].state, TileState::Unknown);
}

#[test]
fn a_toggle_has_no_brightness_step() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    app.update(tap_tile(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            let payload = core::str::from_utf8(&publish.payload).unwrap();
            assert!(!payload.contains("brightness_step_pct"), "{payload}");
        });
}

/// The thermostat: tile 4 on the page of the dining room.
const THERMOSTAT: &str = "climate.dining_room_thermostat";

/// Drives the core to an open connection on the stand, on the page of the
/// dining room. The thermostat heats, and its target temperature is the JSON
/// value `target`.
fn thermostat_at(target: &str) -> (Model, Wire) {
    let (mut model, wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("dining_room");
    set_state(&mut model, THERMOSTAT, "heat");
    set_attribute(&mut model, THERMOSTAT, "current_temperature", "20.3");
    set_attribute(&mut model, THERMOSTAT, "temperature", target);
    (model, wire)
}

/// Returns the JSON payload of `climate.set_temperature` for the thermostat
/// with the target `temperature`, as text.
fn set_temperature(temperature: &str) -> alloc::string::String {
    alloc::format!(
        r#"{{"service":"climate.set_temperature","entity_id":"{THERMOSTAT}","temperature":{temperature}}}"#
    )
}

#[test]
fn the_plus_and_minus_buttons_of_a_thermostat_publish_the_next_half_degree() {
    let app = Dashboard;
    // The target, the button and the target that the call asks for. A
    // target on a half degree moves by a half degree. Another target moves
    // to the nearest half degree in the direction of the button.
    let cases = [
        ("19.5", true, "20.0"),
        ("19.5", false, "19.0"),
        ("19.3", true, "19.5"),
        ("19.3", false, "19.0"),
        ("19.7", true, "20.0"),
        ("19.7", false, "19.5"),
        ("21", true, "21.5"),
        ("21", false, "20.5"),
        ("20.0", true, "20.5"),
        ("20.0", false, "19.5"),
        // Home Assistant does the limits, so the core sends each step.
        ("32", true, "32.5"),
        ("10", false, "9.5"),
    ];
    for (target, up, expected) in cases {
        let (mut model, mut wire) = thermostat_at(target);
        assert!(app.view(&model).tiles()[4].steps, "{target}");
        let tap = if up { tap_plus(4) } else { tap_minus(4) };
        app.update(tap, &mut model)
            .expect_touch_light(STAND_LEVEL)
            .expect_only_service_call_with(&mut wire, &mut model, |publish| {
                assert_eq!(publish.topic, "dashboard/hall/cmd");
                assert_eq!(
                    core::str::from_utf8(&publish.payload).unwrap(),
                    set_temperature(expected),
                    "{target} {up}"
                );
                assert_eq!(publish.qos, Qos::AtLeastOnce);
                assert!(!publish.retain);
            });
    }
}

#[test]
fn a_thermostat_step_publishes_set_temperature_with_a_json_number() {
    let app = Dashboard;
    let (mut model, mut wire) = thermostat_at("19.5");

    app.update(tap_plus(4), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"climate.set_temperature","entity_id":"climate.dining_room_thermostat","temperature":20.0}"#
            );
        });
}

#[test]
fn a_second_step_of_a_thermostat_starts_from_the_target_that_waits() {
    let app = Dashboard;
    let (mut model, mut wire) = thermostat_at("20");

    app.update(tap_plus(4), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(publish.payload, set_temperature("20.5").as_bytes());
        });
    // Home Assistant did not confirm 20.5 °C, so the next step starts from
    // it. The tile already has a mark, so the core does not draw.
    app.update(tap_plus(4), &mut model)
        .expect_light_timer()
        .expect_publish_with(&mut wire, &mut model, |publish| {
            assert_eq!(publish.payload, set_temperature("21.0").as_bytes());
        })
        .expect_pending_timer()
        .expect_session_clear(&wire)
        .expect_no_effects();
    app.update(tap_minus(4), &mut model)
        .expect_light_timer()
        .expect_publish_with(&mut wire, &mut model, |publish| {
            assert_eq!(publish.payload, set_temperature("20.5").as_bytes());
        })
        .expect_pending_timer()
        .expect_session_clear(&wire)
        .expect_no_effects();
    // The tile still shows the target that Home Assistant gave.
    let view = app.view(&model);
    assert_eq!(view.tiles()[4].state, TileState::Pending);
    assert_eq!(view.tiles()[4].detail, "Set 20°");
}

#[test]
fn a_thermostat_with_no_target_temperature_has_no_steps_and_sends_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("dining_room");

    // The first touch turns the front light on. The core has no state for
    // the thermostat.
    app.update(tap_plus(4), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    let assert_no_steps = |model: &mut Model, case: &str| {
        assert!(!app.view(model).tiles()[4].steps, "{case}");
        app.update(tap_plus(4), model)
            .expect_light_timer()
            .expect_no_effects();
        app.update(tap_minus(4), model)
            .expect_light_timer()
            .expect_no_effects();
    };
    assert_no_steps(&mut model, "no state");

    // A thermostat that is off has no target.
    set_state(&mut model, THERMOSTAT, "off");
    set_attribute(&mut model, THERMOSTAT, "current_temperature", "20.3");
    assert_no_steps(&mut model, "off");
    set_attribute(&mut model, THERMOSTAT, "temperature", "null");
    assert_no_steps(&mut model, "off, null");

    // A target that is not a number.
    set_state(&mut model, THERMOSTAT, "heat");
    set_attribute(&mut model, THERMOSTAT, "temperature", r#""warm""#);
    assert_no_steps(&mut model, "not a number");

    // A target, but Home Assistant cannot reach the thermostat.
    set_attribute(&mut model, THERMOSTAT, "temperature", "19.5");
    set_state(&mut model, THERMOSTAT, "unavailable");
    assert_no_steps(&mut model, "unavailable");
    set_state(&mut model, THERMOSTAT, "unknown");
    assert_no_steps(&mut model, "unknown");

    // A thermostat that is off but keeps a target has steps.
    set_state(&mut model, THERMOSTAT, "off");
    assert!(app.view(&model).tiles()[4].steps);
}

#[test]
fn a_tap_on_a_switch_publishes_a_toggle() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("kitchen");

    // Tile 0 is the lights, and tile 1 is the coffee machine.
    app.update(tap_tile(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"switch.toggle","entity_id":"switch.kitchen_coffee_machine_socket"}"#
            );
        });
}

#[test]
fn a_tap_on_a_scene_publishes_scene_turn_on() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("hall");

    app.update(tap_scene(2), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
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
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    // The index page has no tiles and no scenes.
    app.update(tap_tile(0), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    app.update(tap_tile(1), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    app.update(tap_scene(0), &mut model)
        .expect_light_timer()
        .expect_no_effects();

    model.route = Route::room("dining_room");
    // The thermostat and the humidity sensor.
    app.update(tap_tile(4), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    app.update(tap_tile(5), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    app.update(tap_tile(6), &mut model)
        .expect_light_timer()
        .expect_no_effects();
}

#[test]
fn a_tap_before_the_connection_opens_waits_in_the_outbox_and_goes_first() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);

    // The connection is starting, so the core only starts the timeout of the
    // outbox. The toggle has no state, so it does not show the mark.
    app.update(tap_all_lights(), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), QUEUED_TIMEOUT);
        });
    assert_eq!(model.connection.outbox.len(), 1);

    // The service call goes first, so it does not wait behind the subscribe.
    // The call, the status and the subscribe go in one write.
    wire.connect(&mut model)
        .expect_write_with(&mut wire, &mut model, |packets| {
            let publishes: Vec<_> = packets.iter().filter_map(publish_of).collect();
            assert_eq!(
                publishes[0].payload,
                br#"{"service":"light.toggle","entity_id":"light.all_lights"}"#
            );
            assert_eq!(publishes[1].payload, b"online");
            assert_eq!(publishes.len(), 2);
            assert!(matches!(packets.last(), Some(Packet::Subscribe { .. })));
        })
        // The timer of the mark starts when the core issues the call.
        .expect_pending_timer()
        .expect_render()
        .expect_only_session_clear(&wire);
    assert_eq!(model.connection.outbox.len(), 0);
    assert_eq!(model.connection.link(), Link::Up);
}

/// Drives the core to an open connection on the stand, in a dry run.
fn connected_in_a_dry_run() -> Model {
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
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

    app.update(tap_tile(0), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    app.update(tap_scene(2), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    app.update(tap_plus(1), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    app.update(tap_minus(1), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn a_tap_in_a_dry_run_before_the_connection_opens_is_not_kept() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Stand);
    let _ = app.update(Event::SetReadOnly(true), &mut model);

    app.update(tap_all_lights(), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn set_read_only_changes_the_view_and_the_next_tap() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    set_state(&mut model, "light.all_lights", "on");
    assert!(!app.view(&model).read_only);

    app.update(Event::SetReadOnly(true), &mut model)
        .expect_only_render();
    assert!(app.view(&model).read_only);
    // The same value again changes nothing.
    app.update(Event::SetReadOnly(true), &mut model)
        .expect_no_effects();
    app.update(tap_all_lights(), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();

    app.update(Event::SetReadOnly(false), &mut model)
        .expect_only_render();
    assert!(!app.view(&model).read_only);
    app.update(tap_all_lights(), &mut model)
        .expect_light_timer()
        .expect_only_service_call(&mut wire, &mut model);
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
fn start_uses_up_the_dry_run_request() {
    let app = Dashboard;
    let mut model = Model::default();
    let _ = app.update(Event::SetReadOnly(true), &mut model);
    let _ = app.update(start_at(PowerSource::Stand, None), &mut model);
    assert!(app.view(&model).read_only);

    // The user stops the dry run. A later `Start` with settings that do not
    // ask for a dry run does not use the old request.
    let _ = app.update(Event::SetReadOnly(false), &mut model);
    let _ = app.update(start_at(PowerSource::Stand, None), &mut model);
    assert!(!app.view(&model).read_only);
}

#[test]
fn a_second_start_takes_the_dry_run_of_the_shell_again() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::SetReadOnly(true), &mut model);
    assert!(app.view(&model).read_only);

    // The settings of the shell do not ask for a dry run, so the core drops
    // the dry run that the user set after the first `Start`.
    let _ = app.update(start_at(PowerSource::Stand, None), &mut model);
    assert!(!app.view(&model).read_only);
}

#[test]
fn set_read_only_while_asleep_does_not_draw() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    assert!(model.power.asleep);

    app.update(Event::SetReadOnly(true), &mut model)
        .expect_no_effects();
    assert!(app.view(&model).read_only);
    assert!(app.view(&model).status.starts_with("Dry run · Asleep"));
}

#[test]
fn the_status_shows_the_dry_run() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    assert_eq!(app.view(&model).status, "Online");
    let _ = app.update(Event::SetReadOnly(true), &mut model);
    assert_eq!(app.view(&model).status, "Dry run · Online");
}

#[test]
fn a_tap_on_a_navigation_button_turns_the_page() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let count = model.pages.len();

    app.update(Event::Tapped(Some(Control::NextPage)), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).page_index, 1);
    app.update(Event::Tapped(Some(Control::PreviousPage)), &mut model)
        .expect_light_timer()
        .expect_render()
        .expect_full_refresh_timer();
    app.update(Event::Tapped(Some(Control::PreviousPage)), &mut model)
        .expect_light_timer()
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).page_index, count - 1);
    app.update(Event::Tapped(Some(Control::Home)), &mut model)
        .expect_light_timer()
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(app.view(&model).path, "/");
}

#[test]
fn a_tap_on_no_control_while_awake_only_turns_the_light_on() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    app.update(Event::Tapped(None), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
}

#[test]
fn a_tap_on_a_room_button_shows_the_room() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    // The rooms are the pages after the home page, in order.
    app.update(tap_room(7), &mut model)
        .expect_touch_light(STAND_LEVEL)
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
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    app.update(tap_room(9), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    assert_eq!(model.route, Route::Home);
}

#[test]
fn a_tap_on_all_lights_publishes_a_toggle() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    set_state(&mut model, "light.all_lights", "on");
    app.update(tap_all_lights(), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(publish.topic, "dashboard/hall/cmd");
            assert_eq!(
                publish.payload,
                br#"{"service":"light.toggle","entity_id":"light.all_lights"}"#
            );
        });
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.pages = PAGES;
    let view = app.view(&model);
    let shown = &view.index().unwrap().all_lights.as_ref().unwrap().entity_id;
    assert_eq!(shown, "switch.fan");
    app.update(tap_all_lights(), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"switch.toggle","entity_id":"switch.fan"}"#
            );
        });
}
