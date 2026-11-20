//! Tests of the subscribe and of the quiet link: the core subscribes after
//! each connect, and the link is quiet when the broker acknowledged all the
//! topic filters.
//!
//! The link is quiet only inside the core. Thus each test first joins the
//! cached access point, so that a scan for a better access point is due.
//! The `Scan` request then shows that the link is quiet.
//!
//! The step tests drive the core one update at a time. The flow tests run
//! the core in the test shell, against the fake broker.

use alloc::string::String;

use crux_core::App;

use super::{
    connection::{drive_to_connected, drive_to_connecting},
    flows::house,
    shell::Shell,
    start,
    wire::ExpectWrite,
};
use crate::{
    Dashboard, Effect, EffectTestExt, Event, Model, PowerSource,
    capabilities::{power::Wake, radio::Target, stream::StreamItem},
    entities::topics,
    fake::radio::HALL,
    features::{connection::RECONNECT_MIN, power::IDLE_TIMEOUT},
    pages::PAGES,
};

/// Gives the cache of `model` the access point [`HALL`] of a cached join, so
/// that a scan is due.
fn cache_hall(model: &mut Model) {
    model.wifi.cache.joined(Target::Cached(HALL), HALL);
}

/// Returns the house of the flow tests. The cache has the access point of
/// an earlier join, so the start joins it and a scan is due.
fn cached_house() -> Shell {
    let mut shell = house();
    cache_hall(&mut shell.model);
    shell
}

/// Returns the number of times that `effect` is in `log`.
fn count(log: &[String], effect: &str) -> usize {
    log.iter().filter(|entry| *entry == effect).count()
}

/// Returns the place of the first `effect` in `log`. If `effect` is not in
/// `log`, the test fails.
fn place(log: &[String], effect: &str) -> usize {
    log.iter()
        .position(|entry| entry == effect)
        .unwrap_or_else(|| panic!("no {effect} in {log:?}"))
}

/// Connects with `refuse` as the filter that the broker refuses, and
/// returns the number of `Scan` requests after the SUBACK packets.
fn quiet_after_connect(refuse: Option<String>) -> usize {
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);
    cache_hall(&mut model);
    wire.broker.refuse = refuse;
    wire.connect(&mut model)
        .expect_write_with(&mut wire, &mut model, |_| {})
        .expect_only_render();
    wire.effects
        .iter()
        .filter(|effect| matches!(effect, Effect::RadioScan(_)))
        .count()
}

#[test]
fn only_an_answer_for_the_current_connection_makes_the_link_quiet() {
    assert_eq!(quiet_after_connect(None), 1);
    // The broker refused a filter, but the link is still quiet.
    let topic = topics(PAGES).swap_remove(0);
    assert_eq!(quiet_after_connect(Some(topic)), 1);

    // A SUBACK on the stream of an older connection does not reach the
    // session.
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let older = model.connection.id.wrapping_sub(1);
    let event = Event::Stream {
        connection: older,
        item: StreamItem::Received(alloc::vec![0x90, 0x03, 0x00, 0x01, 0x00]),
    };
    Dashboard.update(event, &mut model).expect_no_effects();
}

#[test]
fn the_link_is_quiet_after_the_subscribe_is_done() {
    let mut shell = cached_house();
    shell.broker.hold_suback = true;
    shell.send(start(PowerSource::Stand));
    // The subscribe waits, so the link is not quiet yet.
    assert_eq!(count(&shell.take_log(), "RadioScan"), 0);

    // The connection closes before the SUBACK, so the subscribe fails and
    // the link is not quiet. The next connection subscribes again, and the
    // answer makes the link quiet.
    shell.broker.drop_connection();
    shell.run();
    assert_eq!(count(&shell.take_log(), "RadioScan"), 0);
    shell.broker.hold_suback = false;
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(count(&log, "MqttSubscribe"), 1, "{log:?}");
    assert!(
        place(&log, "MqttSubscribe") < place(&log, "RadioScan"),
        "{log:?}"
    );
    assert_eq!(count(&log, "RadioScan"), 1, "{log:?}");
}

#[test]
fn a_wake_subscribes_again_and_then_the_link_is_quiet() {
    let mut shell = cached_house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    shell.take_log();

    shell.wake(Wake::Timer);
    let log = shell.take_log();
    let status = place(&log, "MqttPublish dashboard/hall/status online");
    let subscribe = place(&log, "MqttSubscribe");
    let quiet = place(&log, "RadioScan");
    assert!(status < subscribe, "{log:?}");
    assert!(subscribe < quiet, "{log:?}");
}

#[test]
fn the_link_is_not_quiet_during_the_sleep_steps() {
    let mut shell = cached_house();
    shell.broker.hold_suback = true;
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    // The subscribe ended with `Failed` at the disconnect of the sleep.
    assert_eq!(count(&shell.take_log(), "RadioScan"), 0);
}

#[test]
fn a_refused_filter_still_makes_the_link_quiet_once() {
    let mut shell = cached_house();
    shell.broker.refuse = Some(topics(PAGES).swap_remove(0));
    shell.send(start(PowerSource::Stand));
    let log = shell.take_log();
    assert_eq!(count(&log, "RadioScan"), 1, "{log:?}");
    assert!(
        place(&log, "MqttSubscribe") < place(&log, "RadioScan"),
        "{log:?}"
    );
}
