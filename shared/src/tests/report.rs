//! Tests of the hourly report on battery, and of the draw rules that the
//! report keeps. The core runs in the test shell against the fake broker.

use alloc::{string::String, vec::Vec};
use core::time::Duration;

use super::{
    broker::publish_of,
    flows::house,
    settings,
    shell::{Frame, Shell},
    start_at, tap_tile,
};
use crate::{
    Event, PowerSource, Settings, Start,
    capabilities::power::WakeSource,
    features::{
        connection::{RECONNECT_MIN, RESTART_REASON},
        power::{self, CHECK_PERIOD, IDLE_TIMEOUT, REPORT_CHECKS, REPORT_LIMIT, WakeReason},
        refresh::PARTIALS_PER_FULL,
        wifi::rules::RESTART_AFTER,
    },
    mqtt::codec::server::Packet,
};

/// The status topic of the dashboard.
const STATUS: &str = "dashboard/hall/status";

/// The topic of the buttons of the dashboard.
const BUTTON: &str = "dashboard/hall/set/button";

/// The shown entity whose state the tests change.
const ROCKET_LAMP: &str = "light.living_room_rocket_lamp";

/// Starts the house on the stand on the living room page, and clears the
/// logs of the shell.
fn living_room_on_the_stand() -> Shell {
    let mut shell = house();
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    shell.take_log();
    shell.take_logs();
    shell
}

/// Starts the house on battery on the living room page, and sleeps after
/// the idle time. Clears the logs of the shell.
fn asleep_in_the_living_room() -> Shell {
    let mut shell = house();
    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    shell.advance(IDLE_TIMEOUT);
    assert_eq!(
        power::sleep_step(&shell.model),
        Some(power::SleepStep::Sleep)
    );
    shell.take_log();
    shell.take_logs();
    shell
}

/// Answers the checks of one report period except the last one, and
/// clears the logs of the shell. The next check reports.
fn checks_before_the_report(shell: &mut Shell) {
    for _ in 1..REPORT_CHECKS {
        shell.check();
    }
    assert!(shell.sleep.is_some());
    shell.take_log();
    shell.take_logs();
}

/// Answers the checks of one report period. The last check reports.
fn report(shell: &mut Shell) {
    checks_before_the_report(shell);
    shell.check();
}

/// Returns the number of `Render` requests in `log`.
fn renders(log: &[String]) -> usize {
    log.iter().filter(|entry| *entry == "Render").count()
}

/// Returns the place of `entry` in `log`.
#[track_caller]
fn place(log: &[String], entry: &str) -> usize {
    log.iter()
        .position(|logged| logged == entry)
        .unwrap_or_else(|| panic!("no {entry} in {log:?}"))
}

/// Returns the status payloads that the dashboard published, in order.
fn statuses(shell: &Shell) -> Vec<String> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == STATUS)
        .map(|publish| String::from_utf8_lossy(&publish.payload).into_owned())
        .collect()
}

/// Returns the last two status payloads that the dashboard published.
fn last_two_statuses(shell: &Shell) -> Vec<String> {
    let statuses = statuses(shell);
    statuses[statuses.len().saturating_sub(2)..].to_vec()
}

/// Returns the lines of `logs` that the core keeps in the ring: the lines
/// that are not `Debug` lines.
fn kept(logs: &[String]) -> Vec<&str> {
    logs.iter()
        .map(String::as_str)
        .filter(|line| !line.starts_with("debug "))
        .collect()
}

/// Returns the number of PINGREQ packets that the broker got.
fn pings(shell: &Shell) -> usize {
    shell
        .broker
        .packets
        .iter()
        .filter(|packet| **packet == Packet::PingReq)
        .count()
}

/// Asserts that no timer of the core waited at a `Sleep` request.
#[track_caller]
fn assert_no_timer_at_sleep(shell: &Shell) {
    assert!(!shell.timers_at_sleep.is_empty(), "no Sleep request");
    for (index, timers) in shell.timers_at_sleep.iter().enumerate() {
        assert_eq!(timers, &Vec::<Duration>::new(), "Sleep request {index}");
    }
}

/// Asserts that the device sleeps with the touch wake and the check timer,
/// with the radio off and no timer of the core.
#[track_caller]
fn assert_sleeps(shell: &Shell) {
    let sleep = shell.sleep.as_ref().expect("a Sleep request");
    let check = u32::try_from(CHECK_PERIOD.as_secs()).unwrap();
    assert_eq!(
        sleep.wake_on,
        [WakeSource::Touch, WakeSource::Timer { seconds: check }]
    );
    assert!(!shell.model.power.is_reporting());
    assert!(!shell.radio.on);
    assert_no_timer_at_sleep(shell);
}

/// Returns the last frame on the panel.
fn last_frame(shell: &Shell) -> &Frame {
    shell.frames.last().expect("a frame")
}

#[test]
fn a_change_on_another_page_draws_nothing_while_awake() {
    let mut shell = living_room_on_the_stand();
    let frames = shell.frames.len();

    // The thermostat is on the dining room page.
    shell
        .broker
        .set_state("climate.dining_room_thermostat", "off", &[]);
    shell.run();
    let log: Vec<String> = shell.take_log();
    assert_eq!(renders(&log), 0, "{log:?}");
    assert_eq!(shell.frames.len(), frames);

    // A change on this page draws.
    shell.broker.set_state(ROCKET_LAMP, "off", &[]);
    shell.run();
    assert_eq!(renders(&shell.take_log()), 1);
    assert_eq!(shell.frames.len(), frames + 1);
}

#[test]
fn a_report_publishes_reporting_the_state_and_the_ring_then_sleeps() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    shell.send(Event::BatteryChanged(Some(81)));
    shell.advance(IDLE_TIMEOUT);
    // A wake with no cause puts an Info line in the ring.
    shell.stray();
    shell.advance(power::STRAY_HOLD);
    checks_before_the_report(&mut shell);
    // A check read a new level with the same rounded level.
    shell.send(Event::BatteryChanged(Some(79)));
    assert!(shell.sleep.is_some());
    let frames = shell.frames.len();
    let writes = shell.writes.len();

    shell.check();
    let log = shell.take_log();

    // The write of the CONNACK has `reporting` (retained) and the
    // SUBSCRIBE packets.
    let connack_write = shell.writes[writes..]
        .iter()
        .find(|write| {
            write
                .iter()
                .any(|packet| matches!(packet, Packet::Subscribe { .. }))
        })
        .expect("the write of the CONNACK");
    let status = connack_write
        .iter()
        .find_map(publish_of)
        .expect("a publish in the write of the CONNACK");
    assert_eq!(status.topic, STATUS);
    assert_eq!(status.payload, b"reporting");
    assert!(status.retain);

    // The state has the new level, and the line of the ring goes.
    let state = log
        .iter()
        .find(|entry| entry.starts_with("MqttPublish dashboard/hall/state "))
        .expect("the state message");
    assert!(state.contains(r#""battery":79"#), "{state}");
    assert!(state.contains(r#""wake":"boot""#), "{state}");
    let line = log
        .iter()
        .find(|entry| entry.starts_with("MqttPublish dashboard/hall/log "))
        .expect("a line of the log");
    assert!(line.contains("woke with no cause"), "{line}");
    assert_eq!(shell.model.logs.lines().len(), 0);

    // After the PUBACKs and the PINGRESP, the steps of the sleep run.
    assert_eq!(pings(&shell), 1);
    let asleep = place(&log, "MqttPublish dashboard/hall/status asleep");
    assert!(place(&log, "MqttSubscribe") < asleep);
    assert!(asleep < place(&log, "MqttDisconnect"));
    assert!(place(&log, "MqttDisconnect") < place(&log, "StreamClose"));
    assert!(place(&log, "StreamClose") < place(&log, "WifiOff"));
    assert_eq!(log.last().map(String::as_str), Some("Sleep"));
    assert_eq!(renders(&log), 0, "{log:?}");
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(last_two_statuses(&shell), ["reporting", "asleep"]);
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));
    assert_eq!(shell.model.connection.reconnects, 0);
    assert_sleeps(&shell);
}

#[test]
fn a_report_with_no_change_does_not_render() {
    let mut shell = asleep_in_the_living_room();
    let frames = shell.frames.len();

    report(&mut shell);
    let log = shell.take_log();
    assert!(log.contains(&"MqttPublish dashboard/hall/status reporting".into()));
    assert_eq!(renders(&log), 0, "{log:?}");
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(
        shell.take_logs(),
        [
            "debug power: sleep 1 reports to Home Assistant",
            "debug power: the report of sleep 1 is complete",
            "debug power: sleep 2 closes the connection",
            "debug power: sleep 2 turns the radio off",
            "debug power: sleep 2 keeps the image and sleeps",
        ]
    );
    assert_sleeps(&shell);
}

#[test]
fn a_report_with_a_changed_entity_renders_once() {
    let mut shell = asleep_in_the_living_room();
    let frames = shell.frames.len();
    let before = last_frame(&shell).view.clone();
    // Home Assistant turns the lamp off two minutes later.
    shell.advance(Duration::from_secs(120));
    shell.broker.set_state(ROCKET_LAMP, "off", &[]);

    report(&mut shell);
    let log = shell.take_log();
    assert_eq!(renders(&log), 1, "{log:?}");
    let render = place(&log, "Render");
    assert!(place(&log, "WifiOff") < render, "{log:?}");
    assert!(render < place(&log, "Sleep"), "{log:?}");

    assert_eq!(shell.frames.len(), frames + 1);
    let frame = last_frame(&shell);
    assert!(!frame.full);
    assert_eq!(frame.view.full_refresh_seq, before.full_refresh_seq);
    assert_ne!(frame.view.page, before.page);
    assert_eq!(before.updated, "Last changed 09:41");
    assert_eq!(frame.view.updated, "Last changed 09:43");
    assert!(frame.view.asleep);
    assert_eq!(frame.view.status, "Asleep");
    assert!(
        shell
            .take_logs()
            .contains(&"debug power: sleep 2 draws the changes and sleeps".into())
    );
    assert_sleeps(&shell);
}

#[test]
fn a_time_only_change_does_not_render() {
    let mut shell = asleep_in_the_living_room();
    let frames = shell.frames.len();
    // The temperature updates with the same text two minutes later.
    shell.advance(Duration::from_secs(120));
    shell.broker.set_state(
        "sensor.living_room_thermometer_temperature",
        "21.4",
        &[("unit_of_measurement", "\"°C\"")],
    );

    report(&mut shell);
    assert_eq!(renders(&shell.take_log()), 0);
    assert_eq!(shell.frames.len(), frames);
    // The panel keeps the old time, but the view has the new one.
    assert_eq!(last_frame(&shell).view.updated, "Last changed 09:41");
    assert_eq!(shell.view().updated, "Last changed 09:43");

    // The next report with a real change draws the newest time.
    shell.advance(Duration::from_secs(120));
    shell.broker.set_state(ROCKET_LAMP, "off", &[]);
    report(&mut shell);
    assert_eq!(renders(&shell.take_log()), 1);
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(last_frame(&shell).view.updated, "Last changed 09:45");
    assert_sleeps(&shell);
}

#[test]
fn a_change_on_another_page_does_not_render_in_a_report() {
    let mut shell = asleep_in_the_living_room();
    let frames = shell.frames.len();
    shell.advance(Duration::from_secs(120));
    // The thermostat is on the dining room page.
    shell
        .broker
        .set_state("climate.dining_room_thermostat", "off", &[]);

    report(&mut shell);
    assert_eq!(renders(&shell.take_log()), 0);
    assert_eq!(shell.frames.len(), frames);
    assert_sleeps(&shell);
}

#[test]
fn a_new_rounded_battery_level_renders_also_when_the_report_fails() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    shell.send(Event::BatteryChanged(Some(80)));
    shell.advance(IDLE_TIMEOUT);
    checks_before_the_report(&mut shell);
    let frames = shell.frames.len();
    shell.send(Event::BatteryChanged(Some(74)));
    shell.radio.blocked = true;

    shell.check();
    assert!(shell.model.power.is_reporting());
    shell.advance(REPORT_LIMIT);
    let log = shell.take_log();
    assert_eq!(renders(&log), 1, "{log:?}");
    assert!(place(&log, "WifiOff") < place(&log, "Render"));
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(last_frame(&shell).view.battery, Some(75));
    assert!(!last_frame(&shell).full);
    assert_sleeps(&shell);
}

#[test]
fn the_render_after_a_report_is_full_after_the_last_partial() {
    let mut shell = asleep_in_the_living_room();
    let seq = last_frame(&shell).view.full_refresh_seq;
    shell.model.refresh.partials = PARTIALS_PER_FULL;
    shell.broker.set_state(ROCKET_LAMP, "off", &[]);

    report(&mut shell);
    assert_eq!(renders(&shell.take_log()), 1);
    let frame = last_frame(&shell);
    assert!(frame.full);
    assert_eq!(frame.view.full_refresh_seq, seq + 1);
    assert_eq!(shell.model.refresh.partials, 0);
    assert_sleeps(&shell);
}

#[test]
fn a_message_after_the_subscribe_but_before_the_pingresp_is_drawn() {
    let mut shell = asleep_in_the_living_room();
    let frames = shell.frames.len();
    shell.broker.hold_pingresp = true;

    report(&mut shell);
    // The subscribe ended, and the PINGREQ went after it. The report
    // waits for the PINGRESP.
    assert!(shell.model.power.is_reporting());
    assert_eq!(pings(&shell), 1);
    let packets = &shell.broker.packets;
    let subscribe = packets
        .iter()
        .rposition(|packet| matches!(packet, Packet::Subscribe { .. }))
        .unwrap();
    let ping = packets
        .iter()
        .position(|packet| *packet == Packet::PingReq)
        .unwrap();
    assert!(subscribe < ping);
    let log = shell.take_log();
    assert!(!log.contains(&"MqttPublish dashboard/hall/status asleep".into()));

    // A change arrives before the PINGRESP.
    shell.broker.set_state(ROCKET_LAMP, "off", &[]);
    assert!(shell.model.power.is_reporting());
    assert_eq!(renders(&shell.take_log()), 0);

    shell.broker.hold_pingresp = false;
    shell.broker.release_pingresps();
    shell.broker.flush();
    shell.run();
    let log = shell.take_log();
    assert_eq!(renders(&log), 1, "{log:?}");
    assert!(place(&log, "MqttDisconnect") < place(&log, "Render"));
    assert_eq!(shell.frames.len(), frames + 1);
    assert_sleeps(&shell);
}

#[test]
fn a_report_does_not_scan() {
    let mut shell = asleep_in_the_living_room();

    report(&mut shell);
    let log = shell.take_log();
    // The join is to the cached access point, so a scan is due.
    assert!(log.contains(&"RadioJoin Cached".into()), "{log:?}");
    assert!(!log.contains(&"RadioScan".into()), "{log:?}");
    assert!(shell.model.wifi.cache.scan_due());
    assert_sleeps(&shell);

    // The next wake scans.
    shell.send(Event::Tapped(None));
    assert!(shell.take_log().contains(&"RadioScan".into()));
}

#[test]
fn the_retained_replay_in_a_report_starts_no_draw_timer() {
    let mut shell = asleep_in_the_living_room();
    shell.auto_draw = false;
    shell.broker.hold_pingresp = true;
    shell.broker.set_state(ROCKET_LAMP, "off", &[]);

    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    assert!(!shell.model.refresh.draw_waits());
    // Only the limit timer waits.
    assert_eq!(shell.waiting(), [REPORT_LIMIT]);
}

#[test]
fn joins_in_a_report_do_not_count_toward_the_restart() {
    let mut shell = asleep_in_the_living_room();
    // A session failed before the sleep, so the link is suspect, and DHCP
    // gives no address.
    shell.model.wifi.health.failed();
    shell.radio.stuck = true;

    // Three reports, each with more joins with no address than the count
    // of a restart.
    for _ in 0..3 {
        report(&mut shell);
        shell.advance(REPORT_LIMIT);
        let log = shell.take_log();
        let joins = log
            .iter()
            .filter(|entry| entry.starts_with("RadioJoin"))
            .count();
        assert!(joins > usize::try_from(RESTART_AFTER).unwrap(), "{log:?}");
        assert!(!log.contains(&"RadioRestart".into()), "{log:?}");
        let logs = shell.take_logs();
        assert!(
            !logs
                .iter()
                .any(|line| line.contains("joins got no new address")),
            "{logs:?}"
        );
        assert_sleeps(&shell);
    }
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert_eq!(shell.model.wifi.health.joins_with_no_lease(), 0);
    assert!(shell.model.wifi.health.suspect());

    // While awake, the joins still count, and the third restarts the chip.
    shell.send(Event::Tapped(None));
    shell.advance(RECONNECT_MIN * 3);
    assert_eq!(shell.restarts, ["no address after 3 joins"]);
}

#[test]
fn a_report_ends_at_its_limit_when_the_join_fails() {
    let mut shell = asleep_in_the_living_room();
    // A wake with no cause puts an Info line in the ring.
    shell.stray();
    shell.advance(power::STRAY_HOLD);
    let ring = shell.model.logs.lines().len();
    assert_eq!(ring, 1);
    shell.radio.blocked = true;

    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    shell.advance(REPORT_LIMIT);
    let log = shell.take_log();
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"));
    let logs = shell.take_logs();
    let kept = kept(&logs);
    assert_eq!(kept.len(), 1, "{logs:?}");
    assert!(
        kept[0].starts_with("info power: the report of sleep 1 stopped after 30 s: "),
        "{kept:?}"
    );
    assert!(
        kept[0].ends_with("tries failed, the last because the link is down"),
        "{kept:?}"
    );
    // The ring keeps its line and the line of the limit.
    assert_eq!(shell.model.logs.lines().len(), ring + 1);
    assert_eq!(shell.model.connection.reconnects, 0);
    assert_sleeps(&shell);
}

#[test]
fn a_report_ends_at_its_limit_when_the_broker_is_down() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.stop();

    report(&mut shell);
    shell.advance(REPORT_LIMIT);
    let log = shell.take_log();
    let connects = log.iter().filter(|entry| *entry == "MqttConnect").count();
    // The first try, and the retries after 1, 2, 4 and 8 s.
    assert_eq!(connects, 5, "{log:?}");
    let logs = shell.take_logs();
    assert_eq!(
        kept(&logs),
        [
            "info power: the report of sleep 1 stopped after 30 s: 5 tries failed, the last \
             because the broker host ended the stream: connection refused"
        ],
        "{logs:?}"
    );
    assert_eq!(shell.model.connection.reconnects, 0);
    assert_sleeps(&shell);
}

#[test]
fn a_report_settles_only_after_the_log_clock_and_every_puback() {
    let mut shell = asleep_in_the_living_room();
    shell.stray();
    shell.advance(power::STRAY_HOLD);
    shell.broker.hold_puback = true;
    shell.hold_clock = true;

    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    assert_eq!(shell.held_clocks(), 1);

    // The clock comes, and the line goes, but its PUBACK waits.
    shell.answer_clock(0);
    assert!(shell.model.power.is_reporting());
    assert!(
        shell
            .take_log()
            .iter()
            .any(|entry| entry.starts_with("MqttPublish dashboard/hall/log "))
    );

    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert!(
        shell
            .take_log()
            .contains(&"MqttPublish dashboard/hall/status asleep".into())
    );
    assert_eq!(shell.model.logs.lines().len(), 0);
    assert_sleeps(&shell);
}

#[test]
fn a_tap_during_a_report_wakes_publishes_online_and_acts() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    shell.take_log();
    shell.take_cleared();
    let frames = shell.frames.len();

    // The lamp tile.
    shell.send(tap_tile(1));
    let log = shell.take_log();
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.model.power.wake(), WakeReason::Touch);
    assert!(
        log.contains(&"MqttPublish dashboard/hall/status online".into()),
        "{log:?}"
    );
    assert_eq!(shell.broker.service_calls.len(), 1, "{log:?}");
    assert!(shell.frames.len() > frames);
    assert!(shell.frames[frames].full);
    assert!(shell.take_cleared().contains(&REPORT_LIMIT));
    assert!(shell.waiting().contains(&IDLE_TIMEOUT));
    assert_eq!(shell.broker.retained(STATUS), Some("online"));
    assert_eq!(shell.view().status, "Online");
}

#[test]
fn a_tap_while_the_report_joins_gets_online_at_the_connack() {
    let mut shell = asleep_in_the_living_room();
    shell.hold_radio = true;
    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    assert_eq!(shell.held_radio(), 1);
    let before = statuses(&shell).len();

    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    shell.hold_radio = false;
    shell.answer_radio();
    assert_eq!(statuses(&shell)[before..], ["online"]);
    assert_eq!(shell.view().status, "Online");
}

#[test]
fn the_stand_during_a_report_wakes_and_publishes_online() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    shell.take_cleared();

    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.model.power.wake(), WakeReason::Stand);
    assert_eq!(last_two_statuses(&shell), ["reporting", "online"]);
    assert!(shell.take_cleared().contains(&REPORT_LIMIT));
    assert!(last_frame(&shell).full);
}

#[test]
fn start_during_a_report_clears_the_limit() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    shell.take_cleared();

    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    assert!(!shell.model.power.is_asleep());
    assert!(shell.take_cleared().contains(&REPORT_LIMIT));
    // The limit and the idle timer have the same length, so only the idle
    // timer waits.
    assert_eq!(REPORT_LIMIT, IDLE_TIMEOUT);
    let waits = shell.waiting();
    assert_eq!(
        waits.iter().filter(|wait| **wait == REPORT_LIMIT).count(),
        1
    );
}

#[test]
fn a_light_command_during_a_report_is_ignored_and_the_light_stays_off() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    shell.take_log();

    shell
        .broker
        .publish("dashboard/hall/set/light", br#"{"state":"ON"}"#, false);
    shell.run();
    let log = shell.take_log();
    assert!(
        !log.iter().any(|entry| entry.starts_with("FrontLight")),
        "{log:?}"
    );
    assert_eq!(shell.light, 0);
    assert!(shell.model.power.is_reporting());
}

#[test]
fn the_buttons_during_a_report() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    shell.take_log();
    let frames = shell.frames.len();

    // Sleep now and Full refresh change nothing.
    for payload in ["sleep", "refresh"] {
        shell.broker.publish(BUTTON, payload.as_bytes(), false);
        shell.run();
        assert!(shell.model.power.is_reporting(), "{payload}");
    }
    assert_eq!(renders(&shell.take_log()), 0);
    assert_eq!(shell.frames.len(), frames);

    // Restart restarts the chip after the clean close.
    shell.broker.publish(BUTTON, b"restart", false);
    shell.run();
    assert_eq!(shell.broker.retained(STATUS), Some("restarting"));
    assert_eq!(shell.restarts, [RESTART_REASON]);
}

#[test]
fn a_report_in_a_dry_run_sends_only_the_log() {
    let mut shell = house();
    let Event::Start(start) = start_at(PowerSource::Battery, Some("/room/living_room")) else {
        unreachable!("a start");
    };
    shell.send(Event::Start(Start {
        settings: Settings {
            read_only: true,
            ..settings()
        },
        ..start
    }));
    shell.advance(IDLE_TIMEOUT);
    shell.stray();
    shell.advance(power::STRAY_HOLD);
    let published = shell.broker.published.len();

    report(&mut shell);
    let topics: Vec<String> = shell.broker.published[published..]
        .iter()
        .map(|publish| publish.topic.clone())
        .collect();
    // The status, the line, then `asleep`.
    assert_eq!(topics, [STATUS, "dashboard/hall/log", STATUS]);
    assert_eq!(last_two_statuses(&shell), ["reporting", "asleep"]);
    assert_sleeps(&shell);
}

#[test]
fn the_summary_of_the_wakes_with_no_cause_goes_with_the_report() {
    let mut shell = asleep_in_the_living_room();
    for _ in 0..2 {
        shell.stray();
        shell.advance(power::STRAY_HOLD);
    }
    shell.take_logs();

    report(&mut shell);
    assert!(
        shell
            .take_logs()
            .contains(&"info power: sleep 1 had 2 wakes with no cause".into())
    );
    let log = shell.take_log();
    assert!(
        log.iter().any(|entry| {
            entry.starts_with("MqttPublish dashboard/hall/log ")
                && entry.contains("sleep 1 had 2 wakes with no cause")
        }),
        "{log:?}"
    );
    assert_sleeps(&shell);
}

#[test]
fn a_report_ends_at_its_limit_when_a_puback_does_not_come() {
    let mut shell = asleep_in_the_living_room();
    // A wake with no cause puts an Info line in the ring.
    shell.stray();
    shell.advance(power::STRAY_HOLD);
    shell.broker.hold_puback = true;

    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    let log = shell.take_log();
    assert!(
        log.iter()
            .any(|entry| entry.starts_with("MqttPublish dashboard/hall/log ")),
        "{log:?}"
    );

    // The session is up at the limit, so the steps publish `asleep` and
    // send DISCONNECT.
    shell.advance(REPORT_LIMIT);
    let log = shell.take_log();
    assert!(
        place(&log, "MqttPublish dashboard/hall/status asleep") < place(&log, "MqttDisconnect")
    );
    assert!(place(&log, "WifiOff") < place(&log, "Sleep"));
    assert_eq!(
        kept(&shell.take_logs()),
        [
            "info power: the report of sleep 1 stopped after 30 s: the publishes or the \
             retained messages did not end"
        ]
    );
    // The line got no PUBACK, so the ring keeps it, with the line of the
    // limit.
    assert_eq!(shell.model.logs.lines().len(), 2);
    assert_sleeps(&shell);

    // The next report sends both lines.
    shell.broker.hold_puback = false;
    report(&mut shell);
    let lines = shell
        .take_log()
        .into_iter()
        .filter(|entry| entry.starts_with("MqttPublish dashboard/hall/log "))
        .count();
    assert_eq!(lines, 2);
    assert_eq!(shell.model.logs.lines().len(), 0);
    assert_sleeps(&shell);
}

#[test]
fn the_taps_that_a_sleep_dropped_stay_on_the_image_through_a_report() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    // The broker is down, so the call of a tap waits in the outbox, and
    // the sleep drops it.
    shell.broker.stop();
    shell.run();
    shell.send(tap_tile(1));
    shell.advance(IDLE_TIMEOUT);
    shell.advance(IDLE_TIMEOUT);
    assert_eq!(
        power::sleep_step(&shell.model),
        Some(power::SleepStep::Sleep)
    );
    assert_eq!(shell.model.unsent, 1);
    assert_eq!(last_frame(&shell).view.status, "Asleep, 1 tap not sent");
    shell.broker.down = false;
    let frames = shell.frames.len();

    // A report does not change the image.
    report(&mut shell);
    assert_eq!(renders(&shell.take_log()), 0);
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.model.unsent, 1);
    assert_sleeps(&shell);

    // A tap during the next report publishes `online` and clears the count.
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    assert!(shell.model.power.is_reporting());
    shell.send(Event::Tapped(None));
    assert_eq!(shell.model.unsent, 0);
    assert_eq!(statuses(&shell).last().map(String::as_str), Some("online"));
    assert_eq!(shell.view().status, "Online");
}

#[test]
fn a_wake_after_the_subscribe_of_a_report_scans() {
    let mut shell = asleep_in_the_living_room();
    shell.broker.hold_pingresp = true;
    report(&mut shell);
    assert!(shell.model.wifi.cache.scan_due());
    assert!(!shell.take_log().contains(&"RadioScan".into()));

    shell.send(Event::Tapped(None));
    assert!(shell.take_log().contains(&"RadioScan".into()));
}
