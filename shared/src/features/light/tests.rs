//! Tests of the light feature: the front light after a touch, its timer,
//! the power source and the sleep.

use crate::features::{light, power};
use alloc::format;

use crux_core::App;

use crate::tests::{
    ExpectLight, ExpectServiceCall, ExpectStep, drive_to_connected, light_state, shell::Shell,
    start, state, tap_tile, wire::ExpectWrite,
};
use crate::{
    Cmd, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource, Route,
    capabilities::{
        log::{Area, Level as LogLevel},
        power::Wake,
    },
    features::{
        device::{self, Button},
        light::{
            Command, DEFAULT_BATTERY_LEVEL, DEFAULT_STAND_LEVEL, LIGHT_TIMEOUT, Level, Percent,
            TapLevel,
        },
        power::IDLE_TIMEOUT,
    },
};

/// Returns the lit level `percent`.
fn level(percent: u8) -> Level {
    Level::new(percent).expect("a lit level")
}

/// Returns the event of the command `command` of Home Assistant.
const fn commanded(command: Command) -> Event {
    Event::Light(light::Event::Commanded(command))
}

/// Returns the event of the command ON at `percent`.
fn on_at(percent: u8) -> Event {
    commanded(Command::On(Some(level(percent))))
}

/// Returns the event of a tap level of `source`: `None` is an empty
/// payload, which is the default.
fn tap_level(source: PowerSource, percent: Option<u8>) -> Event {
    let level = percent.map_or(TapLevel::Default, |percent| {
        TapLevel::Set(Percent::new(percent).expect("a tap level"))
    });
    Event::Light(light::Event::TapLevelSet { source, level })
}

/// Returns `true` if `events` has the expiry of a light timer.
fn has_light_timeout(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, Event::Light(light::Event::Timeout(_))))
}

/// Expects the lines of the log of the light.
trait ExpectDeviceLine {
    /// Expects the next effect to write `message` with the area `device` at
    /// the level `Debug`. The helper drops the `Log` request with no
    /// answer.
    fn expect_device_debug(&mut self, message: &str) -> &mut Self;
}

impl ExpectDeviceLine for Cmd {
    #[track_caller]
    fn expect_device_debug(&mut self, message: &str) -> &mut Self {
        self.expect_log_with(|log| {
            assert_eq!(log.level, LogLevel::Debug);
            assert_eq!(log.area, Area::Device);
            assert_eq!(log.message, message);
        })
    }
}

/// The light topic of the test settings.
const LIGHT: &str = "dashboard/hall/light";

/// The state topic of the test settings.
const STATE: &str = "dashboard/hall/state";

#[test]
fn start_turns_the_light_off() {
    let app = Dashboard;
    let mut model = Model::default();

    // The core does not know the state of the light after a reset, so it
    // sends the level 0 also when its model has the light off.
    let mut cmd = app.update(start(PowerSource::Stand), &mut model);
    cmd.expect_light(0);
    assert_eq!(model.light.level(), 0);
}

#[test]
fn a_tap_turns_the_light_on_at_the_level_of_the_power_source() {
    let app = Dashboard;
    for (source, level) in [
        (PowerSource::Stand, DEFAULT_STAND_LEVEL.get()),
        (PowerSource::Battery, DEFAULT_BATTERY_LEVEL.get()),
    ] {
        let (mut model, mut wire) = drive_to_connected(source);

        app.update(Event::Tapped(None), &mut model)
            .expect_touch_light(level)
            .expect_light_publish(&mut wire, &mut model, level)
            .expect_only_session_clear(&wire);
        assert_eq!(model.light.level(), level, "{source:?}");
    }
}

#[test]
fn the_light_goes_off_at_the_light_timeout() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);

    app.update(
        Event::Light(light::Event::Timeout(model.light.timer)),
        &mut model,
    )
    .expect_light(0)
    .expect_light_publish(&mut wire, &mut model, 0)
    .expect_only_session_clear(&wire);
    assert_eq!(model.light.level(), 0);
}

#[test]
fn a_second_tap_starts_the_timer_again_and_the_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);
    let old = model.light.timer;

    // The light is on at the same level, so the core only starts a timer,
    // and it publishes no state of the light.
    app.update(Event::Tapped(None), &mut model)
        .expect_light_timer()
        .expect_no_effects();

    app.update(Event::Light(light::Event::Timeout(old)), &mut model)
        .expect_no_effects();
    assert_eq!(model.light.level(), DEFAULT_STAND_LEVEL.get());
}

#[test]
fn a_change_of_the_power_source_while_the_light_is_on_sets_the_new_level() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);

    // The light message goes before the state message, in one write.
    let battery = light_state(DEFAULT_BATTERY_LEVEL.get());
    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_light(DEFAULT_BATTERY_LEVEL.get())
        .expect_render()
        .expect_time_notify_after()
        .expect_publishes(
            &mut wire,
            &mut model,
            &[(LIGHT, &battery), (STATE, &state("null", "OFF"))],
        )
        .expect_only_session_clear(&wire);
    assert_eq!(model.light.level(), DEFAULT_BATTERY_LEVEL.get());

    let stand = light_state(DEFAULT_STAND_LEVEL.get());
    app.update(Event::PowerSourceChanged(PowerSource::Stand), &mut model)
        .expect_light(DEFAULT_STAND_LEVEL.get())
        .expect_render()
        .expect_publishes(
            &mut wire,
            &mut model,
            &[(LIGHT, &stand), (STATE, &state("null", "ON"))],
        )
        .expect_only_session_clear(&wire);
    assert_eq!(model.light.level(), DEFAULT_STAND_LEVEL.get());
}

#[test]
fn a_change_of_the_power_source_while_the_light_is_off_does_not_set_it() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_time_notify_after()
        .expect_state(&mut wire, &mut model, &state("null", "OFF"))
        .expect_only_session_clear(&wire);
    assert_eq!(model.light.level(), 0);
}

#[test]
fn the_sleep_turns_the_light_off_before_it_disconnects() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);

    // The core waits for the answer to `FrontLight` before it disconnects.
    let mut sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let mut light = sleep
        .expect_step("sleep 1 turns the front light off")
        .expect_one_effect()
        .expect_front_light();
    assert_eq!(light.operation.level, 0);
    assert!(model.power.is_asleep());
    assert_eq!(model.light.level(), 0);

    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    let mut disconnect = app.update(light_off, &mut model);
    disconnect
        .expect_step("sleep 1 closes the connection")
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();

    // The light timer of the tap expires during the sleep. It does nothing.
    app.update(
        Event::Light(light::Event::Timeout(model.light.timer)),
        &mut model,
    )
    .expect_no_effects();
}

#[test]
fn a_tap_while_the_light_goes_off_for_the_sleep_wakes_and_the_late_answer_is_ignored() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);
    let mut sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let mut light = sleep
        .expect_step("sleep 1 turns the front light off")
        .expect_one_effect()
        .expect_front_light();

    // The user taps before the light is off. The tap wakes the device and
    // turns the light on again.
    let _ = app.update(Event::Tapped(None), &mut model);
    assert!(!model.power.is_asleep());
    assert_eq!(model.light.level(), DEFAULT_BATTERY_LEVEL.get());

    // The answer of the sleep comes late. The core does not continue the
    // sleep.
    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    app.update(light_off, &mut model).expect_no_effects();
    assert!(!model.power.is_asleep());
}

#[test]
fn a_tap_on_no_control_lights_the_light_but_the_sleep_comes_on_time_and_turns_it_off() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;

    // A touch by accident turns the light on, but it does not start the idle
    // timer again.
    app.update(Event::Tapped(None), &mut model)
        .expect_touch_light(DEFAULT_BATTERY_LEVEL.get())
        .expect_light_publish(&mut wire, &mut model, DEFAULT_BATTERY_LEVEL.get())
        .expect_only_session_clear(&wire);
    assert_eq!(model.power.idle, idle);

    // The first idle timer expires while the light is on. The first step of
    // the sleep turns the light off, and only then the core disconnects.
    let mut sleep = app.update(Event::Power(power::Event::IdleTimeout(idle)), &mut model);
    let mut light = sleep
        .expect_step("sleep 1 turns the front light off")
        .expect_one_effect()
        .expect_front_light();
    assert_eq!(light.operation.level, 0);
    assert!(model.power.is_asleep());

    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    app.update(light_off, &mut model)
        .expect_step("sleep 1 closes the connection")
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
    assert_eq!(model.light.level(), 0);
}

#[test]
fn a_sleep_with_the_light_off_disconnects_at_once() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);

    app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    )
    .expect_step("sleep 1 closes the connection")
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
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());
    assert_eq!(shell.model.power.idle, idle);

    // The tap started the light timer.
    let events = shell.advance(LIGHT_TIMEOUT);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Light(light::Event::Timeout(_)))),
        "{events:?}"
    );
    assert_eq!(shell.light, 0);
}

#[test]
fn on_battery_the_light_goes_off_at_the_light_timeout_and_the_sleep_keeps_it_off() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    shell.send(Event::Tapped(None));
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());

    shell.take_log();
    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(
        shell.take_log(),
        [
            "FrontLight 0",
            r#"MqttPublish dashboard/hall/light {"state":"OFF"}"#
        ]
    );
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
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());

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
            .any(|event| matches!(event, Event::Light(light::Event::Timeout(_)))),
        "{events:?}"
    );
    assert_eq!(shell.light, 0);
    assert!(shell.sleep.is_some());

    // A wake tap turns the light on again, and the new light timer turns it
    // off before the next sleep.
    shell.wake(Wake::Touch(None));
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());
    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(shell.light, 0);
    assert!(!shell.model.power.is_asleep());
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
        .filter(|event| matches!(event, Event::Light(light::Event::Timeout(_))))
        .count();
    assert_eq!(timeouts, 1, "{events:?}");
    assert_eq!(shell.light, 0);
}

#[test]
fn a_change_of_the_power_source_while_the_light_is_on_keeps_the_light_timer() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::Tapped(None));
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());

    // The timer of the tap still runs, so the light goes off at the
    // light timeout.
    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(shell.light, 0);
}

#[test]
fn a_start_while_the_light_is_on_clears_the_light_timer() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::Tapped(None));
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.light, 0);

    // The start cleared the timer of the tap, so no light timeout fires.
    let events = shell.advance(LIGHT_TIMEOUT);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Light(light::Event::Timeout(_)))),
        "{events:?}"
    );
}

// The tap levels.

#[test]
fn a_tap_at_the_tap_level_0_does_not_turn_the_light_on_or_start_a_timer() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    app.update(tap_level(PowerSource::Stand, Some(0)), &mut model)
        .expect_device_debug("Home Assistant set the tap level on the stand to 0%")
        .expect_no_effects();
    let timer = model.light.timer;

    app.update(Event::Tapped(None), &mut model)
        .expect_no_effects();
    assert_eq!(model.light.level(), 0);
    assert_eq!(model.light.timer, timer);
}

#[test]
fn a_tap_uses_the_tap_level_of_the_power_source() {
    let app = Dashboard;
    for (source, percent) in [(PowerSource::Stand, 80), (PowerSource::Battery, 10)] {
        let (mut model, mut wire) = drive_to_connected(source);
        // The tap level of the other source does not apply.
        let other = match source {
            PowerSource::Stand => PowerSource::Battery,
            PowerSource::Battery => PowerSource::Stand,
        };
        let _ = app.update(tap_level(other, Some(5)), &mut model);
        let _ = app.update(tap_level(source, Some(percent)), &mut model);

        app.update(Event::Tapped(None), &mut model)
            .expect_touch_light(percent)
            .expect_light_publish(&mut wire, &mut model, percent)
            .expect_only_session_clear(&wire);
        assert_eq!(
            model.light.tap_level(source),
            TapLevel::Set(Percent::new(percent).unwrap())
        );
    }
}

#[test]
fn a_new_tap_level_while_a_tap_lights_the_light_sets_it_and_keeps_the_timer() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);
    let timer = model.light.timer;

    app.update(tap_level(PowerSource::Stand, Some(40)), &mut model)
        .expect_device_debug("Home Assistant set the tap level on the stand to 40%")
        .expect_light(40)
        .expect_light_publish(&mut wire, &mut model, 40)
        .expect_only_session_clear(&wire);
    assert_eq!(model.light.level(), 40);

    // The timer of the tap still turns the light off at its time.
    assert_eq!(model.light.timer, timer);
    app.update(Event::Light(light::Event::Timeout(timer)), &mut model)
        .expect_light(0)
        .expect_light_publish(&mut wire, &mut model, 0)
        .expect_only_session_clear(&wire);
}

#[test]
fn a_new_tap_level_of_0_while_a_tap_lights_the_light_turns_it_off_and_clears_the_timer() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::Tapped(None));
    shell.take_log();

    shell.send(tap_level(PowerSource::Stand, Some(0)));
    assert_eq!(shell.light, 0);
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    assert!(!has_light_timeout(&shell.advance(LIGHT_TIMEOUT)));
}

#[test]
fn a_new_tap_level_of_the_other_source_does_not_change_the_light() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);

    app.update(tap_level(PowerSource::Battery, Some(10)), &mut model)
        .expect_device_debug("Home Assistant set the tap level on battery to 10%")
        .expect_no_effects();
    assert_eq!(model.light.level(), DEFAULT_STAND_LEVEL.get());
}

#[test]
fn the_same_tap_level_again_does_nothing() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);

    // The default is already the tap level after the start.
    app.update(tap_level(PowerSource::Stand, None), &mut model)
        .expect_no_effects();
    wire.settle(
        app.update(tap_level(PowerSource::Stand, Some(40)), &mut model),
        &mut model,
    );
    // The broker sends the retained tap level again at each connect.
    app.update(tap_level(PowerSource::Stand, Some(40)), &mut model)
        .expect_no_effects();

    // An empty payload gives the default again.
    app.update(tap_level(PowerSource::Stand, None), &mut model)
        .expect_device_debug("Home Assistant set the tap level on the stand to the default")
        .expect_light(DEFAULT_STAND_LEVEL.get())
        .expect_light_publish(&mut wire, &mut model, DEFAULT_STAND_LEVEL.get())
        .expect_only_session_clear(&wire);
    assert_eq!(model.light.tap_level(PowerSource::Stand), TapLevel::Default);
}

#[test]
fn a_change_of_the_power_source_to_the_tap_level_0_turns_the_light_off_and_clears_the_timer() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.send(tap_level(PowerSource::Battery, Some(0)));
    shell.send(Event::Tapped(None));
    assert_eq!(shell.light, DEFAULT_STAND_LEVEL.get());
    shell.take_log();

    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert_eq!(shell.light, 0);
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    assert!(!has_light_timeout(&shell.advance(LIGHT_TIMEOUT)));
}

// The commands of Home Assistant.

#[test]
fn a_change_of_the_power_source_while_home_assistant_holds_the_light_keeps_the_level() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.send(on_at(40));

    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert_eq!(shell.light, 40);
    assert!(shell.model.light.is_held());
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert_eq!(shell.light, 40);
}

#[test]
fn on_with_a_brightness_holds_the_light_with_no_timer() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    // The command starts no light timer.
    app.update(on_at(40), &mut model)
        .expect_device_debug("Home Assistant turned the front light on at 40%")
        .expect_light(40)
        .expect_light_publish(&mut wire, &mut model, 40)
        .expect_only_session_clear(&wire);
    assert!(model.light.is_held());
    assert_eq!(model.light.level(), 40);
}

#[test]
fn on_while_a_tap_lights_the_light_clears_the_timer_and_the_old_expiry_does_nothing() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);
    wire.effects.clear();
    let old = model.light.timer;

    app.update(on_at(40), &mut model)
        .expect_device_debug("Home Assistant turned the front light on at 40%")
        .expect_light(40)
        .expect_light_publish(&mut wire, &mut model, 40)
        .expect_only_session_clear(&wire);
    // The command of the tap, which the wire keeps, clears the light timer.
    assert!(
        wire.effects
            .iter()
            .any(|effect| matches!(effect, Effect::TimeClear(_))),
        "{:?}",
        wire.effects
    );

    app.update(Event::Light(light::Event::Timeout(old)), &mut model)
        .expect_no_effects();
    assert_eq!(model.light.level(), 40);
}

#[test]
fn on_with_no_brightness_from_off_uses_the_tap_level_or_the_default() {
    let app = Dashboard;
    for (source, tap, expected) in [
        (PowerSource::Stand, None, DEFAULT_STAND_LEVEL.get()),
        (PowerSource::Stand, Some(40), 40),
        // At the tap level 0, a tap does not light the light, so the
        // command uses the default.
        (PowerSource::Stand, Some(0), DEFAULT_STAND_LEVEL.get()),
        (PowerSource::Battery, None, DEFAULT_BATTERY_LEVEL.get()),
        (PowerSource::Battery, Some(0), DEFAULT_BATTERY_LEVEL.get()),
    ] {
        let (mut model, mut wire) = drive_to_connected(source);
        let _ = app.update(tap_level(source, tap), &mut model);

        app.update(commanded(Command::On(None)), &mut model)
            .expect_device_debug(&format!(
                "Home Assistant turned the front light on at {expected}%"
            ))
            .expect_light(expected)
            .expect_light_publish(&mut wire, &mut model, expected)
            .expect_only_session_clear(&wire);
        assert!(model.light.is_held(), "{source:?} {tap:?}");
    }
}

#[test]
fn on_with_no_brightness_while_a_tap_lights_the_light_keeps_its_level() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);
    let level = DEFAULT_STAND_LEVEL.get();

    // The level does not change, so the core sends no `FrontLight`. It
    // publishes the state again for the command.
    app.update(commanded(Command::On(None)), &mut model)
        .expect_device_debug(&format!(
            "Home Assistant turned the front light on at {level}%"
        ))
        .expect_light_publish(&mut wire, &mut model, level)
        .expect_only_session_clear(&wire);
    assert!(model.light.is_held());
}

#[test]
fn a_tap_while_home_assistant_holds_the_light_does_not_change_it_but_the_call_goes() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.route = Route::room("living_room");
    wire.settle(app.update(on_at(40), &mut model), &mut model);
    let timer = model.light.timer;

    // No `FrontLight`, no light timer and no state of the light.
    app.update(tap_tile(1), &mut model)
        .expect_only_service_call(&mut wire, &mut model);
    assert_eq!(model.light.level(), 40);
    assert_eq!(model.light.timer, timer);
}

#[test]
fn off_turns_the_light_off_from_each_state() {
    let app = Dashboard;
    let off = || commanded(Command::Off);
    let line = "Home Assistant turned the front light off";

    // From the light of a tap.
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(Event::Tapped(None), &mut model), &mut model);
    app.update(off(), &mut model)
        .expect_device_debug(line)
        .expect_light(0)
        .expect_light_publish(&mut wire, &mut model, 0)
        .expect_only_session_clear(&wire);

    // From the light that Home Assistant holds.
    wire.settle(app.update(on_at(40), &mut model), &mut model);
    app.update(off(), &mut model)
        .expect_device_debug(line)
        .expect_light(0)
        .expect_light_publish(&mut wire, &mut model, 0)
        .expect_only_session_clear(&wire);

    // From off: no `FrontLight`, but Home Assistant gets a state.
    app.update(off(), &mut model)
        .expect_device_debug(line)
        .expect_light_publish(&mut wire, &mut model, 0)
        .expect_only_session_clear(&wire);
    assert_eq!(model.light.level(), 0);
}

#[test]
fn a_light_timeout_while_home_assistant_holds_the_light_does_nothing() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    wire.settle(app.update(on_at(40), &mut model), &mut model);

    app.update(
        Event::Light(light::Event::Timeout(model.light.timer)),
        &mut model,
    )
    .expect_no_effects();
    assert_eq!(model.light.level(), 40);
}

#[test]
fn a_sleep_turns_off_the_light_that_home_assistant_holds_and_ignores_a_command() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    wire.settle(app.update(on_at(40), &mut model), &mut model);

    let mut sleep = app.update(
        Event::Power(power::Event::IdleTimeout(model.power.idle)),
        &mut model,
    );
    let mut light = sleep
        .expect_step("sleep 1 turns the front light off")
        .expect_one_effect()
        .expect_front_light();
    assert_eq!(light.operation.level, 0);
    assert_eq!(model.light.level(), 0);

    // The link is still up in this step, so a command can arrive. The sleep
    // wins.
    app.update(on_at(80), &mut model)
        .expect_device_debug(
            "Home Assistant sent a light command, but the device sleeps, so the core ignored it",
        )
        .expect_no_effects();
    assert_eq!(model.light.level(), 0);

    light.resolve(()).unwrap();
    let light_off = sleep.expect_one_event();
    app.update(light_off, &mut model)
        .expect_step("sleep 1 closes the connection")
        .expect_disconnect(&mut wire, &mut model)
        .expect_session_clear(&wire)
        .expect_no_effects();
}

#[test]
fn a_command_while_a_restart_waits_is_ignored() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.broker.hold_closes = true;
    shell.send(Event::Device(device::Event::Pressed(Button::Restart)));
    assert!(shell.model.connection.restart_waits());
    shell.take_logs();

    shell.send(on_at(40));
    assert_eq!(shell.light, 0);
    assert_eq!(
        shell.take_logs(),
        [
            "debug device: Home Assistant sent a light command, but the chip restarts, so the \
          core ignored it"
        ]
    );
}

#[test]
fn start_turns_the_light_off_and_sets_the_default_tap_levels() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(tap_level(PowerSource::Stand, Some(40)), &mut model);
    let _ = app.update(tap_level(PowerSource::Battery, Some(0)), &mut model);
    wire.settle(app.update(on_at(80), &mut model), &mut model);

    let mut cmd = app.update(start(PowerSource::Stand), &mut model);
    cmd.expect_light(0);
    assert_eq!(model.light.level(), 0);
    assert!(!model.light.is_held());
    for source in [PowerSource::Stand, PowerSource::Battery] {
        assert_eq!(
            model.light.tap_level(source),
            TapLevel::Default,
            "{source:?}"
        );
    }
}
