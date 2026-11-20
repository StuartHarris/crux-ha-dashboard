//! Tests of the power feature: the idle timer, sleep and the power source.

use crate::features::{power, wifi};
use alloc::vec::Vec;
use core::time::Duration;

use crux_core::App;

use crate::tests::{
    ExpectFullRefreshTimer, ExpectLight, ExpectRadioLimit, ExpectStep, asleep, drive_to_connected,
    drive_to_connecting, shell::Shell, start, tap_tile, wire::ExpectWrite,
};
use crate::{
    Connection, Control, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource,
    capabilities::power::{Wake, WakeSource},
    features::{
        connection::Link,
        light::BATTERY_LEVEL,
        power::{IDLE_TIMEOUT, SleepStep},
        wifi::{JOIN_TIMEOUT, OFF_TIMEOUT, Phase},
    },
};

#[test]
fn start_on_battery_also_starts_the_idle_timer() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(start(PowerSource::Battery), &mut model)
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_radio_join()
        .expect_radio_limit(JOIN_TIMEOUT)
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });
    assert_eq!(app.view(&model).power, PowerSource::Battery);
}

#[test]
fn the_idle_timeout_sleeps_in_steps_and_a_touch_wakes() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    model.last_sync = chrono::DateTime::from_timestamp(crate::tests::NOW.cast_signed(), 0);

    // Step 1: publish `asleep`, then disconnect. The core waits for the
    // answer to the close of the stream.
    app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    )
    .expect_step("sleep 1 closes the connection")
    .expect_disconnect(&mut wire, &mut model)
    .expect_session_clear(&wire)
    .expect_no_effects();
    let offline = wire.take_event();
    assert!(
        matches!(offline, Event::Power(power::Event::Offline(_))),
        "{offline:?}"
    );
    assert!(model.power.is_asleep());
    assert_eq!(app.view(&model).connection, Connection::Offline);

    // Step 2: turn the radio off, and wait for the answer.
    let radio_off = app
        .update(offline, &mut model)
        .expect_step("sleep 1 turns the radio off")
        .resolve_radio_off(|_| ())
        .expect_radio_limit(OFF_TIMEOUT)
        .expect_one_event();

    // Step 3: draw the last image, and sleep until a touch.
    let mut last = app.update(radio_off, &mut model);
    last.expect_step("sleep 1 draws the last image and sleeps");
    let effects: Vec<Effect> = last.effects().collect();
    // The render of the last image, and the sleep.
    assert_eq!(effects.len(), 2);
    assert!(matches!(effects[0], Effect::Render(_)));
    let mut request = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Sleep(request) => Some(request),
            _ => None,
        })
        .expect("a Sleep request");
    assert_eq!(request.operation.wake_on, [WakeSource::Touch]);
    // The touch is on no control, so it does not act on a control.
    request.resolve(Wake::Touch(None)).unwrap();
    let woke = last.expect_one_event();
    let view = app.view(&model);
    assert!(view.asleep);
    // The start asked for the first full refresh, and the sleep for the second.
    assert_eq!(view.full_refresh_seq, 2);
    assert_eq!(view.updated, "Last changed 09:41");
    assert_eq!(model.wifi.phase, Phase::Off);

    // The touch wakes the device: draw, join and start the idle timer. The
    // touch also turns the light on.
    app.update(woke, &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_radio_join_with(|join| assert!(!join.keep))
        .expect_radio_limit(JOIN_TIMEOUT)
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        })
        .expect_touch_light(BATTERY_LEVEL)
        .expect_no_effects();
    assert!(!model.power.is_asleep());
    assert_eq!(model.full_refresh_seq, 3);
}

#[test]
fn a_tap_during_the_steps_wakes_and_the_late_answers_are_ignored() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    // The write of the DISCONNECT packet waits.
    let mut sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );

    // The user taps before the disconnect ends. The session of the sleep
    // still closes, so the core does not connect yet.
    app.update(Event::Tapped(None), &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_time_notify_after()
        .expect_touch_light(BATTERY_LEVEL)
        .expect_no_effects();
    assert!(!model.power.is_asleep());

    // The DISCONNECT leaves and the stream closes. The wake forgot the
    // sleep, so the answer to the `Close` request does not continue the
    // sleep. The link is still up, so the core connects at once.
    sleep
        .expect_step("sleep 1 closes the connection")
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
    assert!(
        !wire
            .events
            .iter()
            .any(|event| matches!(event, Event::Power(power::Event::Offline(_)))),
        "{:?}",
        wire.events
    );
    assert!(!model.power.is_asleep());
    assert!(
        wire.effects
            .iter()
            .any(|effect| matches!(effect, Effect::StreamOpen(_))),
        "{:?}",
        wire.effects
    );
}

#[test]
fn the_answer_of_an_older_sleep_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let old = model.power.sleep.wrapping_add(1);
    let _ = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    assert_eq!(model.power.sleep, old);

    // The device moves to the stand and wakes. The shell then answers the
    // `Sleep` request.
    let _ = app.update(Event::PowerSourceChanged(PowerSource::Stand), &mut model);
    assert!(!model.power.is_asleep());
    for event in [
        Event::Power(power::Event::Offline(old)),
        Event::Wifi(wifi::Event::RadioOff(old)),
        Event::Power(power::Event::Woke {
            sleep: old,
            wake: Wake::PowerSource,
        }),
    ] {
        app.update(event, &mut model).expect_no_effects();
    }
    assert!(!model.power.is_asleep());
}

#[test]
fn a_tap_restarts_the_idle_timer_and_the_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let old = model.power.idle;

    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });

    app.update(Event::Power(power::Event::IdleTimeout(old)), &mut model)
        .expect_no_effects();
}

#[test]
fn on_the_stand_there_is_no_idle_timer() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    )
    .expect_no_effects();
}

#[test]
fn off_the_stand_starts_the_idle_timer() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_only_time_notify_after();
    assert_eq!(model.connection.link(), Link::Up);
}

#[test]
fn on_the_stand_while_asleep_wakes_and_connects() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    let mut sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    assert!(model.power.is_asleep());

    // The session of the sleep still closes, so the core does not connect
    // yet.
    app.update(Event::PowerSourceChanged(PowerSource::Stand), &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_no_effects();
    // The idle timeout only started the steps of the sleep, so the core
    // drew no last image. The wake asked for the second full refresh.
    assert_eq!(model.full_refresh_seq, 2);
    assert!(!model.power.is_asleep());

    // The DISCONNECT leaves and the session ends. The link is still up, so
    // the core connects at once.
    sleep
        .expect_step("sleep 1 closes the connection")
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
    assert!(
        wire.effects
            .iter()
            .any(|effect| matches!(effect, Effect::StreamOpen(_))),
        "{:?}",
        wire.effects
    );
}

#[test]
fn a_timer_wake_joins_and_starts_the_idle_timer() {
    let app = Dashboard;
    let mut model = asleep_on_battery();

    app.update(woke(&model, Wake::Timer), &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_radio_join()
        .expect_radio_limit(JOIN_TIMEOUT)
        .expect_only_time_notify_after();
    assert_eq!(model.full_refresh_seq, 1);
}

#[test]
fn a_power_source_wake_on_battery_is_ignored() {
    let app = Dashboard;
    let mut model = asleep_on_battery();

    // The shell answers `PowerSource` only after a real change. If the core
    // is still on battery, the answer does not wake it.
    app.update(woke(&model, Wake::PowerSource), &mut model)
        .expect_no_effects();
    assert!(model.power.is_asleep());
}

/// The answer to the `Sleep` request of the newest sleep.
const fn woke(model: &Model, wake: Wake) -> Event {
    Event::Power(power::Event::Woke {
        sleep: model.power.sleep,
        wake,
    })
}

/// Returns a model of a device on battery that sleeps, with settings.
fn asleep_on_battery() -> Model {
    Model {
        power: crate::features::power::State::asleep_in(0),
        startup: crate::Startup::Started(crate::tests::settings()),
        ..Model::default()
    }
}

#[test]
fn a_tap_on_no_control_while_asleep_only_wakes() {
    let app = Dashboard;
    let mut model = asleep_on_battery();
    let route = model.route.clone();

    // Draw, connect and start the idle timer. The touch also turns the
    // light on.
    app.update(woke(&model, Wake::Touch(None)), &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_radio_join()
        .expect_radio_limit(JOIN_TIMEOUT)
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        })
        .expect_touch_light(BATTERY_LEVEL)
        .expect_no_effects();
    assert!(!model.power.is_asleep());
    assert_eq!(model.route, route);
    assert_eq!(model.connection.outbox.len(), 0);
}

#[test]
fn a_touch_wake_on_a_room_button_opens_the_room() {
    let app = Dashboard;
    let mut model = asleep_on_battery();
    assert_eq!(model.route, crate::Route::Home);

    let _ = app.update(
        woke(&model, Wake::Touch(Some(Control::Room(0)))),
        &mut model,
    );
    assert!(!model.power.is_asleep());
    let first_room = model.rooms().next().unwrap().route();
    assert_eq!(model.route, first_room);
    // The redraw after the wake shows the room.
    assert_eq!(app.view(&model).path, first_room.to_path());
}

#[test]
fn a_touch_wake_on_a_light_queues_the_service_call() {
    let app = Dashboard;
    for read_only in [false, true] {
        let mut model = asleep_on_battery();
        if let crate::Startup::Started(settings) = &mut model.startup {
            settings.read_only = read_only;
        }
        model.route = crate::Route::room("living_room");

        let _ = app.update(
            woke(&model, Wake::Touch(Some(Control::Tile(0)))),
            &mut model,
        );
        assert!(!model.power.is_asleep());
        // The connection is not open yet, so the toggle waits in the
        // outbox. A dry run sends nothing.
        let expected = usize::from(!read_only);
        assert_eq!(model.connection.outbox.len(), expected, "{read_only}");
    }
}

#[test]
fn a_tap_on_no_control_while_awake_does_not_restart_the_idle_timer() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;

    // A touch by accident turns the light on, but it does not keep the
    // device awake.
    app.update(Event::Tapped(None), &mut model)
        .expect_touch_light(BATTERY_LEVEL)
        .expect_no_effects();
    assert_eq!(model.power.idle, idle);

    let _ = app.update(Event::Power(power::Event::IdleTimeout(idle)), &mut model);
    assert!(model.power.is_asleep());
}

#[test]
fn page_events_while_asleep_do_nothing() {
    let app = Dashboard;
    let mut model = asleep_on_battery();

    for event in [
        Event::NextPage,
        Event::PreviousPage,
        Event::Navigate(crate::Route::room("kitchen")),
    ] {
        app.update(event, &mut model).expect_no_effects();
    }
    assert!(model.power.is_asleep());
}

#[test]
fn a_battery_level_draws_once_and_the_same_level_draws_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;
    assert_eq!(app.view(&model).battery, None);

    // A partial render, with no timer.
    app.update(Event::BatteryChanged(Some(80)), &mut model)
        .expect_only_render();
    assert_eq!(app.view(&model).battery, Some(80));
    assert_eq!(model.power.idle, idle);

    app.update(Event::BatteryChanged(Some(80)), &mut model)
        .expect_no_effects();

    // The shell cannot read the charge now.
    app.update(Event::BatteryChanged(None), &mut model)
        .expect_only_render();
    assert_eq!(app.view(&model).battery, None);
}

#[test]
fn a_battery_level_while_asleep_does_not_wake_the_device() {
    let app = Dashboard;
    let mut model = asleep_on_battery();
    let idle = model.power.idle;

    // The core keeps the level, but it does not draw, join or start a
    // timer.
    app.update(Event::BatteryChanged(Some(41)), &mut model)
        .expect_no_effects();
    assert!(model.power.is_asleep());
    assert_eq!(model.power.idle, idle);
    assert_eq!(model.wifi.phase, Phase::Off);
    assert_eq!(app.view(&model).battery, Some(40));

    // The wake draws the level.
    let _ = app.update(woke(&model, Wake::Timer), &mut model);
    assert_eq!(app.view(&model).battery, Some(40));
    assert_eq!(model.full_refresh_seq, 1);
}

#[test]
fn the_status_text_does_not_give_the_power_source() {
    let app = Dashboard;
    for source in [PowerSource::Stand, PowerSource::Battery] {
        let (mut model, _wire) = drive_to_connected(source);
        let _ = app.update(Event::BatteryChanged(Some(55)), &mut model);
        let view = app.view(&model);
        assert_eq!(view.status, "Online", "{source:?}");
        assert_eq!(view.power, source);
        assert_eq!(view.battery, Some(55));
    }
}

#[test]
fn the_view_rounds_the_battery_level_to_the_nearest_step() {
    use crate::view::rounded_level;

    for (level, rounded) in [
        (0, 0),
        (2, 0),
        (3, 5),
        (12, 10),
        (13, 15),
        (77, 75),
        (78, 80),
        (98, 100),
        (100, 100),
        (200, 100),
    ] {
        assert_eq!(rounded_level(level), rounded, "{level}");
    }
}

#[test]
fn a_battery_change_that_does_not_change_the_rounded_level_does_not_draw() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    app.update(Event::BatteryChanged(Some(80)), &mut model)
        .expect_only_render();

    // 79, 78 and 81 all show as 80%. The core keeps the exact value.
    for level in [79, 78, 81] {
        app.update(Event::BatteryChanged(Some(level)), &mut model)
            .expect_no_effects();
        assert_eq!(model.power.battery, Some(level));
        assert_eq!(app.view(&model).battery, Some(80));
    }

    // 77 shows as 75%, so the core draws.
    app.update(Event::BatteryChanged(Some(77)), &mut model)
        .expect_only_render();
    assert_eq!(app.view(&model).battery, Some(75));
}

#[test]
fn sleep_now_on_battery_sleeps_at_once() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);

    // The first step of the sleep, as after the idle timeout.
    app.update(Event::SleepNow, &mut model)
        .expect_step("sleep 1 closes the connection")
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
    assert!(model.power.is_asleep());
}

#[test]
fn sleep_now_on_the_stand_or_while_asleep_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    app.update(Event::SleepNow, &mut model).expect_no_effects();
    assert!(!model.power.is_asleep());

    let mut model = asleep_on_battery();
    let sleep = model.power.sleep;
    app.update(Event::SleepNow, &mut model).expect_no_effects();
    assert!(model.power.is_asleep());
    assert_eq!(model.power.sleep, sleep);
}

#[test]
fn sleep_now_with_calls_in_the_outbox_first_waits_one_more_idle_period() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Battery);
    model.route = crate::Route::room("living_room");
    // The connection is not open, so the call waits in the outbox.
    let _ = app.update(tap_tile(0), &mut model);
    assert_eq!(model.connection.outbox.len(), 1);
    let old = model.power.idle;

    // The first `SleepNow` starts one more idle period.
    app.update(Event::SleepNow, &mut model)
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });
    assert!(!model.power.is_asleep());
    // The idle timer that ran before is now stale.
    app.update(Event::Power(power::Event::IdleTimeout(old)), &mut model)
        .expect_no_effects();
    assert!(!model.power.is_asleep());

    // The second `SleepNow` sleeps. The tap turned the light on, so the
    // first step turns it off. The next step drops the call.
    let mut sleep = app.update(Event::SleepNow, &mut model);
    let light_off = sleep
        .expect_step("sleep 1 turns the front light off")
        .resolve_front_light(|_| ())
        .expect_one_event();
    assert!(model.power.is_asleep());
    let _ = app.update(light_off, &mut model);
    assert_eq!(model.unsent, 1);
}

#[test]
fn an_idle_timer_that_expires_after_sleep_now_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;
    let _ = app.update(Event::SleepNow, &mut model);
    let sleep = model.power.sleep;

    // The core cleared the idle timer of the start. If its expiry still
    // arrives, the core ignores it.
    assert_ne!(model.power.idle, idle);
    app.update(Event::Power(power::Event::IdleTimeout(idle)), &mut model)
        .expect_no_effects();
    assert_eq!(model.power.sleep, sleep);
}

#[test]
fn sleep_now_before_start_does_nothing() {
    let app = Dashboard;
    let mut model = Model::default();
    // Before `Start`, the core has no settings. A shell can still report
    // the battery.
    let _ = app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model);

    app.update(Event::SleepNow, &mut model).expect_no_effects();
    assert!(!model.power.is_asleep());
}

/// Returns the number of idle timers that expired in `events`.
fn idle_timeouts(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Event::Power(power::Event::IdleTimeout(_))))
        .count()
}

#[test]
fn a_page_turn_clears_the_idle_timer_and_only_the_new_timer_fires() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    let half = IDLE_TIMEOUT / 2;
    shell.advance(half);
    shell.take_log();

    shell.send(Event::NextPage);
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");

    // The idle timer of the start does not fire.
    assert_eq!(idle_timeouts(&shell.advance(half)), 0);
    assert!(shell.sleep.is_none());
    // The new timer fires, and the device sleeps.
    assert_eq!(idle_timeouts(&shell.advance(half)), 1);
    assert!(shell.sleep.is_some());
}

#[test]
fn sleep_now_clears_the_idle_timer() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    shell.take_log();

    shell.send(Event::SleepNow);
    let log = shell.take_log();
    assert_eq!(
        log.first().map(String::as_str),
        Some("TimeClear"),
        "{log:?}"
    );
    assert!(shell.sleep.is_some());

    assert_eq!(idle_timeouts(&shell.advance(IDLE_TIMEOUT)), 0);
}

/// Returns the number of idle timers that the core cleared while they
/// waited. Refer to [`Shell::take_cleared`]. The queued timer of the
/// pending feature has the same duration, so a test that uses this function
/// must not queue a service call.
fn idle_clears(shell: &mut Shell) -> usize {
    shell
        .take_cleared()
        .into_iter()
        .filter(|duration| *duration == IDLE_TIMEOUT)
        .count()
}

#[test]
fn sleep_now_before_start_clears_the_idle_timer_once() {
    let mut shell = Shell::new();
    // Before `Start`, a move to battery starts an idle timer.
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    shell.take_log();
    shell.take_cleared();

    // `SleepNow` does not sleep before `Start`, but it clears the timer.
    shell.send(Event::SleepNow);
    let log = shell.take_log();
    assert_eq!(log, ["TimeClear"], "{log:?}");
    assert_eq!(idle_clears(&mut shell), 1);
    assert_eq!(idle_timeouts(&shell.advance(IDLE_TIMEOUT)), 0);
}

#[test]
fn sleep_now_on_battery_clears_the_idle_timer_once() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    shell.take_cleared();

    // The sleep and `SleepNow` both stop the idle timer, but the shell gets
    // only one `TimeClear` for it.
    shell.send(Event::SleepNow);
    assert_eq!(idle_clears(&mut shell), 1);
    assert!(shell.sleep.is_some());
}

#[test]
fn a_second_start_on_battery_clears_the_old_idle_timer_once_and_starts_one() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    let half = IDLE_TIMEOUT / 2;
    shell.advance(half);
    shell.take_cleared();
    shell.take_log();

    shell.send(start(PowerSource::Battery));
    assert_eq!(idle_clears(&mut shell), 1);
    let log = shell.take_log();
    let idle = alloc::format!("TimeNotifyAfter {}ms", IDLE_TIMEOUT.as_millis());
    assert_eq!(
        log.iter().filter(|entry| **entry == idle).count(),
        1,
        "{log:?}"
    );

    // The idle timer of the first start does not fire. Only the new timer
    // fires, and the device sleeps.
    assert_eq!(idle_timeouts(&shell.advance(half)), 0);
    assert!(shell.sleep.is_none());
    assert_eq!(idle_timeouts(&shell.advance(half)), 1);
    assert!(shell.sleep.is_some());
}

#[test]
fn an_old_light_off_answer_during_the_next_sleep_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    // The tap turns the light on, so the sleep first turns it off.
    let _ = app.update(Event::Tapped(None), &mut model);
    let mut first = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let mut first_light = first
        .expect_step("sleep 1 turns the front light off")
        .expect_one_effect()
        .expect_front_light();

    // A tap wakes the device and turns the light on again. The next sleep
    // also waits for the light.
    let _ = app.update(Event::Tapped(None), &mut model);
    assert!(!asleep(&model));
    let mut second = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let _second_light = second
        .expect_step("sleep 2 turns the front light off")
        .expect_one_effect()
        .expect_front_light();
    assert!(asleep(&model));

    // The answer of the first sleep arrives late. It does not start the
    // next step of the second sleep.
    first_light.resolve(()).unwrap();
    let old = first.expect_one_event();
    assert!(
        matches!(old, Event::Power(power::Event::LightOffForSleep(_))),
        "{old:?}"
    );
    app.update(old, &mut model).expect_no_effect_or_events();
    assert!(asleep(&model));
}

#[test]
fn an_old_woke_answer_during_the_next_sleep_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(Event::SleepNow, &mut model);
    let first = model.power.sleep;
    let _ = app.update(Event::Tapped(None), &mut model);
    let _ = app.update(Event::SleepNow, &mut model);
    assert!(asleep(&model));

    // The answer to the `Sleep` request of the first sleep does not wake
    // the device from the second sleep.
    app.update(
        Event::Power(power::Event::Woke {
            sleep: first,
            wake: Wake::Touch(None),
        }),
        &mut model,
    )
    .expect_no_effect_or_events();
    assert!(asleep(&model));
}

#[test]
fn start_while_asleep_wakes_and_the_old_step_answers_do_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let _sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let old = model.power.sleep;
    assert!(asleep(&model));

    let _ = app.update(start(PowerSource::Battery), &mut model);
    assert!(!asleep(&model));
    for event in [
        Event::Power(power::Event::LightOffForSleep(old)),
        Event::Power(power::Event::Offline(old)),
        Event::Power(power::Event::Woke {
            sleep: old,
            wake: Wake::Timer,
        }),
    ] {
        app.update(event, &mut model).expect_no_effect_or_events();
    }
    assert!(!asleep(&model));
}

#[test]
fn a_touch_after_the_extension_lets_the_next_idle_end_extend_again() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Battery);
    model.route = crate::Route::room("living_room");
    // The connection is not open, so the call waits in the outbox.
    let _ = app.update(tap_tile(0), &mut model);

    // The first end of the idle time extends it once.
    app.update(Event::SleepNow, &mut model)
        .expect_only_time_notify_after();
    assert!(!asleep(&model));

    // A touch starts a new idle time. Its end extends again, because the
    // call still waits.
    let _ = app.update(Event::NextPage, &mut model);
    app.update(Event::SleepNow, &mut model)
        .expect_only_time_notify_after();
    assert!(!asleep(&model));

    // The next end sleeps.
    let _ = app.update(Event::SleepNow, &mut model);
    assert!(asleep(&model));
}

/// Drives the core on battery to a sleep that follows an older sleep. The
/// first sleep is at the disconnect step when a tap wakes the device. The
/// tap turns the light on, so the second sleep waits for the light. Returns
/// the model and the id of the first sleep.
fn second_sleep() -> (Model, u32) {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(Event::SleepNow, &mut model);
    let first = model.power.sleep;
    let _ = app.update(Event::Tapped(None), &mut model);
    let _ = app.update(Event::SleepNow, &mut model);
    assert!(asleep(&model));
    assert_ne!(model.power.sleep, first);
    (model, first)
}

#[test]
fn offline_ignores_the_id_of_an_older_sleep() {
    let (mut model, first) = second_sleep();
    // The radio is still on. The answer of the first sleep does not turn
    // it off.
    power::offline(first, &mut model).expect_no_effect_or_events();
    assert!(asleep(&model));
}

#[test]
fn radio_off_ignores_the_id_of_an_older_sleep() {
    let (mut model, first) = second_sleep();
    // The answer of the first sleep does not draw the last image or sleep.
    power::radio_off(first, &mut model).expect_no_effect_or_events();
    assert!(asleep(&model));
}

#[test]
fn woke_while_awake_on_battery_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(Event::SleepNow, &mut model);
    let sleep = model.power.sleep;
    let _ = app.update(Event::Tapped(None), &mut model);
    assert!(!asleep(&model));

    // The id is the id of the newest sleep, but a tap woke the device
    // first. The answer does not wake the device again or start a new idle
    // timer.
    power::woke(sleep, Wake::Timer, &mut model).expect_no_effect_or_events();
    assert!(!asleep(&model));
}

#[test]
fn the_sleep_step_follows_each_step_of_a_sleep_and_each_step_writes_one_line() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    assert_eq!(power::sleep_step(&model), None);
    // The tap turns the light on, so the sleep first turns it off.
    let _ = app.update(Event::Tapped(None), &mut model);

    // Step 1: the `FrontLight` request waits.
    let mut sleep = app.update(Event::SleepNow, &mut model);
    let light_off = sleep
        .expect_step("sleep 1 turns the front light off")
        .resolve_front_light(|_| ())
        .expect_one_event();
    assert_eq!(power::sleep_step(&model), Some(SleepStep::LightOff));

    // Step 2: the close of the stream waits.
    let mut disconnect = app.update(light_off, &mut model);
    disconnect.expect_step("sleep 1 closes the connection");
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Disconnect));
    disconnect
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
    let offline = wire.take_event();

    // Step 3: the `Off` request waits.
    let mut off = app.update(offline, &mut model);
    off.expect_step("sleep 1 turns the radio off");
    assert_eq!(power::sleep_step(&model), Some(SleepStep::RadioOff));
    let radio_off = off
        .resolve_radio_off(|_| ())
        .expect_radio_limit(OFF_TIMEOUT)
        .expect_one_event();

    // Step 4: the `Sleep` request waits.
    let mut last = app.update(radio_off, &mut model);
    last.expect_step("sleep 1 draws the last image and sleeps")
        .expect_render();
    let woke = last.resolve_sleep(|_| Wake::Touch(None)).expect_one_event();
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Sleep));

    // The wake ends the sleep, and writes no line.
    let mut awake = app.update(woke, &mut model);
    assert!(
        !awake
            .effects()
            .any(|effect| matches!(effect, Effect::Log(_))),
        "a line of the log"
    );
    assert_eq!(power::sleep_step(&model), None);
}
