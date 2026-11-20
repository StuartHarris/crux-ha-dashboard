//! Tests of the pending feature: the mark that a tap gives a tile or a
//! scene until Home Assistant confirms the service call.

use core::time::Duration;

use crux_core::App;

use super::{
    ExpectLight, ExpectServiceCall,
    broker::{iso_time, publish_of},
    connection::{drive_to_connected, drive_to_connecting, send, topic},
    set_state,
    shell::Shell,
    start_at, tap_minus, tap_plus, tap_scene, tap_tile,
    wire::{ExpectWrite, Wire},
};
use crate::{
    Cmd, Dashboard, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    features::{
        connection::{OUTBOX_LIMIT, RECONNECT_MIN},
        light::STAND_LEVEL,
        pending::{PENDING_TIMEOUT, QUEUED_TIMEOUT, SCENE_TIMEOUT},
        power::IDLE_TIMEOUT,
        refresh::DRAW_DELAY,
    },
    mqtt::codec::server::Packet,
};

/// The rocket lamp: tile 1 on the page of the living room.
const LAMP: &str = "light.living_room_rocket_lamp";

/// The time of the last update of the lamp before the tap: 08:41:00 UTC.
const BEFORE: u64 = super::NOW;

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
        .expect_no_effects();
    assert_ne!(model.pending.timer, first);

    app.update(Event::PendingTimeout(first), &mut model)
        .expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // Home Assistant did not confirm the call, so the tile shows the last
    // state that the core has.
    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
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
        .expect_only_render();
    let scenes = app.view(&model).scenes().to_vec();
    assert!(scenes[2].pending);
    assert!(!scenes[0].pending);

    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
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
    let _ = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    let mut sleep = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    assert!(model.power.asleep);
    // The tap turned the light on, so the first step of the sleep turns it
    // off. The core drops the call in the next step.
    let light_off = sleep.resolve_front_light(|_| ()).expect_one_event();
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
    let _ = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    assert!(model.power.asleep);
    // The call went out before the sleep, so the mark stays.
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(Event::PendingTimeout(id), &mut model)
        .expect_no_effects();
    assert!(model.pending.marks.is_empty());
}

#[test]
fn a_tap_that_wakes_the_device_shows_the_mark_through_the_reconnect() {
    let mut shell = super::flows::house();
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
            .any(|event| matches!(event, Event::PendingTimeout(_))),
        "{events:?}"
    );
}

/// The fake device shows each tap in the frame that it draws.
#[test]
fn the_flow_of_a_tap_draws_the_new_state() {
    let mut shell: Shell = super::flows::house();
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
            .any(|event| matches!(event, Event::PendingTimeout(_))),
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

    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
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

    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
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
    app.update(Event::PendingTimeout(sent), &mut model)
        .expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // The timeout of the outbox removes the mark, but the call stays in the
    // outbox.
    app.update(Event::PendingTimeout(queued), &mut model)
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

    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
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
        .expect_only_render();
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
    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
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
