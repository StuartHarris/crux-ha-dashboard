//! Tests of the front light under the control of Home Assistant, with the
//! test shell and the fake broker: the commands on `set/light`, the tap
//! levels, and the state of the light on `dashboard/<id>/light`.

use alloc::{string::String, vec::Vec};

use super::{flows::house, light_state, shell::Shell, start, start_at, tap_tile};
use crate::{
    Event, PowerSource,
    capabilities::power::Wake,
    features::{
        connection::Link,
        light::{DEFAULT_BATTERY_LEVEL, DEFAULT_STAND_LEVEL, LIGHT_TIMEOUT},
        power::IDLE_TIMEOUT,
    },
    mqtt::Qos,
};

/// The light topic of the test settings.
const LIGHT: &str = "dashboard/hall/light";

/// The command topic of the light.
const SET_LIGHT: &str = "dashboard/hall/set/light";

/// The topic of the tap level on the stand.
const TAP_STAND: &str = "dashboard/hall/set/tap_level_stand";

/// The topic of the tap level on battery.
const TAP_BATTERY: &str = "dashboard/hall/set/tap_level_battery";

/// The topic of the service calls.
const CMD: &str = "dashboard/hall/cmd";

/// Starts the house on `power`, connects, and clears the logs.
fn connected(power: PowerSource) -> Shell {
    let mut shell = house();
    shell.send(start(power));
    assert_eq!(shell.model.connection.link(), Link::Up);
    shell.take_log();
    shell.take_logs();
    shell
}

/// Publishes the light command `json`, as Home Assistant does, and runs
/// the core.
fn command(shell: &mut Shell, json: &str) {
    shell.broker.publish(SET_LIGHT, json.as_bytes(), false);
    shell.run();
}

/// Publishes `payload` retained on the topic of a tap level, as Home
/// Assistant does for a number with `ret`, and runs the core.
fn tap_level(shell: &mut Shell, topic: &str, payload: &str) {
    shell.broker.publish(topic, payload.as_bytes(), true);
    shell.run();
}

/// Returns each state of the light that the broker got, in order.
fn light_states(shell: &Shell) -> Vec<String> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == LIGHT)
        .map(|publish| String::from_utf8_lossy(&publish.payload).into_owned())
        .collect()
}

/// Returns the states of the light at the levels `levels` (0 is OFF).
fn states(levels: &[u8]) -> Vec<String> {
    levels.iter().map(|level| light_state(*level)).collect()
}

#[test]
fn a_live_on_sets_the_light_and_publishes_its_state_retained() {
    let mut shell = connected(PowerSource::Stand);
    command(&mut shell, r#"{"state":"ON","brightness":40}"#);
    assert_eq!(shell.light, 40);
    assert_eq!(light_states(&shell), states(&[0, 40]));
    let publish = shell
        .broker
        .published
        .iter()
        .rfind(|publish| publish.topic == LIGHT)
        .expect("a state of the light");
    assert!(publish.retain);
    assert_eq!(publish.qos, Qos::AtLeastOnce);
    assert_eq!(
        shell.take_logs(),
        ["debug device: Home Assistant turned the front light on at 40%"]
    );

    // A brightness of 0 is OFF.
    command(&mut shell, r#"{"state":"ON","brightness":0}"#);
    assert_eq!(shell.light, 0);
    assert_eq!(light_states(&shell), states(&[0, 40, 0]));
}

#[test]
fn a_retained_command_at_the_connect_changes_nothing() {
    let mut shell = house();
    shell
        .broker
        .retain(SET_LIGHT, br#"{"state":"ON","brightness":40}"#);
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert!(
        shell
            .broker
            .delivered
            .iter()
            .any(|topic| topic == SET_LIGHT)
    );

    // The connection publishes the state once, and the command publishes
    // nothing.
    assert_eq!(shell.light, 0);
    assert_eq!(light_states(&shell), states(&[0]));
    let logs = shell.take_logs();
    assert!(
        logs.contains(
            &"info device: the light command on dashboard/hall/set/light is retained, so the \
              core ignored it"
                .into()
        ),
        "{logs:?}"
    );
}

#[test]
fn a_command_that_is_not_valid_is_ignored_with_a_warning() {
    let mut shell = connected(PowerSource::Stand);
    command(&mut shell, r#"{"state":"ON","brightness":101}"#);
    assert_eq!(shell.light, 0);
    assert_eq!(light_states(&shell), states(&[0]));
    assert_eq!(
        shell.take_logs(),
        [
            r#"warn device: the light command {\"state\":\"ON\",\"brightness\":101} is not valid, so the core ignored it"#
        ]
    );
}

#[test]
fn retained_tap_levels_before_the_start_set_the_level_of_the_first_tap() {
    for (power, topic, other) in [
        (PowerSource::Stand, TAP_STAND, TAP_BATTERY),
        (PowerSource::Battery, TAP_BATTERY, TAP_STAND),
    ] {
        let mut shell = house();
        shell.broker.retain(topic, b"40");
        shell.broker.retain(other, b"5");
        shell.send(start(power));
        assert_eq!(shell.model.connection.link(), Link::Up);

        shell.send(Event::Tapped(None));
        assert_eq!(shell.light, 40, "{power:?}");
        assert_eq!(light_states(&shell), states(&[0, 40]), "{power:?}");
    }
}

#[test]
fn a_live_tap_level_applies_and_an_empty_payload_gives_the_default() {
    let mut shell = connected(PowerSource::Stand);
    tap_level(&mut shell, TAP_STAND, "30");
    assert_eq!(
        shell.take_logs(),
        ["debug device: Home Assistant set the tap level on the stand to 30%"]
    );
    shell.send(Event::Tapped(None));
    assert_eq!(shell.light, 30);

    // A user clears the retained message. The light of the tap takes the
    // default at once.
    tap_level(&mut shell, TAP_STAND, "");
    assert_eq!(shell.light, DEFAULT_STAND_LEVEL.get());

    // A payload that is not valid changes nothing.
    tap_level(&mut shell, TAP_STAND, "40.5");
    assert_eq!(shell.light, DEFAULT_STAND_LEVEL.get());
    let logs = shell.take_logs();
    assert_eq!(
        logs.last().map(String::as_str),
        Some(
            "warn device: the tap level 40.5 on dashboard/hall/set/tap_level_stand is not \
             valid, so the core ignored it"
        )
    );
}

#[test]
fn each_connection_publishes_the_light_once_and_a_wake_tap_publishes_on_then_off() {
    let mut shell = connected(PowerSource::Battery);
    assert_eq!(light_states(&shell), states(&[0]));

    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert_eq!(light_states(&shell), states(&[0]));

    // The wake tap turns the light on. The new connection publishes it.
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    let battery = DEFAULT_BATTERY_LEVEL.get();
    assert_eq!(light_states(&shell), states(&[0, battery]));

    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(light_states(&shell), states(&[0, battery, 0]));
}

#[test]
fn off_while_the_light_is_off_still_publishes_a_state() {
    let mut shell = connected(PowerSource::Stand);
    command(&mut shell, r#"{"state":"OFF"}"#);
    assert_eq!(light_states(&shell), states(&[0, 0]));
}

#[test]
fn a_command_while_the_state_waits_for_its_puback_publishes_again_after_it() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_puback = true;
    command(&mut shell, r#"{"state":"ON","brightness":40}"#);
    assert_eq!(light_states(&shell), states(&[0, 40]));

    // At most one state of the light waits for a PUBACK.
    command(&mut shell, r#"{"state":"ON","brightness":40}"#);
    assert_eq!(light_states(&shell), states(&[0, 40]));

    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert_eq!(light_states(&shell), states(&[0, 40, 40]));
}

#[test]
fn a_sleep_publishes_no_state_of_the_light_and_the_wake_publishes_the_state_then() {
    let mut shell = connected(PowerSource::Battery);
    command(&mut shell, r#"{"state":"ON","brightness":40}"#);
    assert_eq!(light_states(&shell), states(&[0, 40]));

    // The sleep turns the light off, but the queue is closed. The broker
    // keeps the last state.
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    assert_eq!(shell.light, 0);
    assert_eq!(light_states(&shell), states(&[0, 40]));
    let on = light_state(40);
    assert_eq!(shell.broker.retained(LIGHT), Some(on.as_str()));

    // The move to the stand wakes the device with no tap. The new
    // connection publishes the light as it is now.
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(light_states(&shell), states(&[0, 40, 0]));
}

#[test]
fn a_dry_run_obeys_the_light_but_publishes_no_state() {
    let mut shell = house();
    let mut start = start(PowerSource::Stand);
    if let Event::Start(start) = &mut start {
        start.settings.read_only = true;
    }
    shell.send(start);
    assert_eq!(shell.model.connection.link(), Link::Up);
    command(&mut shell, r#"{"state":"ON","brightness":40}"#);
    assert_eq!(shell.light, 40);
    assert_eq!(light_states(&shell), Vec::<String>::new());

    // The end of the dry run publishes the state.
    shell.send(Event::SetReadOnly(false));
    assert_eq!(light_states(&shell), states(&[40]));
}

#[test]
fn a_tap_publishes_on_and_the_light_timeout_publishes_off() {
    let mut shell = connected(PowerSource::Stand);
    shell.send(Event::Tapped(None));
    let stand = DEFAULT_STAND_LEVEL.get();
    assert_eq!(light_states(&shell), states(&[0, stand]));

    // A second tap does not change the light, so it publishes nothing.
    shell.send(Event::Tapped(None));
    assert_eq!(light_states(&shell), states(&[0, stand]));

    shell.advance(LIGHT_TIMEOUT);
    assert_eq!(light_states(&shell), states(&[0, stand, 0]));
}

#[test]
fn a_tap_on_a_tile_publishes_the_service_call_before_the_state_of_the_light() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    let writes = shell.writes.len();

    shell.send(tap_tile(1));
    let topics: Vec<String> = shell.writes[writes..]
        .iter()
        .flat_map(|write| write.iter().filter_map(super::broker::publish_of))
        .map(|publish| publish.topic)
        .collect();
    assert_eq!(topics, [CMD, LIGHT]);
}
