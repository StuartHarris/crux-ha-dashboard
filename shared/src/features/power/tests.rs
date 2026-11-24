//! Tests of the power feature: the idle timer, sleep and the power source.

use crate::features::{power, wifi};
use alloc::vec::Vec;
use core::time::Duration;

use crux_core::App;

use crate::tests::{
    ExpectFullRefreshTimer, ExpectLight, ExpectRadioLimit, ExpectStep, asleep, drive_to_connected,
    drive_to_connecting, shell::Shell, start, state, tap_tile, wire::ExpectWrite,
};
use crate::{
    Charge, Connection, Control, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource,
    Reading,
    capabilities::{
        log::Level,
        power::{Wake, WakeSource},
    },
    features::{
        connection::Link,
        light::DEFAULT_BATTERY_LEVEL,
        power::{
            CHECK_PERIOD, IDLE_TIMEOUT, REPORT_CHECKS, REPORT_LIMIT, ReportPeriod, STRAY_HOLD,
            SleepStep,
        },
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
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        })
        .expect_only_log_with(|log| {
            assert_eq!(log.to_string(), "power: started after a power-on");
        });
    assert_eq!(app.view(&model).power, PowerSource::Battery);
}

#[test]
fn the_idle_timeout_sleeps_in_steps_and_a_touch_wakes() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    model.last_sync = jiff::Timestamp::from_second(crate::tests::NOW.cast_signed()).ok();

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
    assert_eq!(request.operation.wake_on, wake_sources());
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
        .expect_touch_light(DEFAULT_BATTERY_LEVEL.get())
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
        .expect_touch_light(DEFAULT_BATTERY_LEVEL.get())
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    // The core also publishes the new power source.
    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_time_notify_after()
        .expect_state(&mut wire, &mut model, &state("null", "OFF"))
        .expect_only_session_clear(&wire);
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

/// The wake sources of each `Sleep` request: a touch, and the check of the
/// power source.
fn wake_sources() -> Vec<WakeSource> {
    alloc::vec![
        WakeSource::Touch,
        WakeSource::Timer {
            seconds: u32::try_from(CHECK_PERIOD.as_secs()).unwrap(),
        },
    ]
}

#[test]
fn a_timer_wake_is_a_check_that_only_sleeps_again() {
    let app = Dashboard;
    let mut model = asleep_on_battery();

    // No `Render`, no line and no radio: only a new `Sleep` request.
    app.update(woke(&model, Wake::Timer), &mut model)
        .expect_only_sleep_with(|sleep| assert_eq!(sleep.wake_on, wake_sources()));
    assert!(model.power.is_asleep());
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Sleep));
    assert_eq!(model.full_refresh_seq, 0);
    assert_eq!(model.wifi.phase, Phase::Off);
}

#[test]
fn a_power_source_wake_on_battery_sleeps_again_with_a_warning() {
    let app = Dashboard;
    let mut model = asleep_on_battery();

    // The shell answers `PowerSource` only after a real change, which comes
    // first as `PowerSourceChanged`. If the core is still on battery, it
    // writes a warning and sleeps again.
    app.update(woke(&model, Wake::PowerSource), &mut model)
        .expect_log_with(|log| {
            assert_eq!(log.level, Level::Warn);
            assert_eq!(
                log.to_string(),
                "power: sleep 0 got a power source wake on battery, so it sleeps again"
            );
        })
        .expect_only_sleep_with(|sleep| assert_eq!(sleep.wake_on, wake_sources()));
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Sleep));
}

#[test]
fn a_report_period_writes_one_line_and_the_same_period_again_does_nothing() {
    let app = Dashboard;
    let mut model = asleep_on_battery();
    let period = ReportPeriod::of_minutes(10).unwrap();

    app.update(
        Event::Power(power::Event::ReportPeriodSet(period)),
        &mut model,
    )
    .expect_only_log_with(|log| {
        assert_eq!(log.level, Level::Debug);
        assert_eq!(
            log.to_string(),
            "device: Home Assistant set the report period to 10 min"
        );
    });
    assert_eq!(model.power.report_period(), period);
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Sleep));

    app.update(
        Event::Power(power::Event::ReportPeriodSet(period)),
        &mut model,
    )
    .expect_done();
}

#[test]
fn a_check_at_the_period_reports_and_at_the_period_0_only_sleeps_again() {
    let app = Dashboard;

    // At the shortest period, the 5th check starts the report.
    let mut model = asleep_on_battery();
    let _ = app.update(
        Event::Power(power::Event::ReportPeriodSet(
            ReportPeriod::of_minutes(5).unwrap(),
        )),
        &mut model,
    );
    for _ in 0..4 {
        app.update(woke(&model, Wake::Timer), &mut model)
            .expect_only_sleep_with(|sleep| assert_eq!(sleep.wake_on, wake_sources()));
    }
    app.update(woke(&model, Wake::Timer), &mut model)
        .expect_log_with(|log| {
            assert_eq!(log.to_string(), "power: sleep 0 reports to Home Assistant");
        })
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), REPORT_LIMIT);
        });
    assert!(model.power.is_reporting());

    // At the period 0, each check only sleeps again.
    let mut model = asleep_on_battery();
    let _ = app.update(
        Event::Power(power::Event::ReportPeriodSet(ReportPeriod::Off)),
        &mut model,
    );
    for _ in 0..u32::from(REPORT_CHECKS) * 3 {
        app.update(woke(&model, Wake::Timer), &mut model)
            .expect_only_sleep_with(|sleep| assert_eq!(sleep.wake_on, wake_sources()));
    }
    assert!(!model.power.is_reporting());
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
        power: crate::features::power::State::sleeping_in(0),
        startup: crate::Startup::Started {
            settings: crate::tests::settings(),
            firmware: crate::tests::firmware(),
            boot: crate::tests::POWER_ON,
        },
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
        .expect_touch_light(DEFAULT_BATTERY_LEVEL.get())
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
        if let crate::Startup::Started { settings, .. } = &mut model.startup {
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;

    // A touch by accident turns the light on, but it does not keep the
    // device awake.
    app.update(Event::Tapped(None), &mut model)
        .expect_touch_light(DEFAULT_BATTERY_LEVEL.get())
        .expect_light_publish(&mut wire, &mut model, DEFAULT_BATTERY_LEVEL.get())
        .expect_only_session_clear(&wire);
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;
    assert_eq!(app.view(&model).battery, None);

    // A partial render, with no timer. The core also publishes the level.
    app.update(Event::BatteryChanged(Some(80)), &mut model)
        .expect_render()
        .expect_state(&mut wire, &mut model, &state("80", "OFF"))
        .expect_only_session_clear(&wire);
    assert_eq!(app.view(&model).battery, Some(80));
    assert_eq!(model.power.idle, idle);

    // The same level does nothing: no draw, no publish.
    let packets = wire.packets.len();
    app.update(Event::BatteryChanged(Some(80)), &mut model)
        .expect_done();
    assert_eq!(wire.packets.len(), packets);

    // The shell cannot read the charge now.
    app.update(Event::BatteryChanged(None), &mut model)
        .expect_render()
        .expect_state(&mut wire, &mut model, &state("null", "OFF"))
        .expect_only_session_clear(&wire);
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
        .expect_done();
    assert!(model.power.is_asleep());
    assert_eq!(model.power.battery, Some(41));
    assert_eq!(model.power.idle, idle);
    assert_eq!(model.wifi.phase, Phase::Off);
    assert_eq!(app.view(&model).battery, Some(40));

    // The wake draws the level.
    let _ = app.update(woke(&model, Wake::Touch(None)), &mut model);
    assert_eq!(app.view(&model).battery, Some(40));
    assert_eq!(model.full_refresh_seq, 1);
}

#[test]
fn a_power_reading_while_asleep_is_kept_and_does_nothing_else() {
    let app = Dashboard;
    let mut model = asleep_on_battery();
    let before = model.power.idle;
    let reading = Reading {
        charge: Some(Charge::NotCharging),
        voltage_mv: Some(3870),
        current_ma: Some(-40),
        tenth_celsius: Some(245),
    };

    // The check of the power source sends the reading, and then the timer
    // wake. The core only keeps the reading, and it publishes nothing.
    app.update(Event::PowerRead(reading), &mut model)
        .expect_done();
    assert_eq!(model.power.reading, reading);
    assert!(model.power.is_asleep());
    assert_eq!(model.power.idle, before);
    assert_eq!(model.wifi.phase, Phase::Off);
    assert_eq!(model.full_refresh_seq, 0);
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    app.update(Event::BatteryChanged(Some(80)), &mut model)
        .expect_render()
        .expect_state(&mut wire, &mut model, &state("80", "OFF"))
        .expect_only_session_clear(&wire);

    // 78, 81 and 79 all show as 80%. The core keeps the exact value, and
    // publishes it, but does not draw. Each one moves by more than 1%, so
    // the state message goes at once. Refer to `device::BATTERY_FLAP`.
    for level in [78, 81, 79] {
        let json = state(&level.to_string(), "OFF");
        app.update(Event::BatteryChanged(Some(level)), &mut model)
            .expect_state(&mut wire, &mut model, &json)
            .expect_only_session_clear(&wire);
        assert_eq!(model.power.battery, Some(level));
        assert_eq!(app.view(&model).battery, Some(80));
    }

    // 77 shows as 75%, so the core draws.
    app.update(Event::BatteryChanged(Some(77)), &mut model)
        .expect_render()
        .expect_state(&mut wire, &mut model, &state("77", "OFF"))
        .expect_only_session_clear(&wire);
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

    // The core cleared the idle timer of the start
    // (`sleep_now_clears_the_idle_timer`). If its expiry still arrives, the
    // core ignores it.
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
fn a_power_reading_is_kept_and_does_not_draw_or_keep_the_device_awake() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    let half = IDLE_TIMEOUT / 2;
    shell.advance(half);
    shell.take_log();
    let frames = shell.frames.len();

    let reading = Reading {
        charge: Some(Charge::NotCharging),
        voltage_mv: Some(3870),
        current_ma: Some(-40),
        tenth_celsius: Some(245),
    };
    shell.send(Event::PowerRead(reading));

    assert_eq!(shell.model.power.reading, reading);
    // The core publishes the state message, but it does not draw, and it
    // does not start the idle timer again.
    let log = shell.take_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        log[0].starts_with("MqttPublish dashboard/hall/state "),
        "{log:?}"
    );
    assert_eq!(shell.frames.len(), frames);
    // The idle timer of the start still ends the idle time.
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
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);

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

/// Starts the house on battery, sleeps at the idle timeout, and clears the
/// logs. The `Sleep` request waits.
fn at_the_sleep_step() -> Shell {
    let mut shell = crate::tests::flows::house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    shell.take_log();
    shell.take_logs();
    shell
}

/// Returns the lines of `logs` with the area `power`. Each line is
/// `{level} {area}: {message}`.
fn power_lines(logs: &[alloc::string::String]) -> Vec<&str> {
    logs.iter()
        .map(alloc::string::String::as_str)
        .filter(|line| {
            line.split_once(' ')
                .is_some_and(|(_, rest)| rest.starts_with("power:"))
        })
        .collect()
}

#[test]
fn a_tap_at_the_sleep_step_wakes_connects_and_acts_on_its_control() {
    let mut shell = at_the_sleep_step();

    // The tap comes to the core as `Tapped`, not as the answer to `Sleep`.
    // It wakes the device, connects, starts the idle timer and acts on its
    // control.
    shell.queue(crate::tests::tap_all_lights());
    shell.run();
    let log = shell.take_log();
    assert!(!asleep(&shell.model));
    assert_eq!(power::sleep_step(&shell.model), None);
    assert!(
        log.iter().any(|entry| entry.starts_with("RadioJoin")),
        "{log:?}"
    );
    let idle = alloc::format!("TimeNotifyAfter {}ms", IDLE_TIMEOUT.as_millis());
    assert!(log.contains(&idle), "{log:?}");
    assert_eq!(shell.view().connection, Connection::Online);
    let call = shell.broker.service_calls.last().expect("a service call");
    assert_eq!(call.entity_id, "light.all_lights");
    assert_eq!(shell.broker.service_calls.len(), 1);
}

#[test]
fn a_touch_wake_writes_no_power_line() {
    let mut shell = at_the_sleep_step();

    // The shell answers `Sleep` with `Wake::Touch(None)`. A usual wake
    // writes no line with the area `power`.
    shell.send(Event::Tapped(None));
    assert!(!asleep(&shell.model));
    let logs = shell.take_logs();
    assert_eq!(power_lines(&logs), Vec::<&str>::new(), "{logs:?}");
}

#[test]
fn a_battery_level_at_the_sleep_step_sends_nothing() {
    let mut shell = at_the_sleep_step();

    // The core keeps the level, but it sends no effect, and the `Sleep`
    // request still waits.
    shell.send(Event::BatteryChanged(Some(41)));
    assert_eq!(shell.take_log(), Vec::<alloc::string::String>::new());
    assert_eq!(shell.take_logs(), Vec::<alloc::string::String>::new());
    assert!(shell.sleep.is_some());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_eq!(shell.view().battery, Some(40));
}

/// The line of the first wake with no cause in the sleep `id`.
fn stray_line(id: u32) -> alloc::string::String {
    alloc::format!("info power: sleep {id} woke with no cause, so it sleeps again after 3 s")
}

/// Returns the `TimeNotifyAfter` entry of the log for the hold timer.
fn hold_timer() -> alloc::string::String {
    alloc::format!("TimeNotifyAfter {}ms", STRAY_HOLD.as_millis())
}

#[test]
fn the_first_sleep_has_the_check_timer_and_comes_after_the_last_image() {
    let mut shell = crate::tests::flows::house();
    shell.send(start(PowerSource::Battery));
    shell.take_log();
    shell.advance(IDLE_TIMEOUT);
    let log = shell.take_log();
    let render = log.iter().rposition(|entry| entry == "Render").unwrap();
    let sleep = log.iter().position(|entry| entry == "Sleep").unwrap();
    assert!(render < sleep, "{log:?}");
    assert_eq!(shell.sleep.as_ref().unwrap().wake_on, wake_sources());
    assert!(shell.frames.last().unwrap().full);
}

#[test]
fn fifty_nine_checks_give_fifty_nine_sleeps_and_then_a_touch_wakes() {
    let mut shell = at_the_sleep_step();
    let frames = shell.frames.len();

    // The 60th check would report.
    for _ in 0..59 {
        shell.check();
        assert_eq!(shell.take_log(), ["Wake Timer", "Sleep"]);
    }
    assert_eq!(shell.take_logs(), Vec::<alloc::string::String>::new());
    assert_eq!(shell.frames.len(), frames);
    assert!(!shell.radio.on);

    shell.send(Event::Tapped(None));
    assert!(!asleep(&shell.model));
    assert_eq!(shell.view().connection, Connection::Online);
}

/// Answers `count` checks of the power source. Each one must only send a
/// new `Sleep` request with the touch wake and the check timer: no `Render`,
/// no radio request and no line.
#[track_caller]
fn checks_that_only_sleep(shell: &mut Shell, count: usize) {
    for check in 0..count {
        shell.check();
        assert_eq!(shell.take_log(), ["Wake Timer", "Sleep"], "check {check}");
        assert_eq!(shell.sleep.as_ref().unwrap().wake_on, wake_sources());
    }
}

#[test]
fn fifty_nine_checks_only_sleep_again() {
    let mut shell = at_the_sleep_step();
    let frames = shell.frames.len();

    checks_that_only_sleep(&mut shell, 59);
    assert_eq!(shell.take_logs(), Vec::<alloc::string::String>::new());
    assert_eq!(shell.frames.len(), frames);
    assert!(!shell.radio.on);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_no_timer_at_sleep(&shell);
}

#[test]
fn a_stray_wake_keeps_the_check_count() {
    let mut shell = at_the_sleep_step();

    checks_that_only_sleep(&mut shell, 30);
    shell.stray();
    shell.advance(STRAY_HOLD);
    assert_eq!(
        shell.take_log(),
        ["Wake Other".into(), hold_timer(), "Sleep".into()]
    );
    checks_that_only_sleep(&mut shell, 29);
    assert!(!shell.radio.on);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_no_timer_at_sleep(&shell);

    // The stray kept the count, so the 60th check reports.
    expect_a_report(&mut shell, 1);
}

#[test]
fn a_power_source_wake_on_battery_does_not_count() {
    let mut shell = at_the_sleep_step();

    checks_that_only_sleep(&mut shell, 59);
    shell.wake(Wake::PowerSource);
    assert_eq!(shell.take_log(), ["Wake PowerSource", "Sleep"]);
    assert!(!shell.radio.on);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_no_timer_at_sleep(&shell);

    // The power source wake did not count, so the next check is the 60th.
    expect_a_report(&mut shell, 1);
}

/// Answers the next check, which must start the report of the sleep `id`.
/// The report runs to the next `Sleep` request.
#[track_caller]
fn expect_a_report(shell: &mut Shell, id: u32) {
    shell.take_logs();
    shell.check();
    let log = shell.take_log();
    assert!(
        log.contains(&"MqttPublish dashboard/hall/status reporting".into()),
        "{log:?}"
    );
    assert_eq!(log.last().map(alloc::string::String::as_str), Some("Sleep"));
    assert_eq!(
        shell.take_logs().first().map(alloc::string::String::as_str),
        Some(alloc::format!("debug power: sleep {id} reports to Home Assistant").as_str())
    );
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_no_timer_at_sleep(shell);
}

#[test]
fn the_sixtieth_check_reports() {
    let mut shell = at_the_sleep_step();
    checks_that_only_sleep(&mut shell, usize::from(REPORT_CHECKS) - 1);
    shell.hold_radio = true;

    // No `Sleep`, a join, the limit timer and one Debug line.
    shell.check();
    assert_eq!(
        shell.take_log(),
        [
            "Wake Timer".into(),
            alloc::format!("TimeNotifyAfter {}ms", REPORT_LIMIT.as_millis()),
            "RadioJoin Cached".into(),
            alloc::format!("TimeNotifyAfter {}ms", JOIN_TIMEOUT.as_millis()),
        ]
    );
    assert_eq!(
        shell.take_logs(),
        ["debug power: sleep 1 reports to Home Assistant"]
    );
    assert!(shell.sleep.is_none());
    assert!(shell.model.power.is_reporting());
    assert!(asleep(&shell.model));
    assert_eq!(power::sleep_step(&shell.model), None);
    // The device does not draw in a report, so the view still shows that
    // it is asleep.
    assert_eq!(shell.view().status, "Asleep");

    shell.hold_radio = false;
    shell.answer_radio();
    assert!(!shell.model.power.is_reporting());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_no_timer_at_sleep(&shell);
}

#[test]
fn a_touch_wake_starts_the_count_again() {
    let mut shell = at_the_sleep_step();
    checks_that_only_sleep(&mut shell, usize::from(REPORT_CHECKS) - 1);

    shell.send(Event::Tapped(None));
    assert!(!asleep(&shell.model));
    shell.advance(IDLE_TIMEOUT);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    shell.take_log();

    checks_that_only_sleep(&mut shell, usize::from(REPORT_CHECKS) - 1);
    expect_a_report(&mut shell, 2);
}

#[test]
fn the_next_report_is_sixty_checks_after_the_last() {
    let mut shell = at_the_sleep_step();
    checks_that_only_sleep(&mut shell, usize::from(REPORT_CHECKS) - 1);
    expect_a_report(&mut shell, 1);

    checks_that_only_sleep(&mut shell, usize::from(REPORT_CHECKS) - 1);
    expect_a_report(&mut shell, 2);
}

#[test]
fn a_late_reported_or_report_limit_does_nothing() {
    let late = |shell: &mut Shell| {
        for id in 0..=shell.model.power.sleep {
            shell.send(Event::Power(power::Event::Reported(id)));
            shell.send(Event::Power(power::Event::ReportLimit(id)));
        }
        assert_eq!(shell.take_log(), Vec::<alloc::string::String>::new());
        assert_eq!(shell.take_logs(), Vec::<alloc::string::String>::new());
    };
    let mut shell = at_the_sleep_step();
    checks_that_only_sleep(&mut shell, usize::from(REPORT_CHECKS) - 1);
    expect_a_report(&mut shell, 1);
    shell.take_logs();

    // Sleeping, after the report.
    late(&mut shell);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));

    // Held, after a wake with no cause.
    shell.stray();
    shell.take_log();
    shell.take_logs();
    late(&mut shell);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Held));

    // Awake.
    shell.send(Event::Tapped(None));
    shell.take_log();
    shell.take_logs();
    late(&mut shell);
    assert!(!asleep(&shell.model));
}

#[test]
fn a_stray_wake_holds_the_device_awake_then_sleeps_again() {
    let mut shell = at_the_sleep_step();
    let frames = shell.frames.len();

    // The device stays awake with the radio off, and writes one line.
    shell.stray();
    assert_eq!(shell.take_log(), ["Wake Other".into(), hold_timer()]);
    assert_eq!(shell.take_logs(), [stray_line(1)]);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Held));
    assert!(asleep(&shell.model));
    assert!(shell.sleep.is_none());

    // At the end of the hold, a new `Sleep` request with no `Render`.
    shell.advance(STRAY_HOLD.saturating_sub(Duration::from_millis(1)));
    assert!(shell.sleep.is_none());
    shell.advance(Duration::from_millis(1));
    assert_eq!(shell.take_log(), ["Sleep"]);
    assert_eq!(shell.take_logs(), Vec::<alloc::string::String>::new());
    assert_eq!(shell.sleep.as_ref().unwrap().wake_on, wake_sources());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_eq!(shell.frames.len(), frames);
    assert!(!shell.radio.on);
}

#[test]
fn a_second_stray_wake_writes_no_line_and_the_wake_writes_the_count() {
    let mut shell = at_the_sleep_step();
    shell.stray();
    shell.advance(STRAY_HOLD);
    assert_eq!(shell.take_logs(), [stray_line(1)]);

    shell.stray();
    shell.advance(STRAY_HOLD);
    shell.check();
    assert_eq!(shell.take_logs(), Vec::<alloc::string::String>::new());

    // The touch wake writes the count of the wakes with no cause.
    shell.send(Event::Tapped(None));
    assert!(!asleep(&shell.model));
    let logs = shell.take_logs();
    assert_eq!(
        power_lines(&logs),
        ["info power: sleep 1 had 2 wakes with no cause"],
        "{logs:?}"
    );
}

#[test]
fn one_stray_wake_writes_no_count_at_the_wake() {
    let mut shell = at_the_sleep_step();
    shell.stray();
    shell.advance(STRAY_HOLD);
    shell.take_logs();

    shell.send(Event::Tapped(None));
    let logs = shell.take_logs();
    assert_eq!(power_lines(&logs), Vec::<&str>::new(), "{logs:?}");
}

#[test]
fn a_tap_in_the_hold_clears_the_hold_timer_wakes_and_acts_on_its_control() {
    let mut shell = at_the_sleep_step();
    shell.stray();
    shell.take_log();
    shell.take_cleared();

    // No `Sleep` request waits, so the tap comes to the core as `Tapped`.
    shell.send(crate::tests::tap_all_lights());
    let log = shell.take_log();
    assert_eq!(
        log.first().map(String::as_str),
        Some("TimeClear"),
        "{log:?}"
    );
    assert_eq!(shell.take_cleared().first(), Some(&STRAY_HOLD));
    assert!(!asleep(&shell.model));
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(shell.broker.service_calls.len(), 1);

    // The hold timer does not fire, so no `Sleep` request comes.
    let events = shell.advance(STRAY_HOLD);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Power(power::Event::HoldEnded(_)))),
        "{events:?}"
    );
    assert!(shell.sleep.is_none());
}

#[test]
fn the_stand_in_the_hold_clears_the_hold_timer_and_connects() {
    let mut shell = at_the_sleep_step();
    shell.stray();
    shell.take_cleared();

    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert_eq!(shell.take_cleared().first(), Some(&STRAY_HOLD));
    assert_eq!(shell.view().power, PowerSource::Stand);
    assert!(!asleep(&shell.model));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.advance(STRAY_HOLD);
    assert!(shell.sleep.is_none());
}

#[test]
fn start_in_the_hold_clears_the_hold_timer() {
    let mut shell = at_the_sleep_step();
    shell.stray();
    shell.take_cleared();

    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.take_cleared().first(), Some(&STRAY_HOLD));
    assert!(!asleep(&shell.model));
    shell.advance(STRAY_HOLD);
    assert!(shell.sleep.is_none());
}

#[test]
fn a_late_hold_end_does_nothing() {
    let app = Dashboard;
    // The device woke from the hold, so the expiry of its timer is late.
    let mut shell = at_the_sleep_step();
    let first = shell.model.power.sleep;
    shell.stray();
    shell.send(Event::Tapped(None));
    let mut model = shell.model;
    app.update(Event::Power(power::Event::HoldEnded(first)), &mut model)
        .expect_no_effect_or_events();
    assert!(!asleep(&model));

    // The expiry of a timer of an older sleep does not end the hold of the
    // new sleep.
    let mut shell = at_the_sleep_step();
    shell.stray();
    let current = shell.model.power.sleep;
    let mut model = shell.model;
    app.update(
        Event::Power(power::Event::HoldEnded(current.wrapping_sub(1))),
        &mut model,
    )
    .expect_no_effect_or_events();
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Held));

    // While a `Sleep` request waits, an expiry does nothing.
    let mut model = asleep_on_battery();
    app.update(
        Event::Power(power::Event::HoldEnded(model.power.sleep)),
        &mut model,
    )
    .expect_no_effect_or_events();
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Sleep));
}

#[test]
fn a_check_or_a_stray_wake_of_an_older_sleep_does_nothing() {
    let app = Dashboard;
    let mut model = asleep_on_battery();
    let old = model.power.sleep.wrapping_sub(1);
    for wake in [Wake::Timer, Wake::Other, Wake::PowerSource] {
        app.update(
            Event::Power(power::Event::Woke { sleep: old, wake }),
            &mut model,
        )
        .expect_no_effect_or_events();
        assert_eq!(power::sleep_step(&model), Some(SleepStep::Sleep));
    }
}

#[test]
fn an_answer_in_the_steps_or_in_the_hold_does_nothing() {
    let app = Dashboard;
    // In the steps, no `Sleep` request waits.
    let mut model = Model {
        power: crate::features::power::State::asleep_in(0),
        startup: crate::Startup::Started {
            settings: crate::tests::settings(),
            firmware: crate::tests::firmware(),
            boot: crate::tests::POWER_ON,
        },
        ..Model::default()
    };
    app.update(woke(&model, Wake::Other), &mut model)
        .expect_no_effect_or_events();
    assert!(asleep(&model));

    // In the hold, no `Sleep` request waits either.
    let mut shell = at_the_sleep_step();
    shell.stray();
    let mut model = shell.model;
    for wake in [Wake::Timer, Wake::Other, Wake::Touch(None)] {
        app.update(woke(&model, wake), &mut model)
            .expect_no_effect_or_events();
        assert_eq!(power::sleep_step(&model), Some(SleepStep::Held));
    }
}

#[test]
fn a_battery_level_or_sleep_now_in_the_hold_does_nothing() {
    let mut shell = at_the_sleep_step();
    shell.stray();
    shell.take_log();

    shell.send(Event::BatteryChanged(Some(41)));
    shell.send(Event::SleepNow);
    assert_eq!(shell.take_log(), Vec::<alloc::string::String>::new());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Held));

    // The hold continues.
    shell.advance(STRAY_HOLD);
    assert_eq!(shell.take_log(), ["Sleep"]);
}

/// Asserts that no timer waited at any `Sleep` request of `shell`. Refer
/// to [`Shell::timers_at_sleep`].
#[track_caller]
fn assert_no_timer_at_sleep(shell: &Shell) {
    assert!(!shell.timers_at_sleep.is_empty(), "no Sleep request");
    for (index, timers) in shell.timers_at_sleep.iter().enumerate() {
        assert_eq!(timers, &Vec::<Duration>::new(), "Sleep request {index}");
    }
}

#[test]
fn no_core_timer_waits_at_a_sleep_request() {
    // A timer of the core cannot wake the chip, so no timer of the core may
    // wait at a `Sleep` request. The check is the timer of the request.
    let mut shell = crate::tests::flows::house();
    shell.send(start(PowerSource::Battery));
    // A tap on a light turns the front light on and sends a call.
    shell.send(Event::Navigate(crate::Route::room("living_room")));
    shell.send(crate::tests::tap_tile(0));
    // The sleep after the idle timeout, checks, a stray wake and its hold.
    shell.advance(IDLE_TIMEOUT);
    shell.check();
    shell.stray();
    shell.advance(STRAY_HOLD);
    shell.check();
    // A touch wake, then a sleep with the front light on.
    shell.send(Event::Tapped(None));
    shell.send(Event::SleepNow);
    shell.stray();
    shell.advance(STRAY_HOLD);
    // Off the stand and the idle timeout.
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert_eq!(shell.timers_at_sleep.len(), 7);
    assert_no_timer_at_sleep(&shell);
}
