//! Tests of the MQTT session in the core, with the test shell and the fake
//! broker: dead links, the close for a sleep, refused connections, long
//! messages and a full session.

use crate::features::connection;
use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use crux_core::App;

use super::{
    flows::house,
    shell::{Shell, session_timers},
    start, start_at, tap_tile,
};
use crate::{
    Connection, Dashboard, Event, PowerSource,
    capabilities::{
        power::Wake,
        radio::Target,
        stream::{StreamItem, Written},
    },
    entities::topics,
    fake::radio::HALL,
    features::{
        connection::{CLOSE_TIMEOUT, Link, RECONNECT_MAX, RECONNECT_MIN},
        device::BIRTH_TOPIC,
        light::LIGHT_TIMEOUT,
        pending::{PENDING_TIMEOUT, QUEUED_TIMEOUT, SCENE_TIMEOUT, TARGET_TIMEOUT},
        power::{self, IDLE_TIMEOUT},
        refresh::{DRAW_DELAY, FULL_REFRESH_INTERVAL},
        wifi::{
            ADDRESS_TIMEOUT, JOIN_TIMEOUT, LEAVE_TIMEOUT, OFF_TIMEOUT, SCAN_TIMEOUT,
            STOP_SCAN_TIMEOUT,
        },
    },
    mqtt::{
        MAX_IN_FLIGHT, MAX_PACKET, SEND_TIMEOUT,
        codec::server::{Packet, suback},
    },
    pages::PAGES,
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
    assert_eq!(shell.model.connection.link(), Link::Up);
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
    assert_eq!(shell.model.connection.link(), Link::Up);
    shell.advance_session(Duration::from_secs(1));
    assert_ne!(shell.model.connection.link(), Link::Up);
    let close = shell.closes.last().expect("a Close request");
    assert!(!close.clean);
    assert!(close.reason.contains("ping"), "{}", close.reason);

    // The core connects again after the backoff.
    shell.broker.hold_pingresp = false;
    shell.take_log();
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    place(&log, "MqttConnect");
    assert_eq!(shell.model.connection.link(), Link::Up);
}

#[test]
fn a_sleep_sends_asleep_and_disconnect_then_closes_and_turns_wifi_off() {
    let mut shell = connected(PowerSource::Battery);
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());

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
    assert!(shell.model.power.is_asleep());
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
    assert!(shell.model.power.is_asleep());

    // A tap wakes the device while the DISCONNECT waits. The session of the
    // sleep still closes, so the core does not connect yet.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
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
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
}

#[test]
fn a_tap_during_the_sleep_close_connects_after_the_answer_to_the_close() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");

    // The DISCONNECT left, and the `Close` request waits. A tap wakes the
    // device, but the core does not open a new stream yet.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
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
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
}

#[test]
fn after_a_failure_the_core_connects_after_the_backoff_and_the_answer_to_the_close() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_pingresp = true;
    shell.broker.hold_closes = true;
    shell.advance_session(Duration::from_secs(55));
    assert_ne!(shell.model.connection.link(), Link::Up);
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
    assert_eq!(shell.model.connection.link(), Link::Up);
}

#[test]
fn after_a_failure_the_answer_to_the_close_does_not_skip_the_backoff() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_pingresp = true;
    shell.advance_session(Duration::from_secs(55));
    assert_ne!(shell.model.connection.link(), Link::Up);
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
    assert!(!shell.model.power.is_asleep());
    shell.take_log();

    // The shell does not answer the `Close` request. At the timeout, the
    // core acts as if the answer arrived, and connects.
    let deadline = closed_at + CLOSE_TIMEOUT;
    let early = deadline.saturating_sub(DRAW_DELAY + Duration::from_millis(1));
    shell.advance(early.saturating_sub(shell.now()));
    assert!(!shell.take_log().contains(&"MqttConnect".into()));
    shell.advance(deadline.saturating_sub(shell.now()));
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.link(), Link::Up);

    // The late answer does nothing.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert_eq!(shell.model.connection.link(), Link::Up);
}

#[test]
fn after_a_failure_a_close_with_no_answer_delays_the_connect_to_the_close_timeout() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    shell.broker.drop_connection();
    shell.run();
    assert_ne!(shell.model.connection.link(), Link::Up);
    place(&shell.take_log(), "StreamClose");

    // The backoff ends first. The core waits for the timeout of the close.
    shell.advance(CLOSE_TIMEOUT.saturating_sub(Duration::from_millis(1)));
    assert!(!shell.take_log().contains(&"MqttConnect".into()));
    shell.advance(Duration::from_millis(1));
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.link(), Link::Up);

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
    assert!(shell.model.power.is_asleep());
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
    assert_ne!(shell.model.connection.link(), Link::Up);

    shell.broker.code = 0;
    shell.take_log();
    shell.advance(RECONNECT_MIN);
    place(&shell.take_log(), "MqttConnect");
    assert_eq!(shell.model.connection.link(), Link::Up);
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
    assert_eq!(shell.model.connection.link(), Link::Up);
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

/// The write after the CONNACK holds the call at `QoS` 1 with the packet id
/// 1, then the retained `online` status with the id 2, then the SUBSCRIBE
/// packets with all the topics at `QoS` 0, in order. The last topic is the
/// birth message of Home Assistant.
#[test]
fn the_write_after_the_connack_keeps_its_packets_and_their_ids() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start_at(PowerSource::Stand, Some(LIVING_ROOM)));
    shell.send(tap_tile(1));
    shell.broker.down = false;
    shell.writes.clear();
    shell.advance(RECONNECT_MIN * 2);

    let burst = &shell.writes[1];
    let [call, status, subscribes @ ..] = &burst[..] else {
        panic!("a call, a status and subscribes: {burst:?}");
    };
    let Packet::Publish {
        flags,
        topic,
        id,
        payload,
    } = call
    else {
        panic!("the call first: {burst:?}");
    };
    assert_eq!(
        (*flags, topic.as_str(), *id),
        (0x02, "dashboard/hall/cmd", Some(1))
    );
    assert_eq!(
        payload,
        br#"{"service":"light.toggle","entity_id":"light.living_room_rocket_lamp"}"#
    );
    assert_eq!(
        *status,
        Packet::Publish {
            flags: 0x03,
            topic: STATUS.into(),
            id: Some(2),
            payload: b"online".to_vec(),
        }
    );
    let mut filters = Vec::new();
    for (packet, expected_id) in subscribes.iter().zip(3..) {
        let Packet::Subscribe { id, filters: more } = packet else {
            panic!("then the subscribes: {burst:?}");
        };
        assert_eq!(*id, expected_id);
        filters.extend(more.iter().cloned());
    }
    // The filters of the entities, and then the birth message of Home
    // Assistant. The write has nothing else.
    let mut expected: Vec<(String, u8)> =
        topics(PAGES).into_iter().map(|topic| (topic, 0)).collect();
    expected.push((BIRTH_TOPIC.into(), 0));
    assert_eq!(filters, expected);
}

#[test]
fn an_answer_or_a_timer_of_an_older_connection_is_ignored() {
    let mut shell = connected(PowerSource::Stand);
    let events = shell.advance_session(Duration::from_secs(45));
    let timer = events
        .into_iter()
        .find(|event| {
            matches!(
                event,
                Event::Connection(connection::Event::SessionTimer { .. })
            )
        })
        .expect("the ping timer");
    let old = shell.model.connection.id;

    // The connection drops, and the core connects again.
    shell.broker.drop_connection();
    shell.run();
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_ne!(shell.model.connection.id, old);
    shell.take_log();

    let stale: Vec<Event> = vec![
        timer,
        Event::Connection(connection::Event::Written {
            connection: old,
            outcome: Written::Sent,
        }),
        Event::Connection(connection::Event::Stream {
            connection: old,
            item: StreamItem::closed("late"),
        }),
    ];
    for event in stale {
        shell.send(event);
    }
    assert_eq!(shell.take_log(), Vec::<String>::new());
    assert_eq!(shell.model.connection.link(), Link::Up);
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

#[test]
fn only_the_first_message_that_is_too_long_on_a_connection_has_a_line() {
    let mut shell = connected(PowerSource::Stand);
    shell.take_logs();
    let topic = "homeassistant/statestream/light/all_lights/state";
    let too_long = vec![b'x'; MAX_PACKET + 1];
    for _ in 0..2 {
        shell.broker.publish(topic, &too_long, false);
        shell.run();
    }
    // The core counts both messages, and writes one line.
    assert_eq!(shell.model.connection.skipped, 2);
    let lines = shell.take_logs();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("warn mqtt: a message of "),
        "{lines:?}"
    );

    // A new connection writes a line again.
    shell.broker.drop_connection();
    shell.run();
    shell.advance(RECONNECT_MIN * 2);
    assert_eq!(shell.model.connection.link(), Link::Up);
    shell.take_logs();
    shell.broker.publish(topic, &too_long, false);
    shell.run();
    assert_eq!(shell.model.connection.skipped, 3);
    let lines = shell.take_logs();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("warn mqtt: a message of "),
        "{lines:?}"
    );
}

/// Returns `true` if the view shows that the dashboard is online.
fn online(shell: &Shell) -> bool {
    shell.view().connection == Connection::Online
}

/// Returns `true` if `log` has a radio request.
fn has_radio(log: &[String]) -> bool {
    log.iter()
        .any(|effect| effect.starts_with("Radio") || effect == "WifiOff")
}

/// Returns the house with the access point [`HALL`] in the cache, so that a
/// scan for a better access point is due when the link is quiet.
fn cached_house() -> Shell {
    let mut shell = house();
    shell.model.wifi.cache.joined(Target::Cached(HALL), HALL);
    shell
}

/// Returns the SUBACK packets for each SUBSCRIBE packet that the shell
/// wrote, with no refused filter.
fn subacks(shell: &Shell) -> Vec<u8> {
    let mut bytes = Vec::new();
    for packet in shell.writes.iter().flatten() {
        if let Packet::Subscribe { id, filters } = packet {
            suback(&mut bytes, *id, &vec![0; filters.len()]);
        }
    }
    assert!(!bytes.is_empty(), "a SUBSCRIBE in {:?}", shell.writes);
    bytes
}

/// Gives `bytes` from the broker to the core on the current connection.
fn receive(shell: &mut Shell, bytes: Vec<u8>) {
    let connection = shell.model.connection.id;
    shell.send(Event::Connection(connection::Event::Stream {
        connection,
        item: StreamItem::Received(bytes),
    }));
}

/// During the close for a sleep, a state message does not change
/// the entities.
#[test]
fn a_message_during_the_close_for_a_sleep_does_not_change_the_entities() {
    let mut shell = connected(PowerSource::Battery);
    let lamp = "light.living_room_rocket_lamp";
    let state = |shell: &Shell| shell.model.entities.get(lamp).unwrap().state.clone();
    shell.broker.set_state(lamp, "on", &[]);
    shell.run();
    assert_eq!(state(&shell).as_deref(), Some("on"));

    // The DISCONNECT waits, so the session stays open in its close.
    shell.broker.hold_writes = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    place(&shell.take_log(), "MqttDisconnect");

    shell.broker.set_state(lamp, "off", &[]);
    shell.run();
    assert_eq!(state(&shell).as_deref(), Some("on"));
}

/// A subscribe that is done makes the link quiet while the link is up.
/// A scan for a better access point then starts.
#[test]
fn a_subscribe_that_is_done_while_the_link_is_up_makes_the_link_quiet() {
    let mut shell = cached_house();
    shell.broker.hold_suback = true;
    shell.send(start_at(PowerSource::Battery, Some(LIVING_ROOM)));
    assert!(online(&shell));
    assert!(!shell.take_log().contains(&"RadioScan".into()));

    let bytes = subacks(&shell);
    receive(&mut shell, bytes);
    assert!(shell.take_log().contains(&"RadioScan".into()));
}

/// A subscribe that is done after the link went down does not make the
/// link quiet. The device woke during the close for a sleep, so the
/// core wants a connection, but it does not scan.
#[test]
fn a_subscribe_that_is_done_during_the_close_for_a_sleep_does_not_make_the_link_quiet() {
    let mut shell = cached_house();
    shell.broker.hold_suback = true;
    shell.send(start_at(PowerSource::Battery, Some(LIVING_ROOM)));
    assert!(online(&shell));

    // The DISCONNECT waits. A tap wakes the device during the close.
    shell.broker.hold_writes = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    assert!(!online(&shell));
    shell.take_log();

    let bytes = subacks(&shell);
    receive(&mut shell, bytes);
    let log = shell.take_log();
    assert!(!log.contains(&"RadioScan".into()), "{log:?}");
}

/// A call that the full session refuses goes to the front of the
/// outbox, ahead of an older call that waits.
#[test]
fn a_call_that_the_full_session_refuses_goes_ahead_of_an_older_call_that_waits() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_puback = true;
    for _ in 0..MAX_IN_FLIGHT {
        shell.send(tap_tile(1));
    }
    assert!(shell.model.connection.outbox.is_empty());

    // Tile 0 is the light group, and tile 2 is the standard lamp.
    let group = "light.living_room_lights";
    let lamp = "light.living_room_standard_lamp";
    shell.send(tap_tile(0));
    shell.send(tap_tile(2));
    let outbox: Vec<&str> = shell
        .model
        .connection
        .outbox
        .iter()
        .map(|call| call.entity_id)
        .collect();
    assert_eq!(outbox, [lamp, group]);

    // The PUBACK packets arrive, and the calls go in the order of the
    // outbox.
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert!(shell.model.connection.outbox.is_empty());
    let last: Vec<&str> = shell.broker.service_calls[MAX_IN_FLIGHT..]
        .iter()
        .map(|call| call.entity_id.as_str())
        .collect();
    assert_eq!(last, [lamp, group]);
}

/// A new `Start` forgets the stream that drains and clears its close
/// timer. A late answer to the `Close` request or a late close timeout then
/// does nothing.
#[test]
fn a_start_forgets_the_close_that_waits() {
    let mut shell = connected(PowerSource::Battery);
    let old = shell.model.connection.id;
    shell.broker.hold_closes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");

    // A tap wakes the device while the `Close` request waits. The core
    // connects only after the answer.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    assert!(!online(&shell));

    // The shell starts again. The core connects at once.
    shell.take_log();
    shell.send(start_at(PowerSource::Battery, Some(LIVING_ROOM)));
    place(&shell.take_log(), "MqttConnect");
    assert!(online(&shell));

    // A late answer or a late close timeout for the old connection gives no
    // effects and no events, so also no step of the sleep.
    for event in [
        connection::Event::StreamClosed { connection: old },
        connection::Event::CloseTimeout { connection: old },
    ] {
        Dashboard
            .update(Event::Connection(event), &mut shell.model)
            .expect_no_effect_or_events();
    }
    assert!(online(&shell));

    // The close timer does not fire.
    let events = shell.advance(CLOSE_TIMEOUT);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Connection(connection::Event::CloseTimeout { .. })
        )),
        "{events:?}"
    );

    // The late answer does nothing.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert!(!has_radio(&log), "{log:?}");
    assert!(!log.contains(&"Sleep".into()), "{log:?}");
    assert!(online(&shell));
}

/// After a failure, a service call while the old stream drains makes the
/// core connect at the answer to the `Close` request, before the backoff
/// ends.
#[test]
fn a_call_while_the_stream_drains_after_a_failure_connects_at_the_answer_to_the_close() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    let failed_at = shell.now();
    shell.broker.drop_connection();
    shell.run();
    assert!(!online(&shell));
    place(&shell.take_log(), "StreamClose");

    // The call waits. The core does not connect and makes no radio request
    // yet.
    shell.send(tap_tile(1));
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert!(!has_radio(&log), "{log:?}");
    assert_eq!(shell.model.connection.outbox.len(), 1);

    // The answer arrives before the backoff ends, and the core connects.
    shell.broker.release_closes();
    shell.run();
    place(&shell.take_log(), "MqttConnect");
    assert!(online(&shell));
    assert!(shell.now() < failed_at + RECONNECT_MIN);
    assert!(shell.model.connection.outbox.is_empty());

    // The end of the backoff does not connect again.
    shell.advance(RECONNECT_MAX);
    let log = shell.take_log();
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert!(online(&shell));
}

/// A sleep while the stream opens ends the session at once. The
/// core writes no DISCONNECT, and sends a clean `Close` for the connection.
/// The next step of the sleep waits for the answer to the `Close` request.
#[test]
fn a_sleep_before_the_connack_closes_the_stream_with_no_disconnect() {
    let mut shell = house();
    // The CONNECT does not leave, so the session waits for its CONNACK.
    shell.broker.hold_writes = true;
    shell.send(start_at(PowerSource::Battery, Some(LIVING_ROOM)));
    assert_eq!(shell.view().connection, Connection::Connecting);
    let opening = shell.model.connection.id;
    let writes = shell.writes.len();
    shell.take_log();

    shell.broker.hold_closes = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let log = shell.take_log();
    assert_eq!(shell.writes.len(), writes, "{log:?}");
    assert!(!log.contains(&"MqttDisconnect".into()), "{log:?}");
    place(&log, "StreamClose");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    let close = shell.closes.last().unwrap();
    assert_eq!(close.connection, opening);
    assert!(close.clean);

    // The answer arrives, and the sleep continues.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"), "{log:?}");
}

/// A wake during the close for a sleep, and then a failure end,
/// connects after the backoff. The device does not sleep.
#[test]
fn a_failure_after_a_wake_during_the_close_for_a_sleep_connects_after_the_backoff() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_writes = true;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    shell.take_log();

    // The DISCONNECT does not leave, so the send timer ends the session.
    shell.broker.hold_writes = false;
    shell.advance_session(SEND_TIMEOUT);
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!shell.closes.last().unwrap().clean);
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert!(!log.contains(&"Sleep".into()), "{log:?}");

    // The core connects after the backoff.
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    place(&log, "MqttConnect");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert!(!log.contains(&"Sleep".into()), "{log:?}");
    assert!(online(&shell));
    assert!(!shell.model.power.is_asleep());
    assert!(shell.sleep.is_none());
}

/// With no stream, a sleep turns the radio off at once. The core sends no
/// `Close` request.
#[test]
fn a_sleep_with_no_stream_turns_wifi_off_at_once_with_no_close() {
    let mut shell = house();
    // Each join fails, so no stream opens.
    shell.radio.blocked = true;
    shell.send(start_at(PowerSource::Battery, Some(LIVING_ROOM)));
    assert_eq!(shell.view().connection, Connection::Connecting);
    shell.take_log();

    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let log = shell.take_log();
    assert!(!log.contains(&"StreamClose".into()), "{log:?}");
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"), "{log:?}");
    assert_eq!(shell.closes, []);
    assert!(shell.sleep.is_some());
}

/// A wake after a sleep with no stream connects at once.
#[test]
fn a_wake_after_a_sleep_with_no_stream_connects() {
    let mut shell = house();
    shell.radio.blocked = true;
    shell.send(start_at(PowerSource::Battery, Some(LIVING_ROOM)));
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    shell.take_log();

    shell.radio.blocked = false;
    shell.wake(Wake::Touch(None));
    assert!(!shell.model.power.is_asleep());
    let log = shell.take_log();
    place(&log, "MqttConnect");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert!(online(&shell));
    assert_eq!(shell.closes, []);
}

/// A sleep while the old stream drains after a failure waits for the
/// answer to that `Close` request. The core sends no other `Close`
/// request, and stops the reconnect timer.
#[test]
fn a_sleep_while_the_stream_drains_waits_for_the_answer_to_its_close() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.broker.drop_connection();
    shell.run();
    assert!(!online(&shell));
    place(&shell.take_log(), "StreamClose");
    assert_eq!(shell.closes.len(), 1);

    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let log = shell.take_log();
    assert!(!log.contains(&"StreamClose".into()), "{log:?}");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert_eq!(shell.closes.len(), 1);

    // The reconnect timer does not fire, and the radio stays on.
    let events = shell.advance(RECONNECT_MIN);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_)))),
        "{events:?}"
    );
    let log = shell.take_log();
    assert!(!has_radio(&log), "{log:?}");
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");

    // The answer arrives, and the sleep continues.
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"), "{log:?}");
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
    assert_eq!(shell.closes.len(), 1);
}

/// Drives the shell to a sleep whose `Close` request waits for its answer.
/// Returns the shell and the id of the connection of the `Close` request.
fn a_sleep_that_waits_for_its_close() -> (Shell, u32) {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let log = shell.take_log();
    place(&log, "StreamClose");
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    let close = shell.closes.last().unwrap().connection;
    (shell, close)
}

/// Returns the events of the update for `event`, and expects no effects.
#[track_caller]
fn events_of(shell: &mut Shell, event: connection::Event) -> Vec<Event> {
    let mut cmd = Dashboard.update(Event::Connection(event), &mut shell.model);
    cmd.expect_no_effects();
    cmd.events().collect()
}

/// The sleep goes on only at the answer to the `Close` request for
/// which it waits. An answer or a close timeout for another connection
/// does nothing.
#[test]
fn a_sleep_goes_on_only_at_the_answer_for_its_own_close() {
    let (mut shell, close) = a_sleep_that_waits_for_its_close();
    let sleep = shell.model.power.sleep;

    for event in [
        connection::Event::StreamClosed {
            connection: close.wrapping_add(1),
        },
        connection::Event::CloseTimeout {
            connection: close.wrapping_sub(1),
        },
    ] {
        assert_eq!(events_of(&mut shell, event), []);
    }

    let events = events_of(
        &mut shell,
        connection::Event::StreamClosed { connection: close },
    );
    assert_eq!(events, [Event::Power(power::Event::Offline(sleep))]);
}

/// A second sleep, after a wake during the close for the first sleep,
/// replaces the sleep that waits. The answer continues only the second
/// sleep, and the core sends no other `Close` request.
#[test]
fn a_second_sleep_replaces_the_sleep_that_waits_for_the_close() {
    let (mut shell, close) = a_sleep_that_waits_for_its_close();
    let first = shell.model.power.sleep;

    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let second = shell.model.power.sleep;
    assert_ne!(first, second);
    let log = shell.take_log();
    assert!(!log.contains(&"StreamClose".into()), "{log:?}");
    assert_eq!(shell.closes.len(), 1);

    // The answer continues the second sleep, and the core does not connect.
    let events = events_of(
        &mut shell,
        connection::Event::StreamClosed { connection: close },
    );
    assert_eq!(events, [Event::Power(power::Event::Offline(second))]);
    for event in events {
        shell.send(event);
    }
    let log = shell.take_log();
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"), "{log:?}");
    assert!(!log.contains(&"MqttConnect".into()), "{log:?}");
}
