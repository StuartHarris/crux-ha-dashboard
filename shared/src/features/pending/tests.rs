//! Tests of the pending feature: the mark that a tap gives a tile or a
//! scene until Home Assistant confirms the service call.

use crate::features::{pending, power};
use core::time::Duration;

use crux_core::App;

use crate::tests::{
    ExpectLight, ExpectServiceCall, ExpectStep,
    broker::{iso_time, publish_of},
    drive_to_connected, drive_to_connecting, send, set_state,
    shell::Shell,
    start_at, tap_minus, tap_plus, tap_scene, tap_tile, topic,
    wire::{ExpectWrite, Wire},
};
use crate::{
    Cmd, Dashboard, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    features::{
        connection::{OUTBOX_LIMIT, RECONNECT_MIN},
        light::STAND_LEVEL,
        pending::{PENDING_TIMEOUT, QUEUED_TIMEOUT, SCENE_TIMEOUT},
        power::IDLE_TIMEOUT,
        refresh::{DRAW_DELAY, redraw},
    },
    mqtt::{MAX_IN_FLIGHT, codec::server::Packet},
};

/// The rocket lamp: tile 1 on the page of the living room.
const LAMP: &str = "light.living_room_rocket_lamp";

/// The time of the last update of the lamp before the tap: 08:41:00 UTC.
const BEFORE: u64 = crate::tests::NOW;

/// Returns the state of tile `index` in the view.
fn tile_state(model: &Model, index: usize) -> TileState {
    Dashboard.view(model).tiles()[index].state
}

/// Drives the core to an open connection on the stand, on the page of the
/// living room. The lamp is on, with a `last_updated` time of [`BEFORE`].
fn lamp_on() -> (Model, Wire) {
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    let mut state = send(&mut model, &mut wire, &topic(LAMP, "state"), "on");
    let updated = iso_time(BEFORE);
    let _ = send(
        &mut model,
        &mut wire,
        &topic(LAMP, "last_updated"),
        &updated,
    );
    let _ = fire_draw_timer(&mut model, &mut state);
    assert_eq!(tile_state(&model, 1), TileState::On);
    (model, wire)
}

/// Fires the draw timer that `cmd` started, and returns the command of the
/// draw.
#[track_caller]
fn fire_draw_timer(model: &mut Model, cmd: &mut Cmd) -> Cmd {
    let mut timer = cmd.expect_one_effect().expect_time_notify_after();
    assert_eq!(Duration::from(timer.operation.duration), DRAW_DELAY);
    let id = timer.operation.id;
    timer.resolve(id).unwrap();
    let event = cmd.expect_one_event();
    Dashboard.update(event, model)
}

#[test]
fn a_tap_marks_the_tile_until_a_newer_last_updated_confirms_it() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();

    // The tap draws the mark at once, and keeps the text of the last state.
    app.update(tap_tile(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call(&mut wire, &mut model);
    let view = app.view(&model);
    assert_eq!(view.tiles()[1].state, TileState::Pending);
    assert_eq!(view.tiles()[1].text, "On");

    // The new state alone does not confirm the tap. It starts the draw
    // timer, as each change does.
    let mut state = send(&mut model, &mut wire, &topic(LAMP, "state"), "off");
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // A `last_updated` time that is not later than the time at the tap does
    // not confirm it. An example is the retained message after a wake.
    let old = iso_time(BEFORE);
    send(&mut model, &mut wire, &topic(LAMP, "last_updated"), &old).expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // A later time in the same minute confirms the tap. The draw timer of
    // the state already waits, so the confirm does not start another one.
    let new = iso_time(BEFORE + 1);
    send(&mut model, &mut wire, &topic(LAMP, "last_updated"), &new).expect_no_effects();
    assert!(model.pending.marks.is_empty());

    // The one draw shows the new state.
    fire_draw_timer(&mut model, &mut state).expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::Off);
}

#[test]
fn a_confirm_with_no_other_change_draws_after_the_draw_delay() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();
    wire.settle(app.update(tap_plus(1), &mut model), &mut model);

    // Home Assistant changed the brightness. The state and the minute did not
    // change, but the time of the last update confirms the step. Only the
    // mark changes, and the draw waits for the attributes, which Home
    // Assistant sends after `last_updated`.
    let new = iso_time(BEFORE + 1);
    let mut confirmed = send(&mut model, &mut wire, &topic(LAMP, "last_updated"), &new);
    assert_eq!(tile_state(&model, 1), TileState::On);
    fire_draw_timer(&mut model, &mut confirmed).expect_only_render();
}

#[test]
fn the_timeout_shows_the_last_state_and_an_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();

    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let first = model.pending.timer;
    // A second tap on the tile still sends its call. Its timer replaces the
    // first one.
    let mut second_tap = app.update(tap_tile(1), &mut model);
    second_tap
        .expect_light_timer()
        .expect_publish(&mut wire, &mut model)
        .expect_pending_timer()
        .expect_session_clear(&wire)
        .expect_no_effects();
    assert_ne!(model.pending.timer, first);

    app.update(Event::Pending(pending::Event::Timeout(first)), &mut model)
        .expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // Home Assistant did not confirm the call, so the tile shows the last
    // state that the core has.
    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
}

#[test]
fn a_step_keeps_the_brightness_text_while_it_waits() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();
    let _ = send(&mut model, &mut wire, &topic(LAMP, "brightness"), "128");

    app.update(tap_plus(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call(&mut wire, &mut model);
    let view = app.view(&model);
    let tile = &view.tiles()[1];
    assert_eq!(tile.state, TileState::Pending);
    assert_eq!(tile.text, "On 50%");
    assert_eq!(tile.detail, "50%");
    assert!(tile.steps);
}

#[test]
fn a_scene_mark_ends_after_the_scene_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("hall");

    app.update(tap_scene(2), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_publish(&mut wire, &mut model)
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), SCENE_TIMEOUT);
        })
        .expect_render()
        .expect_only_session_clear(&wire);
    let scenes = app.view(&model).scenes().to_vec();
    assert!(scenes[2].pending);
    assert!(!scenes[0].pending);

    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert!(!app.view(&model).scenes()[2].pending);
}

#[test]
fn a_tile_that_is_not_on_or_off_does_not_show_the_mark() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    // The core has no state for the lamp.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Unknown);

    // Home Assistant cannot reach the lamp. The tile keeps the layout of an
    // unavailable tile.
    let _ = send(&mut model, &mut wire, &topic(LAMP, "state"), "unavailable");
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Unavailable);
}

#[test]
fn a_dry_run_gives_no_mark() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::SetReadOnly(true), &mut model);
    model.route = Route::room("hall");

    app.update(tap_tile(0), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_no_effects();
    app.update(tap_scene(2), &mut model)
        .expect_light_timer()
        .expect_no_effects();
    assert!(model.pending.marks.is_empty());
    assert_ne!(tile_state(&model, 0), TileState::Pending);
}

#[test]
fn a_call_that_the_full_outbox_drops_loses_its_mark() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, "light.living_room_lights", "off");
    set_state(&mut model, LAMP, "on");

    let _ = app.update(tap_tile(1), &mut model);
    for _ in 0..OUTBOX_LIMIT {
        let _ = app.update(tap_tile(0), &mut model);
    }
    // The outbox dropped the call for the lamp. The calls for the lights
    // still wait, so the lights keep their mark.
    assert_eq!(model.connection.outbox.len(), OUTBOX_LIMIT);
    assert_eq!(tile_state(&model, 0), TileState::Pending);
    assert_ne!(tile_state(&model, 1), TileState::Pending);
}

#[test]
fn a_call_that_the_sleep_drops_loses_its_mark() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Battery);
    model.route = Route::room("living_room");
    set_state(&mut model, LAMP, "on");

    let _ = app.update(tap_tile(1), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Pending);
    // The first idle timeout waits one more period for the connection.
    let _ = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let mut sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    assert!(model.power.is_asleep());
    // The tap turned the light on, so the first step of the sleep turns it
    // off. The core drops the call in the next step.
    let light_off = sleep
        .expect_step("sleep 1 turns the front light off")
        .resolve_front_light(|_| ())
        .expect_one_event();
    assert_eq!(model.unsent, 0);
    let _ = app.update(light_off, &mut model);
    assert_eq!(model.unsent, 1);
    assert!(model.pending.marks.is_empty());
    assert_ne!(tile_state(&model, 1), TileState::Pending);
}

#[test]
fn a_timeout_while_asleep_removes_the_mark_but_does_not_draw() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    model.route = Route::room("living_room");
    set_state(&mut model, LAMP, "on");

    let _ = app.update(tap_tile(1), &mut model);
    let id = model.pending.timer;
    let _ = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    assert!(model.power.is_asleep());
    // The call went out before the sleep, so the mark stays.
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(Event::Pending(pending::Event::Timeout(id)), &mut model)
        .expect_no_effects();
    assert!(model.pending.marks.is_empty());
}

#[test]
fn a_tap_that_wakes_the_device_shows_the_mark_through_the_reconnect() {
    let mut shell = crate::tests::flows::house();
    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.view().asleep);

    // The broker is down, so the call waits in the outbox. The mark shows,
    // and its timer does not start yet.
    shell.broker.down = true;
    shell.take_log();
    shell.send(tap_tile(1));
    let mark_timer = alloc::format!("TimeNotifyAfter {}ms", PENDING_TIMEOUT.as_millis());
    let log = shell.take_log();
    assert!(!log.contains(&mark_timer), "{log:?}");
    assert!(!shell.view().asleep);
    let frame = shell.frames.last().unwrap();
    assert_eq!(frame.view.tiles()[1].state, TileState::Pending);
    assert_eq!(shell.model.connection.outbox.len(), 1);

    // The connection opens. The timer of the mark starts when the core
    // issues the call, and Home Assistant confirms the call.
    shell.broker.down = false;
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    let call = log
        .iter()
        .position(|effect| effect.starts_with("MqttPublish dashboard/hall/cmd"))
        .expect("the service call");
    // The call, `online` and the subscribe go in one write, and the timer
    // comes after the write.
    assert!(log[call + 1..].contains(&mark_timer), "{log:?}");
    let frame = shell.frames.last().unwrap();
    assert_eq!(frame.view.tiles()[1].state, TileState::Off);
    // The confirm cleared the timer of the mark.
    let events = shell.advance(PENDING_TIMEOUT);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Pending(pending::Event::Timeout(_)))),
        "{events:?}"
    );
}

/// The fake device shows each tap in the frame that it draws.
#[test]
fn the_flow_of_a_tap_draws_the_new_state() {
    let mut shell: Shell = crate::tests::flows::house();
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    shell.send(tap_tile(1));
    // The fake Home Assistant answers at once, so the frame shows the new
    // state, and no timer of a mark waits.
    let frame = shell.frames.last().unwrap();
    assert_eq!(frame.view.tiles()[1].state, TileState::Off);
    assert!(!frame.full);
    let events = shell.advance(PENDING_TIMEOUT);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Pending(pending::Event::Timeout(_)))),
        "{events:?}"
    );
}

/// Sends the `last_updated` time `seconds` of the lamp, and returns the
/// command of the update.
fn lamp_updated(model: &mut Model, wire: &mut Wire, seconds: u64) -> Cmd {
    let time = iso_time(seconds);
    send(model, wire, &topic(LAMP, "last_updated"), &time)
}

#[test]
fn a_toggle_with_the_same_state_and_a_newer_time_is_not_confirmed() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);

    // Another source updated the lamp, but its state did not change.
    let _ = lamp_updated(&mut model, &mut wire, BEFORE + 1);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // The new state confirms the toggle.
    let _ = send(&mut model, &mut wire, &topic(LAMP, "state"), "off");
    assert!(model.pending.marks.is_empty());
    assert_eq!(tile_state(&model, 1), TileState::Off);
}

#[test]
fn a_toggle_with_a_state_but_no_time_at_the_tap_is_confirmed_by_the_state() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    // The core has the state of the lamp, but no time.
    set_state(&mut model, LAMP, "on");
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // The same state does not confirm the toggle. A new state does.
    let _ = send(&mut model, &mut wire, &topic(LAMP, "state"), "on");
    assert_eq!(tile_state(&model, 1), TileState::Pending);
    let _ = send(&mut model, &mut wire, &topic(LAMP, "state"), "off");
    assert_eq!(tile_state(&model, 1), TileState::Off);
}

#[test]
fn a_toggle_with_no_state_at_the_tap_ends_only_at_the_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    // The tap comes before the retained messages.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Unknown);

    // The retained state can be older than the call, so it does not confirm
    // the call. The tile now shows the mark.
    let _ = send(&mut model, &mut wire, &topic(LAMP, "state"), "on");
    let _ = lamp_updated(&mut model, &mut wire, BEFORE);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
}

#[test]
fn a_step_with_no_time_at_the_tap_ends_only_at_the_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    // The core has the state of the lamp, but no time.
    set_state(&mut model, LAMP, "on");
    wire.settle(app.update(tap_plus(1), &mut model), &mut model);

    let _ = lamp_updated(&mut model, &mut wire, BEFORE + 1);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
}

#[test]
fn a_timeout_keeps_the_mark_while_a_newer_call_waits_in_the_outbox() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();

    // The first call goes out, and then the connection closes.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let sent = model.pending.timer;
    let closed = wire.drop_connection(&mut model, "connection lost");
    wire.settle(closed, &mut model);

    // The second call waits in the outbox.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let queued = model.pending.timer;
    assert_eq!(model.connection.outbox.len(), 1);

    // The timeout of the first call does not remove the mark.
    app.update(Event::Pending(pending::Event::Timeout(sent)), &mut model)
        .expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // The timeout of the outbox removes the mark, but the call stays in the
    // outbox.
    app.update(Event::Pending(pending::Event::Timeout(queued)), &mut model)
        .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
    assert_eq!(model.connection.outbox.len(), 1);
}

#[test]
fn a_mark_whose_call_cannot_go_out_ends_after_the_queued_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, LAMP, "on");

    app.update(tap_tile(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), QUEUED_TIMEOUT);
        })
        .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);

    // The connection opens later, and the call still goes out, with no mark.
    wire.connect(&mut model)
        .expect_write_with(&mut wire, &mut model, |packets| {
            let call = publish_of(&packets[0]).expect("a publish");
            assert_eq!(
                call.payload,
                br#"{"service":"light.toggle","entity_id":"light.living_room_rocket_lamp"}"#
            );
            assert_eq!(publish_of(&packets[1]).unwrap().payload, b"online");
            assert!(matches!(packets[2], Packet::Subscribe { .. }));
        });
    assert!(model.pending.marks.is_empty());
}

/// The thermostat: tile 4 on the page of the dining room.
const THERMOSTAT: &str = "climate.dining_room_thermostat";

/// Drives the core to an open connection on the stand, on the page of the
/// dining room. The thermostat heats at 20.3 °C, with the target 19.5 °C and
/// a `last_updated` time of [`BEFORE`].
fn thermostat_heats() -> (Model, Wire) {
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("dining_room");
    let updated = iso_time(BEFORE);
    let mut first = Cmd::done();
    for (index, (key, payload)) in [
        ("state", "heat"),
        ("last_updated", updated.as_str()),
        ("current_temperature", "20.3"),
        ("temperature", "19.5"),
    ]
    .into_iter()
    .enumerate()
    {
        let command = send(&mut model, &mut wire, &topic(THERMOSTAT, key), payload);
        if index == 0 {
            first = command;
        }
    }
    let _ = fire_draw_timer(&mut model, &mut first);
    let view = Dashboard.view(&model);
    let tile = &view.tiles()[4];
    assert_eq!(tile.state, TileState::Value);
    assert_eq!(tile.text, "20.3°");
    assert_eq!(tile.detail, "Set 19.5°");
    assert!(tile.steps);
    (model, wire)
}

#[test]
fn a_thermostat_step_waits_for_the_new_target_and_not_only_a_newer_time() {
    let app = Dashboard;
    let (mut model, mut wire) = thermostat_heats();

    app.update(tap_plus(4), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call(&mut wire, &mut model);
    // The tile shows the mark, with the target that Home Assistant gave.
    let view = app.view(&model);
    assert_eq!(view.tiles()[4].state, TileState::Pending);
    assert_eq!(view.tiles()[4].detail, "Set 19.5°");

    // The room gets warmer. The time of the last update is newer, but the
    // target did not change, so the step waits.
    let new = iso_time(BEFORE + 1);
    let _ = send(
        &mut model,
        &mut wire,
        &topic(THERMOSTAT, "last_updated"),
        &new,
    );
    let mut warmer = send(
        &mut model,
        &mut wire,
        &topic(THERMOSTAT, "current_temperature"),
        "20.4",
    );
    assert_eq!(tile_state(&model, 4), TileState::Pending);
    let _ = fire_draw_timer(&mut model, &mut warmer);

    // Home Assistant sets the target. The time comes first, then the new
    // target confirms the step.
    let newer = iso_time(BEFORE + 2);
    send(
        &mut model,
        &mut wire,
        &topic(THERMOSTAT, "last_updated"),
        &newer,
    )
    .expect_no_effects();
    assert_eq!(tile_state(&model, 4), TileState::Pending);
    let mut target = send(
        &mut model,
        &mut wire,
        &topic(THERMOSTAT, "temperature"),
        "20.0",
    );
    assert!(model.pending.marks.is_empty());
    fire_draw_timer(&mut model, &mut target).expect_only_render();
    let view = app.view(&model);
    assert_eq!(view.tiles()[4].state, TileState::Value);
    assert_eq!(view.tiles()[4].detail, "Set 20.0°");
}

#[test]
fn the_target_of_an_earlier_step_does_not_confirm_a_later_step() {
    let app = Dashboard;
    let (mut model, mut wire) = thermostat_heats();

    // Two taps on + before Home Assistant answers ask for 20 °C, then 20.5 °C.
    wire.settle(app.update(tap_plus(4), &mut model), &mut model);
    app.update(tap_plus(4), &mut model)
        .expect_light_timer()
        .expect_publish_with(&mut wire, &mut model, |publish| {
            let payload = core::str::from_utf8(&publish.payload).unwrap();
            assert!(payload.ends_with(r#""temperature":20.5}"#), "{payload}");
        })
        .expect_pending_timer()
        .expect_session_clear(&wire)
        .expect_no_effects();

    // Home Assistant does the first call. Its target is not the target of
    // the last step, so the mark stays.
    let first = iso_time(BEFORE + 1);
    let _ = send(
        &mut model,
        &mut wire,
        &topic(THERMOSTAT, "last_updated"),
        &first,
    );
    let _ = send(
        &mut model,
        &mut wire,
        &topic(THERMOSTAT, "temperature"),
        "20.0",
    );
    assert_eq!(tile_state(&model, 4), TileState::Pending);

    // A third tap starts from the target that waits, not from 20 °C. The
    // tap also draws the target that Home Assistant gave, 20 °C, with the
    // mark.
    app.update(tap_minus(4), &mut model)
        .expect_light_timer()
        .expect_publish_with(&mut wire, &mut model, |publish| {
            let payload = core::str::from_utf8(&publish.payload).unwrap();
            assert!(payload.ends_with(r#""temperature":20.0}"#), "{payload}");
        })
        .expect_pending_timer()
        .expect_render()
        .expect_only_session_clear(&wire);
    let view = app.view(&model);
    assert_eq!(view.tiles()[4].state, TileState::Pending);
    assert_eq!(view.tiles()[4].detail, "Set 20.0°");
}

#[test]
fn after_the_timeout_a_thermostat_step_starts_from_the_target_of_home_assistant() {
    let app = Dashboard;
    let (mut model, mut wire) = thermostat_heats();

    // Home Assistant does not answer, for example because the target is at
    // its limit.
    wire.settle(app.update(tap_plus(4), &mut model), &mut model);
    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert!(model.pending.marks.is_empty());
    assert_eq!(tile_state(&model, 4), TileState::Value);

    // The mark is gone, so the next step starts from 19.5 °C again.
    app.update(tap_plus(4), &mut model)
        .expect_light_timer()
        .expect_only_service_call_with(&mut wire, &mut model, |publish| {
            let payload = core::str::from_utf8(&publish.payload).unwrap();
            assert!(payload.ends_with(r#""temperature":20.0}"#), "{payload}");
        });
}

#[test]
fn a_second_call_clears_the_timeout_of_the_first_and_only_the_new_timeout_fires() {
    let mut shell = crate::tests::flows::house();
    // Home Assistant does not answer, so no state confirms a call.
    shell.broker.ignore_calls = true;
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    shell.send(tap_tile(1));
    shell.take_log();

    shell.send(tap_tile(1));
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");

    let events = shell.advance(PENDING_TIMEOUT);
    let timeouts = events
        .iter()
        .filter(|event| matches!(event, Event::Pending(pending::Event::Timeout(_))))
        .count();
    assert_eq!(timeouts, 1, "{events:?}");
    assert_eq!(shell.view().tiles()[1].state, TileState::On);
}

/// Returns the number of queued timers that `cmd` starts.
fn queued_timers(cmd: &mut Cmd) -> usize {
    cmd.effects()
        .filter(|effect| match effect {
            crate::Effect::TimeNotifyAfter(request) => {
                Duration::from(request.operation.duration) == QUEUED_TIMEOUT
            }
            _ => false,
        })
        .count()
}

#[test]
fn a_sleep_keeps_the_mark_of_a_call_that_went_out_through_the_queued_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();

    // The first call goes out, then the connection closes, and the second
    // call waits in the outbox.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let closed = wire.drop_connection(&mut model, "connection lost");
    wire.settle(closed, &mut model);
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let queued = model.pending.timer;

    // The device sleeps, and the sleep drops the call that waits.
    let _ = app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model);
    let _ = app.update(Event::SleepNow, &mut model);
    let mut sleep = app.update(Event::SleepNow, &mut model);
    assert!(model.power.is_asleep());
    let light_off = sleep
        .expect_step("sleep 1 turns the front light off")
        .resolve_front_light(|_| ())
        .expect_one_event();
    let _ = app.update(light_off, &mut model);
    assert!(model.connection.outbox.is_empty());

    // The first call went out, so its mark stays.
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // No call waits, so the queued timeout does not remove the mark.
    app.update(Event::Pending(pending::Event::Timeout(queued)), &mut model)
        .expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);
}

#[test]
fn a_call_that_goes_out_ends_the_queued_timer() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, LAMP, "on");
    assert_eq!(queued_timers(&mut app.update(tap_tile(1), &mut model)), 1);

    // The connection opens, and the call goes out.
    let connected = wire.connect(&mut model);
    wire.settle(connected, &mut model);
    assert!(model.connection.outbox.is_empty());

    // The connection closes. The next call that waits gets a new queued
    // timer, because the old one stopped when the first call went out.
    let closed = wire.drop_connection(&mut model, "connection lost");
    wire.settle(closed, &mut model);
    assert_eq!(queued_timers(&mut app.update(tap_tile(1), &mut model)), 1);
}

#[test]
fn a_second_call_that_waits_keeps_the_first_queued_timer() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, LAMP, "on");

    assert_eq!(queued_timers(&mut app.update(tap_tile(1), &mut model)), 1);
    // The first queued timer still runs, so the second call starts no timer.
    assert_eq!(queued_timers(&mut app.update(tap_tile(1), &mut model)), 0);
}

#[test]
fn an_issued_call_keeps_the_queued_timer_while_another_call_waits() {
    let mut shell = crate::tests::flows::house();
    // Home Assistant does not answer, so no state confirms a call.
    shell.broker.ignore_calls = true;
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));

    // The broker holds each PUBACK. The calls for the standard lamp (tile 2)
    // fill the session, so the session refuses the next publish as full.
    shell.broker.hold_puback = true;
    for _ in 0..MAX_IN_FLIGHT {
        shell.send(tap_tile(2));
    }
    assert!(shell.model.connection.outbox.is_empty());

    // Two calls for the rocket lamp wait in the outbox.
    shell.send(tap_tile(1));
    shell.send(tap_tile(1));
    assert_eq!(shell.model.connection.outbox.len(), 2);

    // One PUBACK arrives. The drain issues one call, and the session refuses
    // the other call, which stays in the outbox.
    let held = shell.broker.held_pubacks.split_off(1);
    shell.broker.release_pubacks();
    shell.broker.held_pubacks = held;
    shell.broker.flush();
    shell.run();
    assert_eq!(shell.broker.service_calls.len(), MAX_IN_FLIGHT + 1);
    assert_eq!(shell.model.connection.outbox.len(), 1);
    assert_eq!(shell.view().tiles()[1].state, TileState::Pending);

    // The timeout of the issued call does not remove the mark, because a
    // call waits and its queued timer runs.
    shell.advance(PENDING_TIMEOUT);
    assert_eq!(shell.view().tiles()[1].state, TileState::Pending);

    // A call still waits, so the queued timeout removes the mark. The
    // call stays in the outbox. In this flow, the issued timer and the
    // queued timer start at the same time on the clock of the shell.
    let rest = QUEUED_TIMEOUT
        .checked_sub(PENDING_TIMEOUT)
        .expect("the queued timeout is longer than the pending timeout");
    shell.advance(rest);
    assert_ne!(shell.view().tiles()[1].state, TileState::Pending);
    assert_eq!(shell.model.connection.outbox.len(), 1);
}

#[test]
fn a_sleep_clears_the_queued_timer_of_a_mark_whose_call_went_out() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();

    // The first call goes out, then the connection closes, and the second
    // call waits in the outbox. The wire keeps the command of the queued
    // timer.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let closed = wire.drop_connection(&mut model, "connection lost");
    wire.settle(closed, &mut model);
    wire.effects.clear();
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let queued = wire
        .effects
        .iter()
        .find_map(|effect| match effect {
            crate::Effect::TimeNotifyAfter(request)
                if Duration::from(request.operation.duration) == QUEUED_TIMEOUT =>
            {
                Some(request.operation.id)
            }
            _ => None,
        })
        .expect("the queued timer");
    wire.effects.clear();

    // The device sleeps, and the sleep drops the call that waits.
    let _ = app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model);
    let _ = app.update(Event::SleepNow, &mut model);
    let mut sleep = app.update(Event::SleepNow, &mut model);
    let light_off = sleep
        .expect_step("sleep 1 turns the front light off")
        .resolve_front_light(|_| ())
        .expect_one_event();
    let _ = app.update(light_off, &mut model);
    assert!(model.connection.outbox.is_empty());

    // The first call went out, so the mark stays. No call waits, so the
    // core clears the queued timer.
    assert_eq!(tile_state(&model, 1), TileState::Pending);
    wire.run(&mut model);
    assert!(
        wire.effects.iter().any(|effect| matches!(
            effect,
            crate::Effect::TimeClear(request) if request.operation.id == queued
        )),
        "{:?}",
        wire.effects
    );
}

/// No flow of events reaches this state: the drain fills each place in the
/// session before a new tap can use it. Thus the test makes the state with
/// the functions of the feature.
#[test]
fn an_issued_timeout_removes_a_mark_that_has_no_queued_timer_while_an_old_call_waits() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Stand);
    model.route = Route::room("living_room");
    set_state(&mut model, LAMP, "on");

    // The queued timeout removes the mark, and the call stays in the outbox.
    let _ = app.update(tap_tile(1), &mut model);
    let _ = app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    );
    assert!(model.pending.marks.is_empty());
    assert_eq!(model.connection.outbox.len(), 1);

    // A new call for the lamp goes out while the old call waits. Its mark has
    // only the timeout of the issued call. The draw shows the mark, as after
    // a tap.
    let tap = pending::tap(&model, LAMP, pending::Confirm::Toggle);
    let _ = pending::issued(LAMP, Some(tap), &mut model);
    redraw(&mut model, false).expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // No timer stays after the timeout, so the core removes the mark.
    app.update(
        Event::Pending(pending::Event::Timeout(model.pending.timer)),
        &mut model,
    )
    .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
    assert_eq!(model.connection.outbox.len(), 1);
}

#[test]
fn a_sleep_keeps_the_mark_of_a_call_that_went_out_until_its_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();

    // The call goes out, and no other call waits.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let issued = model.pending.timer;
    assert!(model.connection.outbox.is_empty());

    // The device sleeps. The sleep drops the outbox, which is empty.
    let _ = app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model);
    let mut sleep = app.update(Event::SleepNow, &mut model);
    assert!(model.power.is_asleep());
    let light_off = sleep
        .expect_step("sleep 1 turns the front light off")
        .resolve_front_light(|_| ())
        .expect_one_event();
    let _ = app.update(light_off, &mut model);

    // The call went out, so the mark stays until its timeout.
    assert!(pending::is_pending(&model, LAMP));
    app.update(Event::Pending(pending::Event::Timeout(issued)), &mut model)
        .expect_no_effects();
    assert!(!pending::is_pending(&model, LAMP));
}

#[test]
fn a_full_outbox_clears_the_queued_timer_of_a_mark_whose_call_went_out() {
    let app = Dashboard;
    let (mut model, mut wire) = lamp_on();
    set_state(&mut model, "light.living_room_lights", "off");

    // The first call goes out, then the connection closes, and the second
    // call waits in the outbox. The wire keeps the command of the queued
    // timer.
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let closed = wire.drop_connection(&mut model, "connection lost");
    wire.settle(closed, &mut model);
    wire.effects.clear();
    wire.settle(app.update(tap_tile(1), &mut model), &mut model);
    let queued = wire
        .effects
        .iter()
        .find_map(|effect| match effect {
            crate::Effect::TimeNotifyAfter(request)
                if Duration::from(request.operation.duration) == QUEUED_TIMEOUT =>
            {
                Some(request.operation.id)
            }
            _ => None,
        })
        .expect("the queued timer");
    wire.effects.clear();

    // The calls for the lights fill the outbox, and the last one drops the
    // call for the lamp.
    for _ in 0..OUTBOX_LIMIT {
        wire.settle(app.update(tap_tile(0), &mut model), &mut model);
    }
    assert_eq!(model.connection.outbox.len(), OUTBOX_LIMIT);
    assert!(
        model
            .connection
            .outbox
            .iter()
            .all(|call| call.entity_id != LAMP)
    );

    // The first call went out, so the mark stays. No call waits, so the
    // core clears the queued timer.
    assert_eq!(tile_state(&model, 1), TileState::Pending);
    wire.run(&mut model);
    assert!(
        wire.effects.iter().any(|effect| matches!(
            effect,
            crate::Effect::TimeClear(request) if request.operation.id == queued
        )),
        "{:?}",
        wire.effects
    );
}
