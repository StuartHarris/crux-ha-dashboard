//! Flow tests of the Wi-Fi policy: the core runs in the test shell, with the
//! fake radio and the fake broker.
//!
//! Each radio request has a time limit, so a `TimeNotifyAfter` line follows
//! each request in the log. A test that needs a request that waits sets
//! [`Shell::hold_radio`].

use crate::features::wifi;
use alloc::{format, string::String, vec, vec::Vec};
use core::time::Duration;

use crux_core::App;

use crate::tests::{
    STARTED, asleep as is_asleep, flows::house, settings, shell::Shell, sleep_lines, start,
};
use crate::{
    Connection, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource,
    capabilities::{
        log::Level,
        power::Wake,
        radio::{AccessPoint, Joined, Lease, Scanned, Seen, Target},
    },
    fake::radio::{HALL, IP, LANDING, STRONG_RSSI},
    features::{
        connection::{self, RECONNECT_MAX, RECONNECT_MIN},
        device,
        power::{self, IDLE_TIMEOUT, SleepStep},
        wifi::{
            ADDRESS_TIMEOUT, JOIN_TIMEOUT, LEAVE_TIMEOUT, Linked, OFF_TIMEOUT, Phase, SCAN_TIMEOUT,
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

/// The end of a session that failed because the fake broker dropped the
/// connection.
const LOST: &str = "the stream closed: connection lost";

/// The end of a session that the broker host refused.
const REFUSED: &str = "the broker host ended the stream: connection refused";

/// The end of a session that the broker closed when it stopped.
const BROKER_CLOSED: &str = "the broker host ended the stream: the broker closed the stream";

/// The end of a session that the broker refused with the code 5.
const NOT_AUTHORIZED: &str = "the broker refused the connection (5: not authorized)";

/// The end of a session on a link that moves no data.
const NO_ANSWER: &str = "the stream closed: no answer from the host";

/// Returns the log line of a session that ended with `cause`, and of the
/// reconnect that the core schedules after `secs` seconds.
fn connect_again(cause: &str, secs: u32) -> String {
    format!("warn mqtt: {cause}, so connect again in {secs} s")
}

/// Returns the log line of a reconnect after a link that failed, which the
/// core schedules after `secs` seconds.
fn link_down(secs: u32) -> String {
    format!("info mqtt: the link is down, so connect again in {secs} s")
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

/// Returns a link to `access_point` that is ready, so a link request
/// connects with no join.
const fn ready(access_point: AccessPoint) -> Linked {
    Linked {
        access_point,
        ip: IP,
        rssi: Some(-55),
        ready: true,
    }
}

/// Returns a link to `access_point` that is not ready, so a link request
/// joins again.
const fn not_ready(access_point: AccessPoint) -> Linked {
    Linked {
        access_point,
        ip: IP,
        rssi: Some(-55),
        ready: false,
    }
}

/// Starts the house on the stand, connects, and clears the log.
fn online() -> Shell {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.take_log();
    assert_eq!(shell.take_logs(), [STARTED]);
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
    let mut lines = vec![String::from(STARTED)];
    lines.extend(sleep_lines(1));
    assert_eq!(shell.take_logs(), lines);
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
    shell.wake(Wake::Touch(None));
    shell.answer_radio();
    shell.answer_radio();
    assert_eq!(shell.model.wifi.phase, Phase::Scanning(ready(HALL)));
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
    assert_eq!(shell.model.wifi.phase, Phase::Up(ready(HALL)));
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
    // Only the start and the steps of the sleep write lines, with the area
    // `power`.
    let mut lines = vec![String::from(STARTED)];
    lines.extend(sleep_lines(1));
    assert_eq!(shell.take_logs(), lines);
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
    // writes the wait. The other steps write no line, except the steps of
    // the sleep.
    let mut expected = alloc::vec![connect_again(BROKER_CLOSED, 1), connect_again(REFUSED, 2)];
    expected.extend(sleep_lines(1));
    assert_eq!(shell.take_logs(), expected);
}

#[test]
fn a_wake_joins_the_cached_access_point_and_stays_if_the_scan_finds_less_than_the_margin() {
    let mut shell = asleep();
    // The other access point is stronger, but by less than the margin.
    shell.radio.access_points[1].rssi = -46;

    shell.wake(Wake::Touch(None));
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
            "debug wifi: the scan found no access point that is 10 dB stronger than \
             02:00:00:00:00:01 on channel 6 (-55 dBm), so stay"
        ]
    );
    assert_eq!(shell.radio.associated, Some(HALL));
    assert_eq!(shell.model.wifi.phase, Phase::Up(ready(HALL)));
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

    shell.wake(Wake::Touch(None));
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
    // The join gave the signal of the better access point.
    assert_eq!(
        shell.model.wifi.phase,
        Phase::Up(Linked {
            rssi: Some(STRONG_RSSI),
            ..ready(LANDING)
        })
    );
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
    shell.wake(Wake::Touch(None));
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

    shell.wake(Wake::Touch(None));
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

    shell.wake(Wake::Touch(None));
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
            &link_down(1),
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
            &connect_again(LOST, 1),
            LEAVE_HALL,
            "info wifi: the broker host ended the session, so the link moves data and is not \
             suspect",
            &connect_again(REFUSED, 2),
        ]
    );

    // The link is not suspect, so the next reconnect keeps the association.
    shell.radio.has_address = false;
    shell.advance(RECONNECT_MIN * 2);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Cached keep", "RadioAddress"]);
    assert_eq!(shell.restarts, Vec::<String>::new());
    // The refused connect schedules the next wait.
    assert_eq!(shell.take_logs(), [connect_again(REFUSED, 4)]);
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
            &connect_again(LOST, 1),
            LEAVE_HALL,
            "warn wifi: the address is from before the join, so the network stack can be stuck",
            &no_new_address(1),
            "info wifi: the broker host ended the session, so the link moves data and is not \
             suspect",
            &connect_again(REFUSED, 2),
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
    // The connection has no stream at the restart, so the restart needs
    // no clean close: the session before it already failed.
    assert!(!shell.model.connection.has_stream());
    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(LOST, 1),
            LEAVE_HALL,
            &no_new_address(1),
            "warn wifi: 02:00:00:00:00:01 on channel 6 gave no address, so leave it and join \
             the strongest access point",
            &no_new_address(2),
            "warn wifi: the strongest access point 02:00:00:00:00:01 on channel 6 gave no \
             address, so leave it",
            &link_down(2),
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
            &connect_again(LOST, 1),
            LEAVE_HALL,
            "warn wifi: the address is from before the join, so the network stack can be stuck",
            &no_new_address(1),
            &connect_again(NO_ANSWER, 2),
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
    assert!(!shell.model.power.is_asleep());
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
            .any(|event| matches!(event, Event::Wifi(wifi::Event::RadioTimeout(_)))),
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
    assert_eq!(shell.model.wifi.phase, Phase::Up(ready(HALL)));
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
    // The radio requests of the start got their answers, and each answer
    // cleared its time limit. No time limit expires.
    let mut shell = online();
    let events = shell.advance(JOIN_TIMEOUT);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Wifi(wifi::Event::RadioTimeout(_)))),
        "{events:?}"
    );
    assert_eq!(radio(&shell.take_log()), Vec::<&str>::new());
    assert_eq!(shell.model.wifi.phase, Phase::Up(ready(HALL)));
    assert_eq!(shell.view().connection, Connection::Online);

    // The answer to `Off` cleared its time limit, so nothing happens while
    // the device sleeps.
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
        power: power::State::asleep_in(1),
        startup: crate::Startup::Started {
            settings: settings(),
            firmware: crate::tests::firmware(),
            boot: crate::tests::POWER_ON,
        },
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
fn an_association_after_the_sleep_started_has_no_address_and_no_link() {
    let mut model = sleeping_with(Phase::Joining(Target::Strongest));
    let joined = Joined::Associated {
        access_point: HALL,
        rssi: Some(-55),
        kept: false,
        had_address: false,
    };
    Dashboard
        .update(
            Event::Wifi(wifi::Event::Joined { radio: 9, joined }),
            &mut model,
        )
        .expect_no_effects();
    // The core knows no address, so the device has no link.
    assert_eq!(model.wifi.phase, Phase::Associated(HALL));
    assert_eq!(model.wifi.phase.linked(), None);
    // The core keeps nothing in the cache.
    assert_eq!(model.wifi.cache.plan(), Target::Strongest);

    // A tap before `Off` wakes the device, which joins again from
    // `Associated`. The cache has no access point, so the core leaves first.
    Dashboard
        .update(Event::Tapped(None), &mut model)
        .expect_render()
        .expect_time_notify_after()
        .expect_radio_leave();
}

/// Returns a model in the steps of the sleep 1, with [`HALL`] in the cache.
/// The `Join` request 9 to [`HALL`] waits. If `suspect` is `true`, the link
/// is suspect.
fn sleeping_while_joining_hall(suspect: bool) -> Model {
    let mut model = sleeping_with(Phase::Joining(Target::Cached(HALL)));
    model.wifi.cache.joined(Target::Strongest, HALL);
    if suspect {
        model.wifi.health.failed();
    }
    model
}

/// Sends the answer to the `Join` request 9: the device associated with
/// [`HALL`], and kept the association and its address.
fn associate_with_hall(model: &mut Model) {
    let joined = Joined::Associated {
        access_point: HALL,
        rssi: Some(-55),
        kept: true,
        had_address: true,
    };
    Dashboard
        .update(Event::Wifi(wifi::Event::Joined { radio: 9, joined }), model)
        .expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Associated(HALL));
}

/// Sends a tap, which wakes the device in the steps of a sleep. Returns the
/// radio requests and the log lines of the wake, as in [`Shell::take_log`].
fn wake_with_a_tap(model: &mut Model) -> Vec<String> {
    let mut cmd = Dashboard.update(Event::Tapped(None), model);
    cmd.effects()
        .filter_map(|effect| match effect {
            Effect::RadioJoin(request) => Some(format!(
                "RadioJoin {:?} keep {}",
                request.operation.target, request.operation.keep
            )),
            Effect::RadioLeave(_) => Some("RadioLeave".into()),
            Effect::Log(request) => {
                Some(format!("{} {}", request.operation.level, request.operation))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_lease_after_the_sleep_started_counts_nothing_and_a_wake_leaves_a_suspect_link() {
    let mut model = sleeping_with(Phase::Leasing {
        target: Target::Cached(HALL),
        access_point: HALL,
        rssi: Some(-55),
        had_address: false,
        kept: false,
    });
    model.wifi.cache.joined(Target::Strongest, HALL);
    // The link is suspect, and one join got no address.
    model.wifi.health.failed();
    assert_eq!(model.wifi.health.no_lease(), Recovery::Continue);

    let leased = Event::Wifi(wifi::Event::Leased {
        radio: 9,
        lease: Lease::Leased { ip: IP },
    });
    Dashboard.update(leased, &mut model).expect_no_effects();
    // The new address in the steps of the sleep counts nothing, and the
    // core keeps no address: the device has no link.
    assert_eq!(model.wifi.health.joins_with_no_lease(), 1);
    assert!(model.wifi.health.suspect());
    assert_eq!(model.wifi.cache.plan(), Target::Cached(HALL));
    assert_eq!(model.wifi.phase, Phase::Associated(HALL));
    assert_eq!(model.wifi.phase.linked(), None);

    // A tap wakes the device. The link is suspect, so the core leaves
    // first.
    assert_eq!(wake_with_a_tap(&mut model), [LEAVE_HALL, "RadioLeave"]);
}

#[test]
fn a_no_lease_after_the_sleep_started_keeps_the_association_for_a_wake() {
    let mut model = sleeping_with(Phase::Leasing {
        target: Target::Cached(HALL),
        access_point: HALL,
        rssi: Some(-55),
        had_address: false,
        kept: false,
    });
    model.wifi.cache.joined(Target::Strongest, HALL);
    let no_lease = Event::Wifi(wifi::Event::Leased {
        radio: 9,
        lease: Lease::NoLease,
    });
    Dashboard.update(no_lease, &mut model).expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Associated(HALL));

    // A tap wakes the device, which keeps the association with the cached
    // access point.
    assert_eq!(
        wake_with_a_tap(&mut model),
        [format!("RadioJoin {:?} keep true", Target::Cached(HALL))]
    );
}

#[test]
fn a_wake_from_an_association_during_a_sleep_on_a_suspect_link_leaves_first() {
    let mut model = sleeping_while_joining_hall(true);
    associate_with_hall(&mut model);
    assert_eq!(wake_with_a_tap(&mut model), [LEAVE_HALL, "RadioLeave"]);
}

#[test]
fn a_wake_from_an_association_during_a_sleep_with_the_cached_access_point_keeps_it() {
    let mut model = sleeping_while_joining_hall(false);
    associate_with_hall(&mut model);
    assert_eq!(
        wake_with_a_tap(&mut model),
        [format!("RadioJoin {:?} keep true", Target::Cached(HALL))]
    );
}

#[test]
fn an_end_by_the_host_during_an_association_in_a_sleep_proves_traffic() {
    let mut model = sleeping_while_joining_hall(true);
    associate_with_hall(&mut model);
    // No session can end in this phase, because a session starts only on a
    // ready link. The rule for an association during a sleep is the rule for
    // a link.
    let end = crate::mqtt::End::ByHost("TCP connect failed: ConnectionReset".into());
    crate::features::wifi::session_ended(&end, &mut model).expect_only_log_with(|log| {
        assert_eq!(
            log.to_string(),
            "wifi: the broker host ended the session, so the link moves data and is not suspect"
        );
    });
    assert!(!model.wifi.health.suspect());
    assert_eq!(model.wifi.phase, Phase::Associated(HALL));
}

#[test]
fn a_time_limit_after_the_sleep_started_changes_nothing_in_the_cache() {
    let mut model = sleeping_with(Phase::Leasing {
        target: Target::Cached(HALL),
        access_point: HALL,
        rssi: None,
        had_address: false,
        kept: false,
    });
    model.wifi.cache.joined(Target::Cached(HALL), HALL);
    Dashboard
        .update(Event::Wifi(wifi::Event::RadioTimeout(9)), &mut model)
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
    // The stream closed for the sleep. The core does not turn the radio off,
    // and writes why.
    Dashboard
        .update(Event::Power(power::Event::Offline(1)), &mut model)
        .expect_only_log_with(|log| {
            assert_eq!(log.level, Level::Warn);
            assert_eq!(
                log.to_string(),
                "power: sleep 1 stops before the radio turns off, because the chip restarts"
            );
        });
    assert_eq!(model.wifi.phase, Phase::Restarting);
    assert_eq!(power::sleep_step(&model), Some(SleepStep::Stalled));
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
            &connect_again(LOST, 1),
            LEAVE_HALL,
            "info wifi: bytes from the broker arrived, so the link is not suspect",
            // The join went to the cached access point, so a scan follows.
            "debug wifi: the scan found no access point that is 10 dB stronger than \
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
    assert_eq!(
        shell.take_logs(),
        [String::from(STARTED), connect_again(NOT_AUTHORIZED, 1)]
    );

    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(radio(&log), ["RadioJoin Cached keep", "RadioAddress"]);
    assert!(shell.radio.has_address);
    // The rejoin writes no line of its own. The refused connect writes the
    // wait of the next reconnect.
    assert_eq!(shell.take_logs(), [connect_again(NOT_AUTHORIZED, 2)]);
}

#[test]
fn a_better_access_point_found_during_a_sleep_is_for_the_next_wake() {
    let mut model = sleeping_with(Phase::Scanning(not_ready(HALL)));
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
        .update(
            Event::Wifi(wifi::Event::Scanned { radio: 9, scanned }),
            &mut model,
        )
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
        startup: crate::Startup::Started {
            settings: settings(),
            firmware: crate::tests::firmware(),
            boot: crate::tests::POWER_ON,
        },
        ..Model::default()
    };
    model.wifi.phase = Phase::Leasing {
        target: Target::Strongest,
        access_point: HALL,
        rssi: None,
        had_address: false,
        kept: false,
    };
    model.wifi.radio = 9;
    let health = &mut model.wifi.health;
    health.failed();
    assert_eq!(health.no_lease(), Recovery::Continue);
    assert_eq!(health.no_lease(), Recovery::Continue);

    let leased = Event::Wifi(wifi::Event::Leased {
        radio: 9,
        lease: Lease::NoLease,
    });
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
        startup: crate::Startup::Started {
            settings: settings(),
            firmware: crate::tests::firmware(),
            boot: crate::tests::POWER_ON,
        },
        ..Model::default()
    };
    model.wifi.phase = Phase::Leasing {
        target: Target::Cached(HALL),
        access_point: HALL,
        rssi: Some(-55),
        had_address: true,
        kept: false,
    };
    model.wifi.radio = 9;

    let leased = Event::Wifi(wifi::Event::Leased {
        radio: 9,
        lease: Lease::Leased { ip: IP },
    });
    let mut cmd = Dashboard.update(leased, &mut model);
    let lines: Vec<Effect> = cmd
        .effects()
        .filter(|effect| matches!(effect, Effect::Log(_)))
        .collect();
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(model.wifi.phase, Phase::Up(ready(HALL)));
    assert_eq!(model.wifi.health, Health::default());
}

#[test]
fn a_scan_with_no_signal_for_the_current_access_point_stays() {
    let mut model = Model {
        startup: crate::Startup::Started {
            settings: settings(),
            firmware: crate::tests::firmware(),
            boot: crate::tests::POWER_ON,
        },
        ..Model::default()
    };
    model.wifi.phase = Phase::Scanning(not_ready(HALL));
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
        .update(
            Event::Wifi(wifi::Event::Scanned { radio: 9, scanned }),
            &mut model,
        )
        .expect_only_log_with(|log| {
            assert_eq!(log.level, Level::Debug);
            assert_eq!(
                log.to_string(),
                "wifi: the signal of 02:00:00:00:00:01 on channel 6 is unknown, so stay"
            );
        });
    assert_eq!(model.wifi.phase, Phase::Up(not_ready(HALL)));
    assert_eq!(model.wifi.cache.plan(), Target::Cached(HALL));
}

#[test]
fn a_strongest_join_that_fails_after_the_sleep_started_writes_no_line() {
    let mut model = sleeping_with(Phase::Joining(Target::Strongest));
    let joined = Joined::Failed("no access point found".into());
    Dashboard
        .update(
            Event::Wifi(wifi::Event::Joined { radio: 9, joined }),
            &mut model,
        )
        .expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Idle);
}

#[test]
fn an_answer_clears_the_time_limit_of_its_request() {
    let mut shell = house();
    shell.hold_radio = true;
    shell.send(start(PowerSource::Stand));
    let join = shell.model.wifi.radio;
    shell.take_log();

    shell.answer_radio();
    let log = shell.take_log();
    assert_eq!(
        log.first().map(String::as_str),
        Some("TimeClear"),
        "{log:?}"
    );

    let events = shell.advance(JOIN_TIMEOUT);
    assert!(
        !events.iter().any(
            |event| matches!(event, Event::Wifi(wifi::Event::RadioTimeout(radio)) if *radio == join)
        ),
        "{events:?}"
    );
}

#[test]
fn a_wake_during_the_scan_stop_of_a_sleep_does_not_disconnect() {
    let mut shell = scanning();
    // The sleep first stops the scan.
    shell.send(Event::SleepNow);
    assert!(is_asleep(&shell.model));
    // A tap wakes the device before the answer to `StopScan`.
    shell.send(Event::Tapped(None));
    assert!(!is_asleep(&shell.model));
    shell.take_log();

    // The answer does not continue the sleep.
    shell.answer_radio();
    let log = shell.take_log();
    assert!(
        !log.iter().any(|entry| entry == "MqttDisconnect"),
        "{log:?}"
    );
    assert_eq!(
        shell.model.connection.link(),
        crate::features::connection::Link::Up
    );
    assert!(!is_asleep(&shell.model));
}

#[test]
fn a_new_sleep_during_the_scan_stop_of_an_old_sleep_waits_for_the_stop() {
    let mut shell = scanning();
    // The first sleep stops the scan. A tap wakes the device before the
    // answer, and turns the light on.
    shell.send(Event::SleepNow);
    let old = shell.model.power.sleep;
    shell.send(Event::Tapped(None));
    assert!(!is_asleep(&shell.model));
    shell.take_log();

    // The second sleep turns the light off. Then it takes over the
    // `StopScan` request of the first sleep, so the DISCONNECT waits.
    shell.send(Event::SleepNow);
    let new = shell.model.power.sleep;
    assert_ne!(new, old);
    let log = shell.take_log();
    assert!(log.contains(&"FrontLight 0".into()), "{log:?}");
    assert_eq!(radio(&log), Vec::<&str>::new());
    assert!(!log.contains(&"MqttDisconnect".into()), "{log:?}");
    assert_eq!(shell.held_radio(), 1);
    assert!(matches!(
        shell.model.wifi.phase,
        Phase::StoppingScan { sleep, .. } if sleep == new
    ));
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::StopScan));
    let stop = shell.model.wifi.radio;

    // The answer to the old request continues the second sleep: publish
    // `asleep`, disconnect, turn the radio off and sleep.
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
    assert!(shell.sleep.is_some());
    assert!(is_asleep(&shell.model));
    assert_eq!(shell.model.power.sleep, new);
    assert_eq!(shell.view().connection, Connection::Offline);

    // A second answer with the id of the old request does nothing.
    shell.send(Event::Wifi(wifi::Event::ScanStopped(stop)));
    assert_eq!(shell.take_log(), Vec::<String>::new());
    assert_eq!(shell.model.wifi.phase, Phase::Off);
}

#[test]
fn a_wake_after_a_new_sleep_took_over_the_scan_stop_does_not_disconnect() {
    let mut shell = scanning();
    shell.send(Event::SleepNow);
    shell.send(Event::Tapped(None));
    // The second sleep takes over the `StopScan` request, and a tap wakes
    // the device again before the answer.
    shell.send(Event::SleepNow);
    assert!(matches!(shell.model.wifi.phase, Phase::StoppingScan { .. }));
    shell.send(Event::Tapped(None));
    assert!(!is_asleep(&shell.model));
    shell.take_log();

    // The answer does not continue either sleep.
    shell.answer_radio();
    let log = shell.take_log();
    assert!(!log.contains(&"MqttDisconnect".into()), "{log:?}");
    assert_eq!(shell.model.wifi.phase, Phase::Up(ready(HALL)));
    assert_eq!(
        shell.model.connection.link(),
        crate::features::connection::Link::Up
    );
    assert!(!is_asleep(&shell.model));
}

#[test]
fn a_sleep_during_a_scan_goes_through_each_step_and_writes_one_line_for_each() {
    let mut shell = scanning();
    // The tap turns the light on, so the sleep first turns it off.
    shell.send(Event::Tapped(None));
    assert_eq!(power::sleep_step(&shell.model), None);

    // The shell answers `FrontLight` at once. Then the `StopScan` request
    // waits.
    shell.send(Event::SleepNow);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::StopScan));

    // The answer disconnects. Then the `Off` request waits.
    shell.answer_radio();
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::RadioOff));

    // The answer draws the last image. Then the `Sleep` request waits.
    shell.answer_radio();
    assert!(shell.sleep.is_some());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Sleep));
    assert_eq!(
        shell.take_logs(),
        [
            "debug power: sleep 2 turns the front light off",
            "debug power: sleep 2 waits for the end of the scan",
            "debug power: sleep 2 closes the connection",
            "debug power: sleep 2 turns the radio off",
            "debug power: sleep 2 draws the last image and sleeps",
        ]
    );

    // The wake ends the sleep.
    shell.hold_radio = false;
    shell.wake(Wake::Touch(None));
    assert_eq!(power::sleep_step(&shell.model), None);
}

#[test]
fn a_chip_restart_during_a_sleep_leaves_the_core_asleep_with_no_step_that_waits() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.take_log();
    // A stuck driver makes the core ask for a restart of the chip, before
    // the idle time ends.
    shell.radio.stuck = true;
    shell.radio.drop_link();
    fail_the_session(&mut shell);
    shell.advance(RECONNECT_MIN + RECONNECT_MIN * 2);
    assert_eq!(shell.restarts, ["no address after 3 joins"]);
    assert_eq!(shell.model.wifi.phase, Phase::Restarting);
    shell.take_log();

    // The sleep does not turn the radio off, because the restart wins. No
    // `Sleep` request follows.
    shell.send(Event::SleepNow);
    let log = shell.take_log();
    assert!(!log.iter().any(|entry| entry == "WifiOff"), "{log:?}");
    assert!(is_asleep(&shell.model));
    assert!(shell.sleep.is_none());
    assert_eq!(shell.held_radio(), 0);

    // Time passes, and nothing continues the sleep. The core stays asleep
    // until the shell sends `Start` after the restart.
    shell.advance(IDLE_TIMEOUT);
    assert!(is_asleep(&shell.model));
    assert!(shell.sleep.is_none());
    assert_eq!(shell.view().status, "Asleep");
}

/// Wakes the device from [`asleep`] while the shell holds the radio
/// requests, and answers each one until the scan waits. The other access
/// point is stronger by less than the margin, so the scan finds no better
/// access point.
fn scanning_with_no_better_access_point() -> Shell {
    let mut shell = asleep();
    shell.radio.access_points[1].rssi = -46;
    shell.hold_radio = true;
    shell.wake(Wake::Touch(None));
    shell.answer_radio();
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Scanning(_)));
    assert_eq!(shell.held_radio(), 1);
    shell.take_log();
    shell
}

#[test]
fn a_scan_that_finds_no_better_access_point_keeps_the_link_ready() {
    let mut shell = scanning_with_no_better_access_point();
    shell.hold_radio = false;
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Up(_)));
    assert_eq!(shell.radio.associated, Some(HALL));
    // The link is ready, so a link request connects with no radio request.
    wifi::link(&mut shell.model).expect_no_effects();
}

/// A restart that Home Assistant asks for during a scan does not wait for
/// the scan. After the core asked for the restart, the answer of the scan
/// changes nothing.
#[test]
fn the_chip_restart_ignores_the_answer_of_a_scan_that_ran() {
    let mut shell = scanning_with_no_better_access_point();
    shell.take_logs();
    shell.send(Event::Device(device::Event::Pressed(
        device::Button::Restart,
    )));
    assert_eq!(shell.restarts, [connection::RESTART_REASON]);
    assert_eq!(shell.model.wifi.phase, Phase::Restarting);
    assert_eq!(shell.held_radio(), 1);

    shell.answer_radio();
    assert_eq!(shell.model.wifi.phase, Phase::Restarting);
    assert_eq!(shell.take_logs(), Vec::<String>::new());
    let log = shell.take_log();
    assert!(
        !log.iter()
            .any(|entry| entry.starts_with("Radio") && entry != "RadioRestart"),
        "{log:?}"
    );
}

#[test]
fn a_session_that_fails_during_a_scan_makes_the_next_link_join_again() {
    let mut shell = scanning_with_no_better_access_point();
    fail_the_session(&mut shell);
    // The scan completes before the reconnect.
    shell.hold_radio = false;
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Up(_)));
    // The link is not ready, so a link request leaves and joins again.
    let mut cmd = wifi::link(&mut shell.model);
    assert!(
        cmd.effects()
            .any(|effect| matches!(effect, Effect::RadioJoin(_) | Effect::RadioLeave(_))),
        "the core must join again"
    );
}

#[test]
fn a_wake_during_the_scan_stop_of_a_sleep_keeps_the_link_ready() {
    let mut shell = scanning_with_no_better_access_point();
    // The sleep stops the scan, and a tap wakes the device before the
    // answer to `StopScan`.
    shell.send(Event::SleepNow);
    shell.send(Event::Tapped(None));
    shell.hold_radio = false;
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Up(_)));
    assert_eq!(shell.radio.associated, Some(HALL));
    // The link is ready, so a link request connects with no radio request.
    wifi::link(&mut shell.model).expect_no_effects();
}

#[test]
fn a_session_that_fails_again_after_a_stale_address_keeps_the_count_of_joins() {
    let mut shell = online();
    fail_the_session(&mut shell);
    shell.hold_radio = true;
    shell.advance(RECONNECT_MIN);
    // The `Leave` waits.
    shell.answer_radio();

    // The network stack keeps its address through the join, so the join
    // counts and the link goes up. The broker host does not answer the
    // connect, so the session fails again while the link is suspect.
    shell.radio.has_address = true;
    shell.broker.unreachable = true;
    shell.answer_radio();
    shell.answer_radio();
    assert!(matches!(shell.model.wifi.phase, Phase::Up(_)));
    assert_eq!(
        shell.take_logs(),
        [
            SUSPECT,
            &connect_again(LOST, 1),
            LEAVE_HALL,
            "warn wifi: the address is from before the join, so the network stack can be stuck",
            &no_new_address(1),
            &connect_again(NO_ANSWER, 2),
        ]
    );
    shell.take_log();

    // The driver is stuck now, so each join gets no address. The second
    // failure keeps the count, so the second join of the next reconnect is
    // the third that counts, and the core restarts the chip.
    shell.hold_radio = false;
    shell.radio.stuck = true;
    shell.advance(RECONNECT_MIN * 2);
    assert_eq!(
        radio(&shell.take_log()),
        [
            "RadioLeave",
            "RadioJoin Cached",
            "RadioAddress",
            "RadioLeave",
            "RadioJoin Strongest",
            "RadioAddress",
            "RadioRestart",
        ]
    );
    assert_eq!(shell.restarts, ["no address after 3 joins"]);
    assert_eq!(
        shell.take_logs(),
        [
            LEAVE_HALL,
            &no_new_address(2),
            "warn wifi: 02:00:00:00:00:01 on channel 6 gave no address, so leave it and join \
             the strongest access point",
            &no_new_address(3),
        ]
    );
}

/// Starts the house on battery, sleeps at the idle timeout, and clears the
/// logs. Unlike [`asleep`], it does not check the lines of the sleep.
fn at_the_sleep_step() -> Shell {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.take_log();
    shell.take_logs();
    shell
}

/// Returns the level and the message of each line on the log topic that
/// the broker got, in order.
fn published_lines(shell: &Shell) -> Vec<(String, String)> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == "dashboard/hall/log")
        .map(|publish| {
            let payload: serde_json::Value =
                serde_json::from_slice(&publish.payload).expect("JSON");
            (
                payload["level"].as_str().expect("a level").into(),
                payload["msg"].as_str().expect("a msg").into(),
            )
        })
        .collect()
}

#[test]
fn a_roam_after_the_wake_scan_keeps_its_line() {
    let mut shell = at_the_sleep_step();
    assert_eq!(shell.radio.move_strongest(), Some(LANDING));

    // A tap wakes the device. The scan finds a better access point, and
    // the core changes to it.
    shell.send(Event::Tapped(None));
    let change = "change to 02:00:00:00:00:02 on channel 11 (-45 dBm), because the \
                  signal of 02:00:00:00:00:01 on channel 6 is -75 dBm";
    let logs = shell.take_logs();
    assert!(logs.contains(&format!("info wifi: {change}")), "{logs:?}");
    assert_eq!(shell.radio.associated, Some(LANDING));

    // The ring kept the line, and it went to Home Assistant at Info.
    let lines = published_lines(&shell);
    assert!(lines.contains(&("info".into(), change.into())), "{lines:?}");
}

#[test]
fn a_failed_wake_scan_keeps_its_warning() {
    let mut shell = at_the_sleep_step();
    shell.hold_radio = true;

    // A tap wakes the device. The join and the address answer, and the scan
    // gets no answer in its time limit.
    shell.send(Event::Tapped(None));
    shell.answer_radio();
    shell.answer_radio();
    assert_eq!(shell.model.wifi.phase, Phase::Scanning(ready(HALL)));
    shell.advance(SCAN_TIMEOUT);
    let failed = "the scan failed, so stay on 02:00:00:00:00:01 on channel 6";
    let logs = shell.take_logs();
    assert!(logs.contains(&format!("warn wifi: {failed}")), "{logs:?}");

    // The ring kept the line, and it went to Home Assistant as a warning.
    shell.hold_radio = false;
    shell.answer_radio();
    let lines = published_lines(&shell);
    assert!(lines.contains(&("warn".into(), failed.into())), "{lines:?}");
}

#[test]
fn a_wake_scan_with_no_better_access_point_keeps_no_line() {
    let mut shell = at_the_sleep_step();
    shell.send(Event::Tapped(None));
    assert_eq!(
        shell.take_logs(),
        [
            "debug wifi: the scan found no access point that is 10 dB stronger than \
             02:00:00:00:00:01 on channel 6 (-55 dBm), so stay"
        ]
    );
    // The line is not in the ring, and it does not go to Home Assistant.
    // Only the start line went.
    assert_eq!(shell.model.logs.lines().len(), 0);
    assert_eq!(
        published_lines(&shell),
        [("info".into(), "started after a power-on".into())]
    );
}
