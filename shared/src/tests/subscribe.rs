//! Tests of the subscribe request and of `WifiQuiet`: the core subscribes
//! after each connect, and the link is quiet when the broker acknowledged
//! all the topic filters.
//!
//! The step tests drive the core one update at a time. The flow tests run
//! the core in the test shell, against the fake broker.

use alloc::string::String;

use crux_core::App;

use super::{connection::drive_to_connected, flows::house, start};
use crate::{
    Dashboard, EffectTestExt, Event, PowerSource,
    capabilities::{mqtt::SubscribeOutcome, power::Wake},
    features::{connection::RECONNECT_MIN, power::IDLE_TIMEOUT},
};

/// Returns the number of times that `effect` is in `log`.
fn count(log: &[String], effect: &str) -> usize {
    log.iter().filter(|entry| *entry == effect).count()
}

/// Returns the place of the first `effect` in `log`.
fn place(log: &[String], effect: &str) -> Option<usize> {
    log.iter().position(|entry| entry == effect)
}

#[test]
fn only_done_for_the_current_connection_makes_the_link_quiet() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    let older = model.connection.id.wrapping_sub(1);
    let answer = |connection, outcome| Event::Subscribed {
        connection,
        outcome,
    };
    app.update(answer(older, SubscribeOutcome::Done), &mut model)
        .expect_no_effects();

    let connection = model.connection.id;
    app.update(answer(connection, SubscribeOutcome::Failed), &mut model)
        .expect_no_effects();
    app.update(answer(connection, SubscribeOutcome::Done), &mut model)
        .expect_only_wifi_quiet();
}

#[test]
fn the_link_is_quiet_after_the_subscribe_is_done() {
    let mut shell = house();
    shell.broker.hold_suback = true;
    shell.send(start(PowerSource::Stand));
    // The subscribe waits, so the link is not quiet yet.
    assert_eq!(count(&shell.take_log(), "WifiQuiet"), 0);

    // The connection closes before the SUBACK. The next connection
    // subscribes again, and the answer makes the link quiet.
    shell.broker.drop_connection();
    shell.run();
    shell.broker.hold_suback = false;
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(count(&log, "MqttSubscribe"), 1, "{log:?}");
    assert!(
        place(&log, "MqttSubscribe") < place(&log, "WifiQuiet"),
        "{log:?}"
    );
    assert_eq!(count(&log, "WifiQuiet"), 1, "{log:?}");
}

#[test]
fn a_wake_subscribes_again_and_then_the_link_is_quiet() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    shell.take_log();

    shell.wake(Wake::Timer);
    let log = shell.take_log();
    let status = place(&log, "MqttPublish dashboard/hall/status online");
    let subscribe = place(&log, "MqttSubscribe");
    let quiet = place(&log, "WifiQuiet");
    assert!(status.is_some() && status < subscribe, "{log:?}");
    assert!(subscribe < quiet, "{log:?}");
}

#[test]
fn the_link_is_not_quiet_during_the_sleep_steps() {
    let mut shell = house();
    shell.broker.hold_suback = true;
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    // The subscribe ended with `Failed` at the disconnect of the sleep.
    assert_eq!(count(&shell.take_log(), "WifiQuiet"), 0);
}
