//! Tests of the light feature: the front light after a touch, its timer,
//! the power source and the sleep.

use crux_core::App;

use super::{ExpectLight, connection::drive_to_connected, shell::Shell, start, wire::ExpectWrite};
use crate::{
    Dashboard, EffectTestExt, Event, Model, PowerSource,
    capabilities::power::Wake,
    features::{
        light::{BATTERY_LEVEL, LIGHT_TIMEOUT, STAND_LEVEL},
        power::IDLE_TIMEOUT,
    },
};

#[test]
fn start_turns_the_light_off() {
    let app = Dashboard;
    let mut model = Model::default();

    // The core does not know the state of the light after a reset, so it
    // sends the level 0 also when its model has the light off.
    let mut cmd = app.update(start(PowerSource::Stand), &mut model);
    cmd.expect_light(0);
    assert_eq!(model.light.level, 0);
}

#[test]
fn a_tap_turns_the_light_on_at_the_level_of_the_power_source() {
    let app = Dashboard;
    for (source, level) in [
        (PowerSource::Stand, STAND_LEVEL),
        (PowerSource::Battery, BATTERY_LEVEL),
    ] {
        let (mut model, _wire) = drive_to_connected(source);

        app.update(Event::Tapped(None), &mut model)
            .expect_touch_light(level)
            .expect_no_effects();
        assert_eq!(model.light.level, level, "{source:?}");
    }
}

#[test]
fn the_light_goes_off_at_the_light_timeout() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::Tapped(None), &mut model);

    app.update(Event::LightTimeout(model.light.timer), &mut model)
        .expect_light(0)
        .expect_no_effects();
    assert_eq!(model.light.level, 0);
}

#[test]
fn a_second_tap_starts_the_timer_again_and_the_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::Tapped(None), &mut model);
    let old = model.light.timer;

    // The light is on at the same level, so the core only starts a timer.
    app.update(Event::Tapped(None), &mut model)
        .expect_light_timer()
        .expect_no_effects();

    app.update(Event::LightTimeout(old), &mut model)
        .expect_no_effects();
    assert_eq!(model.light.level, STAND_LEVEL);
}

#[test]
fn a_change_of_the_power_source_while_the_light_is_on_sets_the_new_level() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::Tapped(None), &mut model);

    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_light(BATTERY_LEVEL)
        .expect_render()
        .expect_only_time_notify_after();
    assert_eq!(model.light.level, BATTERY_LEVEL);

    app.update(Event::PowerSourceChanged(PowerSource::Stand), &mut model)
        .expect_light(STAND_LEVEL)
        .expect_only_render();
    assert_eq!(model.light.level, STAND_LEVEL);
}

#[test]
fn a_change_of_the_power_source_while_the_light_is_off_does_not_set_it() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_only_time_notify_after();
    assert_eq!(model.light.level, 0);
}

#[test]
fn the_sleep_turns_the_light_off_before_it_disconnects() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(Event::Tapped(None), &mut model);

    // The core waits for the answer to `FrontLight` before it disconnects.
    let mut sleep = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    let mut light = sleep.expect_one_effect().expect_front_light();
    assert_eq!(light.operation.level, 0);
    assert!(model.power.asleep);
    assert_eq!(model.light.level, 0);

    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    let mut disconnect = app.update(light_off, &mut model);
    disconnect
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();

    // The light timer of the tap expires during the sleep. It does nothing.
    app.update(Event::LightTimeout(model.light.timer), &mut model)
        .expect_no_effects();
}

#[test]
fn a_tap_while_the_light_goes_off_for_the_sleep_wakes_and_the_late_answer_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let _ = app.update(Event::Tapped(None), &mut model);
    let mut sleep = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    let mut light = sleep.expect_one_effect().expect_front_light();

    // The user taps before the light is off. The tap wakes the device and
    // turns the light on again.
    let _ = app.update(Event::Tapped(None), &mut model);
    assert!(!model.power.asleep);
    assert_eq!(model.light.level, BATTERY_LEVEL);

    // The answer of the sleep comes late. The core does not continue the
    // sleep.
    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    app.update(light_off, &mut model).expect_no_effects();
    assert!(!model.power.asleep);
}

#[test]
fn a_tap_on_no_control_lights_the_light_but_the_sleep_comes_on_time_and_turns_it_off() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;

    // A touch by accident turns the light on, but it does not start the idle
    // timer again.
    app.update(Event::Tapped(None), &mut model)
        .expect_touch_light(BATTERY_LEVEL)
        .expect_no_effects();
    assert_eq!(model.power.idle, idle);

    // The first idle timer expires while the light is on. The first step of
    // the sleep turns the light off, and only then the core disconnects.
    let mut sleep = app.update(Event::IdleTimeout(idle), &mut model);
    let mut light = sleep.expect_one_effect().expect_front_light();
    assert_eq!(light.operation.level, 0);
    assert!(model.power.asleep);

    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    app.update(light_off, &mut model)
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
    assert_eq!(model.light.level, 0);
}

#[test]
fn a_sleep_with_the_light_off_disconnects_at_once() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);

    app.update(Event::IdleTimeout(model.power.idle), &mut model)
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
}

#[test]
fn a_tap_on_no_control_turns_the_light_on_but_does_not_keep_the_device_awake() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    let idle = shell.model.power.idle;

    shell.send(Event::Tapped(None));
    assert_eq!(shell.light, BATTERY_LEVEL);
    assert_eq!(shell.model.power.idle, idle);

    // The tap started the light timer.
    let events = shell.advance(LIGHT_TIMEOUT);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LightTimeout(_))),
        "{events:?}"
    );
    assert_eq!(shell.light, 0);
}

#[test]
fn on_battery_the_light_goes_off_at_the_light_timeout_and_the_sleep_keeps_it_off() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    shell.send(Event::Tapped(None));
    assert_eq!(shell.light, BATTERY_LEVEL);

    shell.take_log();
    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(shell.take_log(), ["FrontLight 0"]);
    assert_eq!(shell.light, 0);

    // The light is already off, so the sleep does not send `FrontLight`.
    shell.advance(IDLE_TIMEOUT.saturating_sub(LIGHT_TIMEOUT));
    assert!(shell.sleep.is_some());
    let log = shell.take_log();
    assert!(
        !log.iter().any(|effect| effect.starts_with("FrontLight")),
        "{log:?}"
    );
}

#[test]
fn the_sleep_turns_the_light_off_and_a_wake_tap_turns_it_on_again() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    // Half of the idle time passes. Then a tap on no control turns the light
    // on, but it does not start the idle timer again.
    let half = IDLE_TIMEOUT / 2;
    shell.advance(half);
    shell.send(Event::Tapped(None));
    assert_eq!(shell.light, BATTERY_LEVEL);

    // The idle timer expires while the light is still on.
    shell.take_log();
    shell.advance(half);
    assert!(shell.sleep.is_some());
    assert_eq!(shell.light, 0);
    let log = shell.take_log();
    let light = log.iter().position(|effect| effect == "FrontLight 0");
    let disconnect = log.iter().position(|effect| effect == "MqttDisconnect");
    assert!(light.is_some() && light < disconnect, "{log:?}");

    // The sleep turned the light off, so it cleared the light timer of the
    // tap.
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    let events = shell.advance(LIGHT_TIMEOUT);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LightTimeout(_))),
        "{events:?}"
    );
    assert_eq!(shell.light, 0);
    assert!(shell.sleep.is_some());

    // A wake tap turns the light on again, and the new light timer turns it
    // off before the next sleep.
    shell.wake(Wake::Touch(None));
    assert!(!shell.model.power.asleep);
    assert_eq!(shell.light, BATTERY_LEVEL);
    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(shell.light, 0);
    assert!(!shell.model.power.asleep);
}

#[test]
fn a_second_tap_clears_the_light_timer_and_only_the_new_timer_fires() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::Tapped(None));
    shell.take_log();

    shell.send(Event::Tapped(None));
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");

    let events = shell.advance(LIGHT_TIMEOUT);
    let timeouts = events
        .iter()
        .filter(|event| matches!(event, Event::LightTimeout(_)))
        .count();
    assert_eq!(timeouts, 1, "{events:?}");
    assert_eq!(shell.light, 0);
}
