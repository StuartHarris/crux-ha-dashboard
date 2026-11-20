//! Tests of the MQTT session in the core, with the test shell and the fake
//! broker: dead links, the close for a sleep, refused connections, long
//! messages and a full session.

use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use super::{
    flows::house,
    shell::{Shell, session_timers},
    start, start_at, tap_tile,
};
use crate::{
    Event, PowerSource,
    capabilities::stream::{StreamItem, Written},
    features::{
        connection::{CLOSE_TIMEOUT, Link, RECONNECT_MAX, RECONNECT_MIN},
        light::LIGHT_TIMEOUT,
        pending::{PENDING_TIMEOUT, QUEUED_TIMEOUT, SCENE_TIMEOUT, TARGET_TIMEOUT},
        power::IDLE_TIMEOUT,
        refresh::{DRAW_DELAY, FULL_REFRESH_INTERVAL},
        wifi::{
            ADDRESS_TIMEOUT, JOIN_TIMEOUT, LEAVE_TIMEOUT, OFF_TIMEOUT, SCAN_TIMEOUT,
            STOP_SCAN_TIMEOUT,
        },
    },
    mqtt::{MAX_IN_FLIGHT, MAX_PACKET, SEND_TIMEOUT, codec::server::Packet},
};

/// The page of the living room. Tile 1 is a lamp.
const LIVING_ROOM: &str = "/room/living_room";

/// The status topic of the dashboard in the tests.
const STATUS: &str = "dashboard/hall/status";

/// Returns the place of the first entry in `log` that starts with `entry`.
#[track_caller]
fn place(log: &[String], entry: &str) -> usize {
    log.iter()
        .position(|logged| logged.starts_with(entry))
        .unwrap_or_else(|| panic!("{entry} in {log:?}"))
}

/// Starts the house on `power`, connects, and clears the log.
fn connected(power: PowerSource) -> Shell {
    let mut shell = house();
    shell.send(start_at(power, Some(LIVING_ROOM)));
    assert_eq!(shell.model.connection.mqtt, Link::Up);
    shell.take_log();
    shell
}

/// Returns the durations of the timers of the features, with each step of
/// the reconnect backoff.
fn feature_timers() -> Vec<Duration> {
    let mut features = vec![
        CLOSE_TIMEOUT,
        LIGHT_TIMEOUT,
        PENDING_TIMEOUT,
        TARGET_TIMEOUT,
        SCENE_TIMEOUT,
        QUEUED_TIMEOUT,
        IDLE_TIMEOUT,
        DRAW_DELAY,
        FULL_REFRESH_INTERVAL,
        JOIN_TIMEOUT,
        ADDRESS_TIMEOUT,
        LEAVE_TIMEOUT,
        SCAN_TIMEOUT,
        STOP_SCAN_TIMEOUT,
        OFF_TIMEOUT,
    ];
    let mut backoff = RECONNECT_MIN;
    while backoff < RECONNECT_MAX {
        features.push(backoff);
        backoff *= 2;
    }
    features.push(RECONNECT_MAX);
    features
}

#[test]
fn the_timers_of_the_session_have_durations_that_no_feature_timer_has() {
    let features = feature_timers();
    for duration in session_timers() {
        assert!(!features.contains(&duration), "{duration:?}");
    }
}

#[test]
fn a_broker_that_does_not_answer_the_ping_ends_the_session_in_55_seconds() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_pingresp = true;

    // The ping goes after 45 s, and its answer must arrive in 10 s.
    shell.advance_session(Duration::from_secs(54));
    assert_eq!(shell.model.connection.mqtt, Link::Up);
    shell.advance_session(Duration::from_secs(1));
    assert_ne!(shell.model.connection.mqtt, Link::Up);
    let close = shell.closes.last().expect("a Close request");
    assert!(!close.clean);
    assert!(close.reason.contains("ping"), "{}", close.reason);

    // The core connects again after the backoff.
    shell.broker.hold_pingresp = false;
    shell.take_log();
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    place(&log, "MqttConnect");
    assert_eq!(shell.model.connection.mqtt, Link::Up);
}

#[test]
fn a_sleep_sends_asleep_and_disconnect_then_closes_and_turns_wifi_off() {
    let mut shell = connected(PowerSource::Battery);
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);

    let log = shell.take_log();
    let asleep = place(&log, "MqttPublish dashboard/hall/status asleep");
    let disconnect = place(&log, "MqttDisconnect");
    let close = place(&log, "StreamClose");
    let wifi_off = place(&log, "WifiOff");
    assert!(
        asleep < disconnect && disconnect < close && close < wifi_off,
        "{log:?}"
    );
    assert!(shell.closes.last().unwrap().clean);

    // The broker got the DISCONNECT, so it does not publish the last will.
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));
    shell.advance_session(Duration::from_secs(120));
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));
}

#[test]
fn a_sleep_with_a_dead_link_still_turns_wifi_off_and_sleeps() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_writes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    assert!(!shell.take_log().contains(&"WifiOff".into()));

    // The write of `asleep` and the DISCONNECT does not end. The send timer
    // ends the session, and the sleep continues.
    shell.advance_session(SEND_TIMEOUT);
    let log = shell.take_log();
    assert!(
        place(&log, "StreamClose") < place(&log, "WifiOff"),
        "{log:?}"
    );
    place(&log, "Sleep");
    assert!(!shell.closes.last().unwrap().clean);
    // The broker did not get the DISCONNECT, so it published the last will.
    assert_eq!(shell.broker.retained(STATUS), Some("offline"));
}

#[test]
fn a_wake_during_the_close_connects_again_after_the_close() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_writes = true;
    shell.broker.hold_closes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);

    // A tap wakes the device while the DISCONNECT waits. The session of the
    // sleep still closes, so the core does not connect yet.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.asleep);
    shell.take_log();

    // The DISCONNECT leaves, and the core closes the stream. The core does
    // not open a new stream before the answer to the `Close` request.
    shell.broker.release_writes();
    shell.run();
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert!(shell.closes.last().unwrap().clean);
    // The broker got the DISCONNECT, so it does not publish the last will.
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));

    // The answer arrives, and the core connects again.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    place(&log, "MqttConnect");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert_eq!(shell.model.connection.mqtt, Link::Up);
    assert!(!shell.model.power.asleep);
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
}

#[test]
fn a_tap_during_the_sleep_close_connects_after_the_answer_to_the_close() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");

    // The DISCONNECT left, and the `Close` request waits. A tap wakes the
    // device, but the core does not open a new stream yet.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.asleep);
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));

    // The answer arrives. The late step of the sleep does nothing, and the
    // core connects.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    place(&log, "MqttConnect");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert_eq!(shell.model.connection.mqtt, Link::Up);
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
}

#[test]
fn after_a_failure_the_core_connects_after_the_backoff_and_the_answer_to_the_close() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_pingresp = true;
    shell.broker.hold_closes = true;
    shell.advance_session(Duration::from_secs(55));
    assert_ne!(shell.model.connection.mqtt, Link::Up);
    place(&shell.take_log(), "StreamClose");

    // The backoff ends before the answer to the `Close` request, so the core
    // does not connect yet.
    shell.broker.hold_pingresp = false;
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");

    // The answer arrives, and the core connects.
    shell.broker.release_closes();
    shell.run();
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.mqtt, Link::Up);
}

#[test]
fn after_a_failure_the_answer_to_the_close_does_not_skip_the_backoff() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_pingresp = true;
    shell.advance_session(Duration::from_secs(55));
    assert_ne!(shell.model.connection.mqtt, Link::Up);
    // The shell answered the `Close` request at once, but the core waits
    // for the backoff.
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");

    shell.broker.hold_pingresp = false;
    shell.advance(RECONNECT_MIN);
    place(&shell.take_log(), "MqttConnect");
}

#[test]
fn a_close_with_no_answer_lets_the_core_connect_after_the_close_timeout() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.advance(IDLE_TIMEOUT);
    // The idle timer fired at most one draw delay before this time.
    let closed_at = shell.now();
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.asleep);
    shell.take_log();

    // The shell does not answer the `Close` request. At the timeout, the
    // core acts as if the answer arrived, and connects.
    let deadline = closed_at + CLOSE_TIMEOUT;
    let early = deadline.saturating_sub(DRAW_DELAY + Duration::from_millis(1));
    shell.advance(early.saturating_sub(shell.now()));
    assert!(!shell.take_log().contains(&"MqttConnect".into()));
    shell.advance(deadline.saturating_sub(shell.now()));
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.mqtt, Link::Up);

    // The late answer does nothing.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert_eq!(shell.model.connection.mqtt, Link::Up);
}

#[test]
fn after_a_failure_a_close_with_no_answer_delays_the_connect_to_the_close_timeout() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    shell.broker.drop_connection();
    shell.run();
    assert_ne!(shell.model.connection.mqtt, Link::Up);
    place(&shell.take_log(), "StreamClose");

    // The backoff ends first. The core waits for the timeout of the close.
    shell.advance(CLOSE_TIMEOUT.saturating_sub(Duration::from_millis(1)));
    assert!(!shell.take_log().contains(&"MqttConnect".into()));
    shell.advance(Duration::from_millis(1));
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.mqtt, Link::Up);

    // The late answer does nothing.
    shell.broker.release_closes();
    shell.run();
    assert!(!shell.take_log().contains(&"MqttConnect".into()));
}

#[test]
fn a_sleep_continues_after_the_close_timeout_if_the_close_has_no_answer() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.asleep);
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");

    // At the timeout, the core acts as if the answer arrived.
    shell.advance(CLOSE_TIMEOUT);
    let log = shell.take_log();
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"), "{log:?}");
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));

    // The late answer does not do the steps of the sleep again.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert!(!log.contains(&"Sleep".into()), "{log:?}");
}

#[test]
fn a_refused_connack_closes_the_stream_and_the_core_tries_again_after_the_backoff() {
    let mut shell = house();
    shell.broker.code = 5;
    shell.send(start(PowerSource::Stand));
    let close = shell.closes.last().expect("a Close request");
    assert!(!close.clean);
    assert!(close.reason.contains("not authorized"), "{}", close.reason);
    assert_ne!(shell.model.connection.mqtt, Link::Up);

    shell.broker.code = 0;
    shell.take_log();
    shell.advance(RECONNECT_MIN);
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.mqtt, Link::Up);
}

#[test]
fn a_message_that_is_too_long_is_skipped_and_the_connection_stays() {
    let mut shell = connected(PowerSource::Stand);
    let topic = "homeassistant/statestream/light/all_lights/state";
    shell
        .broker
        .publish(topic, &vec![b'x'; MAX_PACKET + 1], false);
    shell.run();
    assert_eq!(shell.model.connection.skipped, 1);
    assert_eq!(shell.model.connection.mqtt, Link::Up);
    assert_eq!(shell.closes, []);
    assert_eq!(
        shell.take_logs(),
        [format!(
            "warn mqtt: a message of {} bytes is too long, so the core skipped it",
            MAX_PACKET + 1 + topic.len() + 2
        )]
    );

    // The next message arrives.
    shell.broker.set_state("light.all_lights", "off", &[]);
    shell.run();
    assert_eq!(
        shell
            .model
            .entities
            .get("light.all_lights")
            .unwrap()
            .state
            .as_deref(),
        Some("off")
    );
}

#[test]
fn a_call_that_the_full_session_refuses_waits_and_goes_after_the_pubacks() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_puback = true;
    for _ in 0..MAX_IN_FLIGHT {
        shell.send(tap_tile(1));
    }
    assert!(shell.model.connection.outbox.is_empty());

    // The session has the most publishes that wait for a PUBACK, so the
    // next call waits first in the outbox.
    shell.send(tap_tile(1));
    assert_eq!(shell.model.connection.outbox.len(), 1);

    // The PUBACK packets arrive, and the call goes.
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert!(shell.model.connection.outbox.is_empty());
    assert_eq!(shell.broker.service_calls.len(), MAX_IN_FLIGHT + 1);
}

#[test]
fn the_outbox_the_status_and_the_subscribe_go_in_one_write() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start_at(PowerSource::Stand, Some(LIVING_ROOM)));
    shell.send(tap_tile(1));
    assert_eq!(shell.model.connection.outbox.len(), 1);

    // The tap asked for a connection, which the broker refused too, so the
    // backoff is now twice as long.
    shell.broker.down = false;
    shell.writes.clear();
    shell.advance(RECONNECT_MIN * 2);
    // The first write is the CONNECT. The second is the burst after the
    // CONNACK.
    let burst = &shell.writes[1];
    let Packet::Publish { topic, .. } = &burst[0] else {
        panic!("the call first: {burst:?}");
    };
    assert_eq!(topic, "dashboard/hall/cmd");
    let Packet::Publish { topic, payload, .. } = &burst[1] else {
        panic!("the status second: {burst:?}");
    };
    assert_eq!(topic, STATUS);
    assert_eq!(payload, b"online");
    assert!(
        burst[2..]
            .iter()
            .all(|packet| matches!(packet, Packet::Subscribe { .. })),
        "{burst:?}"
    );
}

#[test]
fn an_answer_or_a_timer_of_an_older_connection_is_ignored() {
    let mut shell = connected(PowerSource::Stand);
    let events = shell.advance_session(Duration::from_secs(45));
    let timer = events
        .into_iter()
        .find(|event| matches!(event, Event::SessionTimer { .. }))
        .expect("the ping timer");
    let old = shell.model.connection.id;

    // The connection drops, and the core connects again.
    shell.broker.drop_connection();
    shell.run();
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.model.connection.mqtt, Link::Up);
    assert_ne!(shell.model.connection.id, old);
    shell.take_log();

    let stale: Vec<Event> = vec![
        timer,
        Event::Written {
            connection: old,
            outcome: Written::Sent,
        },
        Event::Stream {
            connection: old,
            item: StreamItem::closed("late"),
        },
    ];
    for event in stale {
        shell.send(event);
    }
    assert_eq!(shell.take_log(), Vec::<String>::new());
    assert_eq!(shell.model.connection.mqtt, Link::Up);
}

#[cfg(feature = "fixtures")]
#[test]
fn the_fixed_shell_stays_online_when_all_its_timers_fire() {
    use crate::{Connection, fixtures::FixedShell};

    let mut shell = FixedShell::start(PowerSource::Stand, LIVING_ROOM);
    assert_eq!(shell.view().connection, Connection::Online);
    // Each round fires the ping, silence and acknowledgement timers too.
    for _ in 0..5 {
        shell.fire_timers();
        assert_eq!(shell.view().connection, Connection::Online);
    }
}
