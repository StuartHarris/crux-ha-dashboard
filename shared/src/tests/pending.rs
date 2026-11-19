//! Tests of the pending feature: the mark that a tap gives a tile or a
//! scene until Home Assistant confirms the service call.

use core::time::Duration;

use crux_core::{App, Request};

use super::{
    ExpectLight, ExpectServiceCall,
    broker::iso_time,
    connection::{drive_to_connected, drive_to_connecting, send, topic},
    set_state,
    shell::Shell,
    start_at, tap_brighter, tap_scene, tap_tile,
};
use crate::{
    Cmd, Dashboard, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    capabilities::mqtt::{Connect, MqttEvent},
    features::{
        connection::{OUTBOX_LIMIT, RECONNECT_MIN},
        light::STAND_LEVEL,
        pending::{PENDING_TIMEOUT, QUEUED_TIMEOUT, SCENE_TIMEOUT},
        power::IDLE_TIMEOUT,
        refresh::DRAW_DELAY,
    },
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
fn lamp_on() -> (Model, Cmd, Request<Connect>) {
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    let mut state = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "on",
    );
    let updated = iso_time(BEFORE);
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "last_updated"),
        &updated,
    );
    let _ = fire_draw_timer(&mut model, &mut state);
    assert_eq!(tile_state(&model, 1), TileState::On);
    (model, cmd, request)
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
    let (mut model, mut cmd, mut request) = lamp_on();

    // The tap draws the mark at once, and keeps the text of the last state.
    app.update(tap_tile(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call();
    let view = app.view(&model);
    assert_eq!(view.tiles()[1].state, TileState::Pending);
    assert_eq!(view.tiles()[1].text, "On");

    // The new state alone does not confirm the tap. It starts the draw
    // timer, as each change does.
    let mut state = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "off",
    );
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // A `last_updated` time that is not later than the time at the tap does
    // not confirm it. An example is the retained message after a wake.
    let old = iso_time(BEFORE);
    send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "last_updated"),
        &old,
    )
    .expect_no_effects();
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // A later time in the same minute confirms the tap. The draw timer of
    // the state already waits, so the confirm does not start another one.
    let new = iso_time(BEFORE + 1);
    send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "last_updated"),
        &new,
    )
    .expect_no_effects();
    assert!(model.pending.marks.is_empty());

    // The one draw shows the new state.
    fire_draw_timer(&mut model, &mut state).expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::Off);
}

#[test]
fn a_confirm_with_no_other_change_draws_after_the_draw_delay() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = lamp_on();
    let _ = app.update(tap_brighter(1), &mut model);

    // Home Assistant changed the brightness. The state and the minute did not
    // change, but the time of the last update confirms the step. Only the
    // mark changes, and the draw waits for the attributes, which Home
    // Assistant sends after `last_updated`.
    let new = iso_time(BEFORE + 1);
    let mut confirmed = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "last_updated"),
        &new,
    );
    assert_eq!(tile_state(&model, 1), TileState::On);
    fire_draw_timer(&mut model, &mut confirmed).expect_only_render();
}

#[test]
fn the_timeout_shows_the_last_state_and_an_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = lamp_on();

    let _ = app.update(tap_tile(1), &mut model);
    let first = model.pending.timer;
    // A second tap on the tile still sends its call. Its timer replaces the
    // first one.
    let mut second_tap = app.update(tap_tile(1), &mut model);
    second_tap
        .expect_light_timer()
        .expect_mqtt_publish()
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
    let (mut model, mut cmd, mut request) = lamp_on();
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "brightness"),
        "128",
    );

    app.update(tap_brighter(1), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_only_service_call();
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("hall");

    app.update(tap_scene(2), &mut model)
        .expect_touch_light(STAND_LEVEL)
        .expect_mqtt_publish()
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
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    // The core has no state for the lamp.
    let _ = app.update(tap_tile(1), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Unknown);

    // Home Assistant cannot reach the lamp. The tile keeps the layout of an
    // unavailable tile.
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "unavailable",
    );
    let _ = app.update(tap_tile(1), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Unavailable);
}

#[test]
fn a_dry_run_gives_no_mark() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connecting(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connecting(PowerSource::Battery);
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
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
    assert_eq!(log[call + 1], mark_timer);
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
fn lamp_updated(
    model: &mut Model,
    cmd: &mut Cmd,
    request: &mut Request<Connect>,
    seconds: u64,
) -> Cmd {
    let time = iso_time(seconds);
    send(model, cmd, request, &topic(LAMP, "last_updated"), &time)
}

#[test]
fn a_toggle_with_the_same_state_and_a_newer_time_is_not_confirmed() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = lamp_on();
    let _ = app.update(tap_tile(1), &mut model);

    // Another source updated the lamp, but its state did not change.
    let _ = lamp_updated(&mut model, &mut cmd, &mut request, BEFORE + 1);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // The new state confirms the toggle.
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "off",
    );
    assert!(model.pending.marks.is_empty());
    assert_eq!(tile_state(&model, 1), TileState::Off);
}

#[test]
fn a_toggle_with_a_state_but_no_time_at_the_tap_is_confirmed_by_the_state() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    // The core has the state of the lamp, but no time.
    set_state(&mut model, LAMP, "on");
    let _ = app.update(tap_tile(1), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    // The same state does not confirm the toggle. A new state does.
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "on",
    );
    assert_eq!(tile_state(&model, 1), TileState::Pending);
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "off",
    );
    assert_eq!(tile_state(&model, 1), TileState::Off);
}

#[test]
fn a_toggle_with_no_state_at_the_tap_ends_only_at_the_timeout() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");

    // The tap comes before the retained messages.
    let _ = app.update(tap_tile(1), &mut model);
    assert_eq!(tile_state(&model, 1), TileState::Unknown);

    // The retained state can be older than the call, so it does not confirm
    // the call. The tile now shows the mark.
    let _ = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(LAMP, "state"),
        "on",
    );
    let _ = lamp_updated(&mut model, &mut cmd, &mut request, BEFORE);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
        .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
}

#[test]
fn a_step_with_no_time_at_the_tap_ends_only_at_the_timeout() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    // The core has the state of the lamp, but no time.
    set_state(&mut model, LAMP, "on");
    let _ = app.update(tap_brighter(1), &mut model);

    let _ = lamp_updated(&mut model, &mut cmd, &mut request, BEFORE + 1);
    assert_eq!(tile_state(&model, 1), TileState::Pending);

    app.update(Event::PendingTimeout(model.pending.timer), &mut model)
        .expect_only_render();
    assert_eq!(tile_state(&model, 1), TileState::On);
}

#[test]
fn a_timeout_keeps_the_mark_while_a_newer_call_waits_in_the_outbox() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = lamp_on();

    // The first call goes out, and then the connection closes.
    let _ = app.update(tap_tile(1), &mut model);
    let sent = model.pending.timer;
    request
        .resolve(MqttEvent::Disconnected { reason: None })
        .unwrap();
    let event = cmd.expect_one_event();
    let _ = app.update(event, &mut model);

    // The second call waits in the outbox.
    let _ = app.update(tap_tile(1), &mut model);
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
    let (mut model, mut cmd, mut request) = drive_to_connecting(PowerSource::Stand);
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
    request.resolve(MqttEvent::Connected).unwrap();
    let event = cmd.expect_one_event();
    app.update(event, &mut model)
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(
                publish.payload,
                br#"{"service":"light.toggle","entity_id":"light.living_room_rocket_lamp"}"#
            );
        })
        .expect_mqtt_publish()
        .expect_mqtt_subscribe();
    assert!(model.pending.marks.is_empty());
}
