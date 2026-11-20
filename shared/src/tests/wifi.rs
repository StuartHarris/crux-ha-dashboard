//! Flow tests of the Wi-Fi policy: the core runs in the test shell, with the
//! fake radio and the fake broker.
//!
//! Each radio request has a time limit, so a `TimeNotifyAfter` line follows
//! each request in the log. A test that needs a request that waits sets
//! [`Shell::hold_radio`].

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use crux_core::App;

use super::{flows::house, settings, shell::Shell, start};
use crate::{
    Connection, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource,
    capabilities::{
        log::Level,
        power::Wake,
        radio::{Joined, Lease, Scanned, Seen, Target},
    },
    fake::radio::{HALL, LANDING},
    features::{
        connection::{RECONNECT_MAX, RECONNECT_MIN},
        power::{self, IDLE_TIMEOUT},
        wifi::{
            ADDRESS_TIMEOUT, JOIN_TIMEOUT, LEAVE_TIMEOUT, OFF_TIMEOUT, Phase, SCAN_TIMEOUT,
            STOP_SCAN_TIMEOUT,
            rules::{Health, RESTART_AFTER, Recovery},
        },
    },
    mqtt::codec::server::Packet,
};

/// The status topic of the dashboard in the tests.
const STATUS: &str = "dashboard/hall/status";

/// The log line of a session that failed because the fake broker dropped
/// the connection. Refer to [`fail_the_session`].
const SUSPECT: &str = "warn wifi: the link is suspect, because the MQTT session failed: the stream closed: connection lost";

/// The log line of a rejoin from [`HALL`] while the link is suspect.
const LEAVE_HALL: &str =
    "warn wifi: the link is suspect, so leave 02:00:00:00:00:01 on channel 6 and join again";

/// The log line of a join to [`HALL`] from the cache that failed.
const JOIN_TO_HALL_FAILED: &str = "warn wifi: the join to the cached access point \
    02:00:00:00:00:01 on channel 6 failed, so join the strongest access point";

/// Returns the log line after `count` joins with no new address while the
/// link is suspect.
fn no_new_address(count: u32) -> String {
    format!(
        "warn wifi: {count} of {RESTART_AFTER} joins got no new address while the link is suspect"
    )
}

/// Returns the log line of a reconnect that the core schedules after `secs`
/// seconds.
fn connect_again(secs: u32) -> String {
    format!("info mqtt: connect again in {secs} s")
}

/// Returns the radio entries of `log` in order: the radio requests,
/// `WifiOff` and `RadioRestart`.
fn radio(log: &[String]) -> Vec<&str> {
    log.iter()
        .map(String::as_str)
        .filter(|entry| entry.starts_with("Radio") || *entry == "WifiOff")
        .collect()
}

/// Returns the place of the first `entry` in `log`. If `entry` is not in
/// `log`, the test fails.
#[track_caller]
fn place(log: &[String], entry: &str) -> usize {
    log.iter()
        .position(|logged| logged == entry)
        .unwrap_or_else(|| panic!("no {entry} in {log:?}"))
}

/// Starts the house on the stand, connects, and clears the log.
fn online() -> Shell {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.take_log();
    shell
}

/// Starts the house on battery, connects, sleeps at the idle timeout, and
/// clears the log. The cache has [`HALL`], so the next join goes there.
fn asleep() -> Shell {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.take_log();
    shell
}

/// Ends the connection as when the link fails, so the link is suspect.
fn fail_the_session(shell: &mut Shell) {
    shell.broker.drop_connection();
    shell.run();
    assert!(shell.model.wifi.health.suspect());
}

/// Wakes the device from [`asleep`] while the shell holds the radio
/// requests, and answers each one until the scan for a better access point
/// waits.
fn scanning() -> Shell {
    let mut shell = asleep();
    shell.hold_radio = true;
    shell.wake(Wake::Timer);
    shell.answer_radio();
    shell.answer_radio();
    assert_eq!(shell.model.wifi.phase, Phase::Scanning(HALL));
    assert_eq!(shell.held_radio(), 1);
    shell.take_log();
    shell
}

#[test]
fn the_first_start_joins_the_strongest_access_point_and_does_not_scan() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));

    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Strongest", "RadioAddress"]);
    let join = place(&log, "RadioJoin Strongest");
    assert_eq!(log[join + 1], "TimeNotifyAfter 25000ms");
    let address = place(&log, "RadioAddress");
    assert_eq!(log[address + 1], "TimeNotifyAfter 25000ms");
    assert!(address < place(&log, "MqttConnect"), "{log:?}");

    assert_eq!(shell.radio.associated, Some(HALL));
    assert_eq!(shell.model.wifi.phase, Phase::Up(HALL));
    // The join to the strongest access point found the best one, so no
    // scan is due.
    assert_eq!(shell.model.wifi.cache.plan(), Target::Cached(HALL));
    assert!(!shell.model.wifi.cache.scan_due());
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn the_usual_steps_write_no_line_to_the_log() {
    // The first start joins the strongest access point, gets a new address,
    // connects and subscribes. The idle timer puts the device to sleep.
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert_eq!(shell.take_logs(), Vec::<String>::new());
}

#[test]
fn a_stop_of_the_broker_writes_only_the_waits_of_the_reconnects() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.take_log();
    shell.take_logs();

    // The broker stops and starts again. Each reconnect keeps the
    // association.
    shell.broker.stop();
    shell.run();
    shell.advance(RECONNECT_MIN);
    shell.broker.down = false;
    shell.advance(RECONNECT_MIN * 2);
    assert_eq!(shell.view().connection, Connection::Online);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        [
            "RadioJoin Cached keep",
            "RadioAddress",
            "RadioJoin Cached keep",
            "RadioAddress",
        ]
    );

    // The idle timer puts the device to sleep.
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());

    // Each failure of the broker host schedules a reconnect, so the core
    // writes the wait. The other steps write no line.
    assert_eq!(shell.take_logs(), [connect_again(1), connect_again(2)]);
}

#[test]
fn a_wake_joins_the_cached_access_point_and_stays_if_the_scan_finds_less_than_the_margin() {
    let mut shell = asleep();
    // The other access point is stronger, but by less than the margin.
    shell.radio.access_points[1].rssi = -46;

    shell.wake(Wake::Timer);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioJoin Cached", "RadioAddress", "RadioScan"]
    );
    // The scan waits for the subscribe, when the link is quiet.
    assert!(
        place(&log, "MqttSubscribe") < place(&log, "RadioScan"),
        "{log:?}"
    );
    assert_eq!(log[place(&log, "RadioScan") + 1], "TimeNotifyAfter 15000ms");
    assert_eq!(
        shell.take_logs(),
        [
            "info wifi: the scan found no access point that is 10 dB stronger than \
             02:00:00:00:00:01 on channel 6 (-55 dBm), so stay"
        ]
    );
    assert_eq!(shell.radio.associated, Some(HALL));
    assert_eq!(shell.model.wifi.phase, Phase::Up(HALL));
    assert!(!shell.model.wifi.cache.scan_due());
    assert_eq!(shell.broker.connections, 2);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_better_access_point_closes_cleanly_and_the_core_joins_it_once() {
    let mut shell = asleep();
    assert_eq!(shell.radio.move_strongest(), Some(LANDING));
    let disconnects = |shell: &Shell| {
        shell
            .broker
            .packets
            .iter()
            .filter(|packet| matches!(packet, Packet::Disconnect))
            .count()
    };
    let before = disconnects(&shell);

    shell.wake(Wake::Timer);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        [
            "RadioJoin Cached",
            "RadioAddress",
            "RadioScan",
            "RadioLeave",
            "RadioJoin Cached",
            "RadioAddress",
        ]
    );
    // The core sends a DISCONNECT packet and closes the stream. It leaves
    // only after the answer to the `Close` request.
    let scan = place(&log, "RadioScan");
    let disconnect = place(&log, "MqttDisconnect");
    let close = place(&log, "StreamClose");
    assert!(scan < disconnect && disconnect < close, "{log:?}");
    assert!(close < place(&log, "RadioLeave"), "{log:?}");
    assert_eq!(disconnects(&shell), before + 1);
    assert_eq!(
        shell.take_logs(),
        [
            "info wifi: change to 02:00:00:00:00:02 on channel 11 (-45 dBm), because the \
             signal of 02:00:00:00:00:01 on channel 6 is -75 dBm"
        ]
    );
    let roam = shell.closes.last().expect("a Close request");
    assert!(roam.clean, "{roam:?}");

    // The broker got the DISCONNECT, so it did not publish the last will.
    // The status stays `online`.
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
    assert_eq!(shell.broker.connections, 3);
    assert_eq!(shell.view().connection, Connection::Online);

    // One join went to the better access point, and the core does not
    // scan after it.
    assert_eq!(shell.radio.associated, Some(LANDING));
    assert_eq!(shell.radio.joins, 3);
    assert_eq!(shell.model.wifi.phase, Phase::Up(LANDING));
    assert_eq!(shell.model.wifi.cache.plan(), Target::Cached(LANDING));
    assert!(!shell.model.wifi.cache.scan_due());
}

#[test]
fn a_join_to_a_better_access_point_that_a_sleep_stops_does_not_skip_the_next_scan() {
    let mut shell = scanning();
    shell.radio.move_strongest();
    // The scan finds the better access point. The core closes the session,
    // leaves, and joins it.
    shell.answer_radio();
    shell.answer_radio();
    assert_eq!(
        shell.model.wifi.phase,
        Phase::Joining(Target::Cached(LANDING))
    );

    // The device sleeps before the join ends. `Off` stops the join.
    shell.send(Event::SleepNow);
    shell.answer_radio();
    assert!(shell.sleep.is_some());
    shell.hold_radio = false;
    shell.take_log();

    // The next join is not the join of a change of access point, so a scan
    // is due after it.
    shell.wake(Wake::Timer);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioJoin Cached", "RadioAddress", "RadioScan"]
    );
    assert_eq!(shell.radio.associated, Some(LANDING));
}

#[test]
fn a_cached_join_that_fails_joins_the_strongest_access_point_with_no_backoff() {
    let mut shell = asleep();
    // The cached access point is gone.
    shell
        .radio
        .access_points
        .retain(|seen| seen.access_point != HALL);

    shell.wake(Wake::Timer);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioJoin Cached", "RadioJoin Strongest", "RadioAddress"]
    );
    assert!(!log.contains(&"TimeNotifyAfter 1000ms".into()), "{log:?}");
    assert_eq!(shell.take_logs(), [JOIN_TO_HALL_FAILED]);
    assert_eq!(shell.radio.associated, Some(LANDING));
    assert_eq!(shell.model.wifi.cache.plan(), Target::Cached(LANDING));
    assert!(!shell.model.wifi.cache.scan_due());
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_join_to_the_strongest_that_fails_waits_for_the_backoff() {
    let mut shell = asleep();
    shell.radio.blocked = true;

    shell.wake(Wake::Timer);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Cached", "RadioJoin Strongest"]);
    let strongest = place(&log, "RadioJoin Strongest");
    assert!(
        log[strongest..].contains(&"TimeNotifyAfter 1000ms".into()),
        "{log:?}"
    );
    assert_eq!(shell.model.wifi.cache.plan(), Target::Strongest);
    assert_eq!(shell.view().connection, Connection::Connecting);
    assert_eq!(
        shell.take_logs(),
        [
            JOIN_TO_HALL_FAILED,
            "warn wifi: the join to the strongest access point failed, so the link is down",
            &connect_again(1),
        ]
    );

    // After the backoff, the core joins the strongest access point.
    shell.radio.blocked = false;
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Strongest", "RadioAddress"]);
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(shell.take_logs(), Vec::<String>::new());
}

#[test]
fn a_router_that_blocks_the_device_for_3_minutes_does_not_restart_the_chip() {
    let mut shell = online();
    shell.radio.blocked = true;
    shell.radio.drop_link();
    fail_the_session(&mut shell);

    shell.advance(Duration::from_secs(180));
    let log = shell.take_log();
    let joins = radio(&log)
        .iter()
        .filter(|entry| entry.starts_with("RadioJoin"))
        .count();
    assert!(joins > RESTART_AFTER as usize, "{log:?}");
    // Each join failed before DHCP, so nothing counted.
    assert!(!log.contains(&"RadioAddress".into()), "{log:?}");
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 0);

    // After the block, the core connects again within the longest backoff.
    shell.radio.blocked = false;
    shell.advance(RECONNECT_MAX);
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(shell.restarts, Vec::<String>::new());
}

#[test]
fn a_broker_host_that_does_not_answer_for_3_minutes_gets_new_leases_and_no_restart() {
    let mut shell = online();
    shell.broker.unreachable = true;
    fail_the_session(&mut shell);

    shell.advance(Duration::from_secs(180));
    let log = shell.take_log();
    // No answer arrives, so the link stays suspect, and each reconnect
    // leaves and joins again. Each join gets a new address.
    let leaves = radio(&log)
        .iter()
        .filter(|entry| **entry == "RadioLeave")
        .count();
    assert!(leaves > RESTART_AFTER as usize, "{log:?}");
    assert!(shell.model.wifi.health.suspect());
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 0);

    shell.broker.unreachable = false;
    shell.advance(RECONNECT_MAX);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_broker_that_is_down_for_3_minutes_keeps_the_association_on_each_reconnect() {
    let mut shell = house();
    // The broker is down, but the broker host is up. The host refuses each
    // connect with a TCP reset, which proves that the link works.
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));
    assert!(!shell.model.wifi.health.suspect());
    shell.take_log();

    shell.advance(Duration::from_secs(180));
    let log = shell.take_log();
    let joins = radio(&log);
    assert!(joins.len() > 2 * RESTART_AFTER as usize, "{log:?}");
    for pair in joins.chunks(2) {
        assert_eq!(pair, ["RadioJoin Cached keep", "RadioAddress"], "{log:?}");
    }
    let opens = log
        .iter()
        .filter(|entry| entry.starts_with("MqttConnect"))
        .count();
    assert!(opens > RESTART_AFTER as usize, "{log:?}");
    assert_eq!(shell.radio.joins, 1);
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert!(!shell.model.wifi.health.suspect());

    shell.broker.down = false;
    shell.advance(RECONNECT_MAX);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_refused_connect_clears_a_suspect_link() {
    let mut shell = online();
    shell.broker.down = true;
    fail_the_session(&mut shell);

    // The reconnect leaves and joins again, because the link is suspect.
    // Then the broker host refuses the connect.
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioLeave", "RadioJoin Cached", "RadioAddress"]
    );
    assert!(
        place(&log, "RadioAddress") < place(&log, "MqttConnect"),
        "{log:?}"
    );
    assert_eq!(shell.model.wifi.health, Health::default());

    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(1),
            LEAVE_HALL,
            "info wifi: the broker host ended the session, so the link moves data and is not \
             suspect",
            &connect_again(2),
        ]
    );

    // The link is not suspect, so the next reconnect keeps the association.
    shell.radio.has_address = false;
    shell.advance(RECONNECT_MIN * 2);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Cached keep", "RadioAddress"]);
    assert_eq!(shell.restarts, Vec::<String>::new());
    // The refused connect schedules the next wait.
    assert_eq!(shell.take_logs(), [connect_again(4)]);
}

#[test]
fn a_refused_connect_clears_a_suspect_link_and_its_count() {
    let mut shell = online();
    shell.broker.down = true;
    fail_the_session(&mut shell);
    shell.hold_radio = true;
    shell.advance(RECONNECT_MIN);
    // The `Leave` waits.
    shell.answer_radio();

    // The network stack keeps its address through the join, which counts
    // toward a restart. Then the broker host refuses the connect, which
    // clears the count.
    shell.radio.has_address = true;
    shell.answer_radio();
    shell.answer_radio();
    let log = shell.take_log();
    assert!(
        place(&log, "RadioAddress") < place(&log, "MqttConnect"),
        "{log:?}"
    );
    assert_eq!(shell.model.wifi.health, Health::default());
    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(1),
            LEAVE_HALL,
            "warn wifi: the address is from before the join, so the network stack can be stuck",
            &no_new_address(1),
            "info wifi: the broker host ended the session, so the link moves data and is not \
             suspect",
            &connect_again(2),
        ]
    );
}

#[test]
fn a_broker_that_stops_closes_the_session_and_the_core_keeps_the_association() {
    let mut shell = online();
    // The broker host closes the session with a FIN, and then refuses each
    // connect with a TCP reset. Both prove that the link works.
    shell.broker.stop();
    shell.run();
    assert!(!shell.model.wifi.health.suspect());

    shell.advance(RECONNECT_MIN + RECONNECT_MIN * 2);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        [
            "RadioJoin Cached keep",
            "RadioAddress",
            "RadioJoin Cached keep",
            "RadioAddress",
        ]
    );
    assert_eq!(shell.restarts, Vec::<String>::new());

    shell.broker.down = false;
    shell.advance(RECONNECT_MAX);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn an_end_by_the_host_while_the_device_has_no_link_does_not_clear_the_suspicion() {
    let mut shell = online();
    fail_the_session(&mut shell);
    // The core leaves before it joins again. While the `Leave` waits, the
    // device has no link, so an end by the host proves nothing.
    shell.hold_radio = true;
    shell.advance(RECONNECT_MIN);
    assert!(matches!(shell.model.wifi.phase, Phase::Leaving(_)));
    let end = crate::mqtt::End::ByHost("TCP connect failed: ConnectionReset".into());
    crate::features::wifi::session_ended(&end, &mut shell.model).expect_no_effects();
    assert!(shell.model.wifi.health.suspect());
}

#[test]
fn a_connect_with_no_answer_keeps_the_link_suspect() {
    let mut shell = online();
    shell.broker.unreachable = true;
    fail_the_session(&mut shell);

    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioLeave", "RadioJoin Cached", "RadioAddress"]
    );
    assert!(
        log.iter().any(|entry| entry.starts_with("MqttConnect")),
        "{log:?}"
    );
    assert!(shell.model.wifi.health.suspect());

    // The next reconnect leaves again.
    shell.advance(RECONNECT_MIN * 2);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioLeave", "RadioJoin Cached", "RadioAddress"]
    );
}

#[test]
fn a_stuck_driver_restarts_the_chip_after_3_joins_with_no_address() {
    let mut shell = online();
    shell.radio.stuck = true;
    shell.radio.drop_link();
    fail_the_session(&mut shell);

    // The first reconnect leaves and joins the cached access point, and then
    // the strongest one. Both get no address. After the backoff, the third
    // join gets no address too.
    shell.advance(RECONNECT_MIN + RECONNECT_MIN * 2);
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        [
            "RadioLeave",
            "RadioJoin Cached",
            "RadioAddress",
            "RadioLeave",
            "RadioJoin Strongest",
            "RadioAddress",
            "RadioLeave",
            "RadioJoin Strongest",
            "RadioAddress",
            "RadioRestart",
        ]
    );
    assert_eq!(shell.restarts, ["no address after 3 joins"]);
    assert_eq!(shell.model.wifi.phase, Phase::Restarting);
    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(1),
            LEAVE_HALL,
            &no_new_address(1),
            "warn wifi: 02:00:00:00:00:01 on channel 6 gave no address, so leave it and join \
             the strongest access point",
            &no_new_address(2),
            "warn wifi: the strongest access point 02:00:00:00:00:01 on channel 6 gave no \
             address, so leave it",
            &connect_again(2),
            &no_new_address(3),
        ]
    );

    // The core does no more radio work. The shell restarts the chip.
    shell.advance(RECONNECT_MAX * 2);
    assert_eq!(radio(&shell.take_log()), Vec::<&str>::new());

    // After the restart, the link is not suspect.
    shell.radio.stuck = false;
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.model.wifi.health, Health::default());
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_stale_lease_counts_and_bytes_from_the_broker_clear_the_count() {
    let mut shell = online();
    fail_the_session(&mut shell);
    shell.hold_radio = true;
    shell.advance(RECONNECT_MIN);
    // The `Leave` waits.
    shell.answer_radio();

    // The network stack keeps its address through the join, and the broker
    // host does not answer, so no bytes arrive.
    shell.radio.has_address = true;
    shell.broker.unreachable = true;
    shell.answer_radio();
    shell.answer_radio();
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 1);
    assert!(shell.model.wifi.health.suspect());
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(1),
            LEAVE_HALL,
            "warn wifi: the address is from before the join, so the network stack can be stuck",
            &no_new_address(1),
            &connect_again(2),
        ]
    );

    // The broker is up again. The next join gets a new address, and the
    // CONNACK packet proves that the link moves data.
    shell.hold_radio = false;
    shell.broker.unreachable = false;
    shell.advance(RECONNECT_MAX);
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(shell.model.wifi.health, Health::default());
}

#[test]
fn a_refused_connack_keeps_the_association() {
    let mut shell = house();
    // The broker refuses the connection. This is not a failure of the link.
    shell.broker.code = 5;
    shell.send(start(PowerSource::Stand));
    assert!(!shell.model.wifi.health.suspect());
    shell.take_log();

    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Cached keep", "RadioAddress"]);
    assert_eq!(shell.radio.joins, 1);
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 0);
}

#[test]
fn a_sleep_during_a_join_stops_the_join_and_turns_the_radio_off() {
    let mut shell = house();
    shell.hold_radio = true;
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.held_radio(), 1);
    shell.take_log();

    shell.send(Event::SleepNow);
    // `Off` stopped the join, and waits.
    assert_eq!(radio(&shell.take_log()), ["WifiOff"]);
    assert_eq!(shell.held_radio(), 1);
    shell.answer_radio();
    assert!(shell.sleep.is_some());
    assert!(!shell.radio.on);
    assert_eq!(shell.model.wifi.phase, Phase::Off);
}

#[test]
fn a_sleep_during_the_wait_for_an_address_turns_the_radio_off() {
    let mut shell = house();
    shell.hold_radio = true;
    shell.send(start(PowerSource::Battery));
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Leasing { .. }));
    shell.take_log();

    shell.send(Event::SleepNow);
    assert_eq!(radio(&shell.take_log()), ["WifiOff"]);
    shell.answer_radio();
    assert!(shell.sleep.is_some());
    assert!(!shell.radio.on);
}

#[test]
fn a_sleep_during_a_scan_stops_the_scan_before_the_disconnect() {
    let mut shell = scanning();

    shell.send(Event::SleepNow);
    // The DISCONNECT waits for the answer to `StopScan`.
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioStopScan"]);
    assert!(!log.contains(&"MqttDisconnect".into()), "{log:?}");

    shell.answer_radio();
    shell.answer_radio();
    let log = shell.take_log();
    let disconnect = place(&log, "MqttDisconnect");
    assert!(
        place(&log, "MqttPublish dashboard/hall/status asleep") < disconnect,
        "{log:?}"
    );
    assert!(disconnect < place(&log, "StreamClose"), "{log:?}");
    assert!(
        place(&log, "StreamClose") < place(&log, "WifiOff"),
        "{log:?}"
    );
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"), "{log:?}");
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));
    // A scan that a request stopped does not count, so it stays due.
    assert!(shell.model.wifi.cache.scan_due());
}

#[test]
fn a_wake_while_the_radio_turns_off_joins_after_the_answer() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.hold_radio = true;
    shell.send(Event::SleepNow);
    assert_eq!(shell.held_radio(), 1);
    shell.take_log();

    // The tap wakes the device. The core waits for the radio to turn off.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.asleep);
    assert_eq!(radio(&shell.take_log()), Vec::<&str>::new());

    shell.hold_radio = false;
    shell.answer_radio();
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioJoin Cached", "RadioAddress", "RadioScan"]
    );
    assert!(!log.contains(&"Sleep".into()), "{log:?}");
    assert_eq!(shell.sleep, None);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_join_with_no_answer_in_its_time_limit_fails_and_the_late_answer_is_ignored() {
    let mut shell = house();
    shell.hold_radio = true;
    shell.send(start(PowerSource::Stand));
    shell.take_log();

    let events = shell.advance(JOIN_TIMEOUT);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::RadioTimeout(_))),
        "{events:?}"
    );
    // The join to the strongest access point failed, so the core waits for
    // the backoff.
    assert_eq!(shell.model.wifi.phase, Phase::Idle);
    assert!(shell.take_log().contains(&"TimeNotifyAfter 1000ms".into()));

    // The late answer is ignored.
    shell.answer_radio();
    assert_eq!(radio(&shell.take_log()), Vec::<&str>::new());
    assert_eq!(shell.model.wifi.phase, Phase::Idle);

    shell.hold_radio = false;
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn an_address_with_no_answer_in_its_time_limit_does_not_count_toward_a_restart() {
    let mut shell = online();
    fail_the_session(&mut shell);
    shell.hold_radio = true;
    shell.advance(RECONNECT_MIN);
    shell.answer_radio();
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Leasing { .. }));
    shell.take_log();

    shell.advance(ADDRESS_TIMEOUT);
    // The core acts as for no lease: it leaves, and then joins the
    // strongest access point. The link is suspect, but nothing counts.
    assert_eq!(radio(&shell.take_log()), ["RadioLeave"]);
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 0);

    shell.hold_radio = false;
    shell.answer_radio();
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Strongest", "RadioAddress"]);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_leave_with_no_answer_in_its_time_limit_continues_with_the_join() {
    let mut shell = online();
    fail_the_session(&mut shell);
    shell.hold_radio = true;
    shell.advance(RECONNECT_MIN);
    assert!(matches!(shell.model.wifi.phase, Phase::Leaving(_)));
    shell.take_log();

    shell.advance(LEAVE_TIMEOUT);
    assert_eq!(radio(&shell.take_log()), ["RadioJoin Cached"]);

    // The late answer to the `Leave` is ignored. The join continues, and
    // then the scan that the cached join made due.
    shell.hold_radio = false;
    shell.answer_radio();
    assert_eq!(radio(&shell.take_log()), ["RadioAddress", "RadioScan"]);
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_scan_with_no_answer_in_its_time_limit_is_complete_and_the_late_answer_is_ignored() {
    let mut shell = scanning();

    shell.advance(SCAN_TIMEOUT);
    assert_eq!(shell.model.wifi.phase, Phase::Up(HALL));
    assert!(!shell.model.wifi.cache.scan_due());
    assert_eq!(
        shell.take_logs(),
        [
            "warn wifi: the scan request got no answer in its time limit",
            "warn wifi: the scan failed, so stay on 02:00:00:00:00:01 on channel 6",
        ]
    );

    // The late answer finds a better access point, but the core ignores it.
    shell.radio.move_strongest();
    shell.answer_radio();
    assert_eq!(radio(&shell.take_log()), Vec::<&str>::new());
    assert_eq!(shell.radio.associated, Some(HALL));
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_stop_of_a_scan_with_no_answer_in_its_time_limit_continues_the_sleep() {
    let mut shell = scanning();
    shell.send(Event::SleepNow);
    shell.take_log();

    shell.advance(STOP_SCAN_TIMEOUT);
    let log = shell.take_log();
    assert!(
        place(&log, "MqttDisconnect") < place(&log, "WifiOff"),
        "{log:?}"
    );

    // The late answer to `StopScan` is ignored, and `Off` ends the steps of
    // the sleep.
    shell.answer_radio();
    assert!(shell.sleep.is_some());
}

#[test]
fn an_off_with_no_answer_in_its_time_limit_still_sleeps() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.hold_radio = true;
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_none());

    shell.advance(OFF_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.take_log();

    // The late answer is ignored.
    shell.answer_radio();
    assert_eq!(shell.take_log(), Vec::<String>::new());
    assert!(shell.sleep.is_some());
}

#[test]
fn a_session_that_fails_during_a_scan_leaves_and_joins_and_the_scan_stays_due() {
    let mut shell = scanning();
    fail_the_session(&mut shell);

    // The reconnect leaves, which stops the scan.
    shell.advance(RECONNECT_MIN);
    assert_eq!(radio(&shell.take_log()), ["RadioLeave"]);
    assert_eq!(shell.held_radio(), 1);

    shell.hold_radio = false;
    shell.answer_radio();
    let log = shell.take_log();
    assert_eq!(
        radio(&log),
        ["RadioJoin Cached", "RadioAddress", "RadioScan"]
    );
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_time_limit_after_the_answer_does_nothing() {
    // The radio requests of the start got their answers, so their time
    // limits expire with the link up.
    let mut shell = online();
    let events = shell.advance(JOIN_TIMEOUT);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::RadioTimeout(_))),
        "{events:?}"
    );
    assert_eq!(radio(&shell.take_log()), Vec::<&str>::new());
    assert_eq!(shell.model.wifi.phase, Phase::Up(HALL));
    assert_eq!(shell.view().connection, Connection::Online);

    // The time limit of `Off` expires while the device sleeps.
    let mut shell = asleep();
    let radio_off = shell.model.wifi.radio;
    shell.advance(OFF_TIMEOUT);
    assert_eq!(shell.take_log(), Vec::<String>::new());
    assert_eq!(shell.model.wifi.phase, Phase::Off);
    assert_eq!(shell.model.wifi.radio, radio_off);
    assert!(shell.sleep.is_some());
}

#[test]
fn the_status_stays_online_while_the_core_changes_access_point() {
    let mut shell = scanning();
    shell.radio.move_strongest();

    // The scan finds a better access point. The core closes the session
    // with a DISCONNECT packet, and the `Leave` request waits.
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Leaving(_)));
    assert!(!shell.broker.is_connected());
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
    assert_eq!(shell.view().connection, Connection::Connecting);

    shell.hold_radio = false;
    shell.answer_radio();
    assert_eq!(shell.radio.associated, Some(LANDING));
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
    assert_eq!(shell.view().connection, Connection::Online);
}

/// Returns a model of a device on battery in the steps of the sleep 1,
/// with settings. The radio request with the id 9 waits in `phase`.
fn sleeping_with(phase: Phase) -> Model {
    let mut model = Model {
        power: power::State {
            source: PowerSource::Battery,
            asleep: true,
            sleep: 1,
            ..power::State::default()
        },
        settings: Some(settings()),
        ..Model::default()
    };
    model.wifi.phase = phase;
    model.wifi.radio = 9;
    model
}

#[test]
fn a_no_lease_after_the_sleep_started_does_not_count_or_restart_the_chip() {
    let mut shell = house();
    shell.hold_radio = true;
    shell.send(start(PowerSource::Battery));
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Leasing { .. }));
    // The link is suspect, and two joins got no address. The next one is
    // the last before a restart.
    let health = &mut shell.model.wifi.health;
    health.failed();
    assert_eq!(health.no_lease(), Recovery::Continue);
    assert_eq!(health.no_lease(), Recovery::Continue);
    shell.radio.stuck = true;
    shell.take_log();

    // The sleep starts, and `NoLease` arrives before `Off`.
    shell.queue(Event::SleepNow);
    shell.answer_radio();
    let log = shell.take_log();
    assert_eq!(radio(&log), ["WifiOff"]);
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 2);

    // The sleep continues in the usual order.
    shell.answer_radio();
    let log = shell.take_log();
    assert!(place(&log, "Render") < place(&log, "Sleep"), "{log:?}");
    assert!(shell.sleep.is_some());
}

#[test]
fn an_association_after_the_sleep_started_is_up_but_not_ready() {
    let mut model = sleeping_with(Phase::Joining(Target::Strongest));
    let joined = Joined::Associated {
        access_point: HALL,
        rssi: Some(-55),
        kept: false,
        had_address: false,
    };
    Dashboard
        .update(Event::Joined { radio: 9, joined }, &mut model)
        .expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Up(HALL));
    assert!(!model.wifi.ready);
    // The core keeps nothing in the cache.
    assert_eq!(model.wifi.cache.plan(), Target::Strongest);

    // A wake before `Off` joins again from `Up`. The cache has no access
    // point, so the core leaves first.
    let woke = Event::Woke {
        sleep: 1,
        wake: Wake::Timer,
    };
    Dashboard
        .update(woke, &mut model)
        .expect_render()
        .expect_time_notify_after()
        .expect_radio_leave();
}

#[test]
fn a_time_limit_after_the_sleep_started_changes_nothing_in_the_cache() {
    let mut model = sleeping_with(Phase::Leasing {
        target: Target::Cached(HALL),
        access_point: HALL,
        had_address: false,
        kept: false,
    });
    model.wifi.cache.joined(Target::Cached(HALL), HALL);
    Dashboard
        .update(Event::RadioTimeout(9), &mut model)
        .expect_only_log_with(|log| {
            assert_eq!(
                log.to_string(),
                "wifi: the address request got no answer in its time limit"
            );
        });
    assert_eq!(model.wifi.phase, Phase::Idle);
    assert_eq!(model.wifi.cache.plan(), Target::Cached(HALL));
}

#[test]
fn a_restart_wins_over_a_sleep() {
    let mut model = sleeping_with(Phase::Restarting);
    // The stream closed for the sleep. The core does not turn the radio off.
    Dashboard
        .update(Event::Offline(1), &mut model)
        .expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Restarting);
}

#[test]
fn bytes_from_the_broker_clear_a_suspect_link_with_one_line() {
    let mut shell = online();
    fail_the_session(&mut shell);

    // The reconnect gets a new address. The CONNACK and the SUBACK packets
    // arrive in separate chunks, but only the first chunk writes a line.
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.view().connection, Connection::Online);
    assert!(!shell.model.wifi.health.suspect());
    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(1),
            LEAVE_HALL,
            "info wifi: bytes from the broker arrived, so the link is not suspect",
            // The join went to the cached access point, so a scan follows.
            "info wifi: the scan found no access point that is 10 dB stronger than \
             02:00:00:00:00:01 on channel 6 (-55 dBm), so stay",
        ]
    );
}

#[test]
fn a_rejoin_that_keeps_the_association_and_its_address_writes_no_line() {
    let mut shell = house();
    // The broker refuses the connection. The link is not suspect, so the
    // reconnect keeps the association, and the network stack keeps its
    // address.
    shell.broker.code = 5;
    shell.send(start(PowerSource::Stand));
    shell.take_log();
    assert_eq!(shell.take_logs(), [connect_again(1)]);

    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Cached keep", "RadioAddress"]);
    assert!(shell.radio.has_address);
    // The rejoin writes no line of its own. The refused connect writes the
    // wait of the next reconnect.
    assert_eq!(shell.take_logs(), [connect_again(2)]);
}

#[test]
fn a_better_access_point_found_during_a_sleep_is_for_the_next_wake() {
    let mut model = sleeping_with(Phase::Scanning(HALL));
    model.wifi.cache.joined(Target::Cached(HALL), HALL);
    let scanned = Scanned::Found {
        seen: alloc::vec![
            Seen {
                access_point: HALL,
                rssi: -75,
            },
            Seen {
                access_point: LANDING,
                rssi: -45,
            },
        ],
        rssi: None,
    };
    Dashboard
        .update(Event::Scanned { radio: 9, scanned }, &mut model)
        .expect_only_log_with(|log| {
            assert_eq!(
                log.to_string(),
                "wifi: change to 02:00:00:00:00:02 on channel 11 at the next wake, because \
                 the signal of 02:00:00:00:00:01 on channel 6 is -75 dBm"
            );
        });
    assert_eq!(model.wifi.cache.plan(), Target::Cached(LANDING));
}

#[test]
fn the_count_of_joins_with_no_new_address_comes_before_the_restart() {
    let mut model = Model {
        settings: Some(settings()),
        ..Model::default()
    };
    model.wifi.phase = Phase::Leasing {
        target: Target::Strongest,
        access_point: HALL,
        had_address: false,
        kept: false,
    };
    model.wifi.radio = 9;
    let health = &mut model.wifi.health;
    health.failed();
    assert_eq!(health.no_lease(), Recovery::Continue);
    assert_eq!(health.no_lease(), Recovery::Continue);

    let leased = Event::Leased {
        radio: 9,
        lease: Lease::NoLease,
    };
    Dashboard
        .update(leased, &mut model)
        .expect_log_with(|log| {
            assert_eq!(log.level, Level::Warn);
            assert_eq!(
                log.message,
                "3 of 3 joins got no new address while the link is suspect"
            );
        })
        .expect_only_radio_restart_with(|restart| {
            assert_eq!(restart.reason, "no address after 3 joins");
        });
}

#[test]
fn a_stale_address_on_a_link_that_is_not_suspect_writes_no_line() {
    let mut model = Model {
        settings: Some(settings()),
        ..Model::default()
    };
    model.wifi.phase = Phase::Leasing {
        target: Target::Cached(HALL),
        access_point: HALL,
        had_address: true,
        kept: false,
    };
    model.wifi.radio = 9;

    let leased = Event::Leased {
        radio: 9,
        lease: Lease::Leased,
    };
    let mut cmd = Dashboard.update(leased, &mut model);
    let lines: Vec<Effect> = cmd
        .effects()
        .filter(|effect| matches!(effect, Effect::Log(_)))
        .collect();
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(model.wifi.phase, Phase::Up(HALL));
    assert_eq!(model.wifi.health, Health::default());
}

#[test]
fn a_scan_with_no_signal_for_the_current_access_point_stays() {
    let mut model = Model {
        settings: Some(settings()),
        ..Model::default()
    };
    model.wifi.phase = Phase::Scanning(HALL);
    model.wifi.radio = 9;
    model.wifi.cache.joined(Target::Cached(HALL), HALL);
    let scanned = Scanned::Found {
        seen: alloc::vec![Seen {
            access_point: LANDING,
            rssi: -45,
        }],
        rssi: None,
    };
    Dashboard
        .update(Event::Scanned { radio: 9, scanned }, &mut model)
        .expect_only_log_with(|log| {
            assert_eq!(log.level, Level::Info);
            assert_eq!(
                log.to_string(),
                "wifi: the signal of 02:00:00:00:00:01 on channel 6 is unknown, so stay"
            );
        });
    assert_eq!(model.wifi.phase, Phase::Up(HALL));
    assert_eq!(model.wifi.cache.plan(), Target::Cached(HALL));
}

#[test]
fn a_strongest_join_that_fails_after_the_sleep_started_writes_no_line() {
    let mut model = sleeping_with(Phase::Joining(Target::Strongest));
    let joined = Joined::Failed("no access point found".into());
    Dashboard
        .update(Event::Joined { radio: 9, joined }, &mut model)
        .expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Idle);
}
