//! Tests of the buttons of the device in Home Assistant, with the test
//! shell and the fake broker: the routing of a button message, Sleep now,
//! Full refresh, and the restart after a clean close.

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use super::{broker::publish_of, flows::house, shell::Shell, start, tap_all_lights};
use crate::{
    Depth, Event, PowerSource,
    capabilities::log::Area,
    features::{
        connection::{CLOSE_TIMEOUT, Link, RECONNECT_MAX, RESTART_REASON},
        device::{self, Button},
        power::{self, SleepStep},
        refresh::DRAW_DELAY,
        wifi,
    },
    mqtt::{Publish, Qos, SEND_TIMEOUT, codec::server::Packet},
};

/// The button topic of the test settings.
const BUTTON: &str = "dashboard/hall/set/button";

/// The status topic of the test settings.
const STATUS: &str = "dashboard/hall/status";

/// The line for a sleep that a restart stops.
fn stalled(sleep: u32) -> String {
    format!("warn power: sleep {sleep} stops before the radio turns off, because the chip restarts")
}

/// Starts the house on `power`, connects, and clears the logs.
fn connected(power: PowerSource) -> Shell {
    let mut shell = house();
    shell.send(start(power));
    assert_eq!(shell.model.connection.link(), Link::Up);
    shell.take_log();
    shell.take_logs();
    shell
}

/// Publishes `payload` on the button topic, as Home Assistant does for a
/// press, and runs the core.
fn press(shell: &mut Shell, payload: &str) {
    shell.broker.publish(BUTTON, payload.as_bytes(), false);
    shell.run();
}

/// Sends the event of a press of `button`, as if its message arrived. A
/// message that arrives while the connection is not up never gives this
/// event, so the tests use it to start a restart in other phases.
fn pressed(shell: &mut Shell, button: Button) {
    shell.send(Event::Device(device::Event::Pressed(button)));
}

/// Returns the number of DISCONNECT packets that the broker got.
fn disconnects(shell: &Shell) -> usize {
    shell
        .broker
        .packets
        .iter()
        .filter(|packet| matches!(packet, Packet::Disconnect))
        .count()
}

/// Returns the number of PUBLISH packets that the broker got.
fn publishes(shell: &Shell) -> usize {
    shell
        .broker
        .packets
        .iter()
        .filter(|packet| matches!(packet, Packet::Publish { .. }))
        .count()
}

/// Returns `true` if `log` has an `Open` request or a radio request other
/// than the restart.
fn connects(log: &[String]) -> bool {
    log.iter().any(|entry| {
        entry == "MqttConnect" || (entry.starts_with("Radio") && entry != "RadioRestart")
    })
}

/// Returns the place of the first entry in `log` that is `entry`.
#[track_caller]
fn place(log: &[String], entry: &str) -> usize {
    log.iter()
        .position(|logged| logged == entry)
        .unwrap_or_else(|| panic!("{entry} in {log:?}"))
}

/// Expects one restart for Home Assistant, and the Wi-Fi phase that does
/// no more radio work.
#[track_caller]
fn expect_restart(shell: &Shell) {
    assert_eq!(shell.restarts, [RESTART_REASON]);
    assert_eq!(shell.restart_areas, [Area::Device]);
    assert_eq!(shell.model.wifi.phase, wifi::Phase::Restarting);
}

// The restart.

#[test]
fn a_restart_while_up_publishes_restarting_and_disconnects_then_restarts_after_the_close() {
    let mut shell = connected(PowerSource::Stand);
    let before = disconnects(&shell);
    shell.broker.hold_closes = true;

    press(&mut shell, "restart");
    assert_eq!(disconnects(&shell), before + 1);
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert!(shell.model.connection.restart_waits());
    // The DISCONNECT is clean, so the broker publishes no last will, and it
    // keeps `restarting`.
    assert_eq!(shell.broker.retained(STATUS), Some("restarting"));

    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(
        place(&log, "StreamClose") < place(&log, "RadioRestart"),
        "{log:?}"
    );
    expect_restart(&shell);
    assert_eq!(shell.broker.retained(STATUS), Some("restarting"));
    assert!(!connects(&log), "{log:?}");
    // The core writes no line of its own. The shell writes the reason.
    assert_eq!(shell.take_logs(), Vec::<String>::new());
}

#[test]
fn a_restart_publishes_only_restarting_and_then_the_disconnect() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    let before = shell.broker.packets.len();
    press(&mut shell, "restart");

    // A change of the battery and of the power source while the restart
    // waits publish nothing.
    shell.send(Event::BatteryChanged(Some(40)));
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    shell.broker.release_closes();
    shell.run();
    expect_restart(&shell);
    let after = &shell.broker.packets[before..];
    let published: Vec<Publish> = after.iter().filter_map(publish_of).collect();
    assert_eq!(
        published,
        [Publish {
            topic: STATUS.into(),
            payload: b"restarting".to_vec(),
            qos: Qos::AtLeastOnce,
            retain: true,
        }]
    );
    // The status goes before the DISCONNECT.
    let status = after
        .iter()
        .position(|packet| publish_of(packet).is_some())
        .expect("the status");
    let disconnect = after
        .iter()
        .position(|packet| matches!(packet, Packet::Disconnect))
        .expect("a DISCONNECT");
    assert!(status < disconnect, "{after:?}");
}

#[test]
fn a_restart_whose_close_gets_no_answer_restarts_after_the_close_timeout() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    let close = shell.closes.last().expect("a Close request").connection;

    shell.advance(CLOSE_TIMEOUT.saturating_sub(Duration::from_millis(1)));
    assert_eq!(shell.restarts, Vec::<String>::new());
    shell.advance(Duration::from_millis(1));
    expect_restart(&shell);
    assert_eq!(
        shell.take_logs(),
        [format!(
            "warn mqtt: the close of connection {close} got no answer in 6 s"
        )]
    );
}

#[test]
fn a_restart_with_no_stream_restarts_at_once() {
    let mut shell = connected(PowerSource::Stand);
    // The broker stops, so the session fails, and the core waits to
    // connect again.
    shell.broker.stop();
    shell.run();
    assert!(!shell.model.connection.has_stream());
    shell.take_log();
    shell.take_logs();

    let before = publishes(&shell);
    pressed(&mut shell, Button::Restart);
    let log = shell.take_log();
    assert!(!log.contains(&"StreamClose".into()), "{log:?}");
    expect_restart(&shell);
    // With no stream, the core publishes nothing.
    assert_eq!(publishes(&shell), before);

    // The reconnect timer stopped, and the core does no more radio work.
    shell.advance(RECONNECT_MAX);
    assert!(!connects(&shell.take_log()));
}

#[test]
fn a_restart_before_the_connack_ends_the_session_and_restarts_after_the_close() {
    let mut shell = house();
    shell.broker.hold_writes = true;
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.model.connection.link(), Link::Starting);
    shell.take_log();

    pressed(&mut shell, Button::Restart);
    // The session was not up, so it ends with no status and no DISCONNECT
    // packet.
    assert_eq!(publishes(&shell), 0);
    assert_eq!(disconnects(&shell), 0);
    let log = shell.take_log();
    assert!(
        place(&log, "StreamClose") < place(&log, "RadioRestart"),
        "{log:?}"
    );
    expect_restart(&shell);
}

#[test]
fn a_tap_while_a_restart_waits_does_not_connect() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    shell.take_log();

    shell.send(tap_all_lights());
    assert_eq!(shell.model.connection.outbox.len(), 1);
    assert!(shell.model.connection.restart_waits());

    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!connects(&log), "{log:?}");
    expect_restart(&shell);
}

#[test]
fn a_sleep_while_a_restart_waits_stalls_and_the_chip_restarts() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    shell.take_log();

    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let sleep = shell.model.power.sleep;
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Stalled));
    assert!(shell.model.connection.restart_waits());
    assert_eq!(shell.take_logs(), [stalled(sleep)]);

    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert!(shell.sleep.is_none());
    expect_restart(&shell);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Stalled));
}

#[test]
fn a_wake_while_a_restart_waits_does_not_connect() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    shell.take_log();

    // A tap wakes the device. The restart still waits.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    assert!(shell.model.connection.restart_waits());

    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!connects(&log), "{log:?}");
    expect_restart(&shell);
}

#[test]
fn a_restart_during_a_sleep_close_wins() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    shell.send(Event::SleepNow);
    let sleep = shell.model.power.sleep;
    assert_eq!(shell.model.connection.sleep_waiter(), Some(sleep));
    shell.take_log();
    shell.take_logs();

    pressed(&mut shell, Button::Restart);
    assert_eq!(shell.model.connection.sleep_waiter(), None);
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Stalled));

    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    assert!(!log.contains(&"WifiOff".into()), "{log:?}");
    assert!(shell.sleep.is_none());
    expect_restart(&shell);
}

#[test]
fn a_failed_end_while_a_restart_waits_writes_one_line_and_does_not_retry() {
    let mut shell = connected(PowerSource::Stand);
    let reconnects = shell.model.connection.reconnects;
    // The shell does not confirm the write of the DISCONNECT, so the send
    // timer of the session ends it with a failure.
    shell.broker.hold_writes = true;
    press(&mut shell, "restart");
    assert_eq!(shell.restarts, Vec::<String>::new());
    shell.take_logs();
    shell.advance_session(SEND_TIMEOUT);

    let logs = shell.take_logs();
    let mqtt: Vec<&String> = logs
        .iter()
        .filter(|line| line.contains(" mqtt: "))
        .collect();
    assert_eq!(mqtt.len(), 1, "{logs:?}");
    assert!(mqtt[0].starts_with("warn mqtt: "), "{logs:?}");
    assert!(mqtt[0].ends_with(", during the restart"), "{logs:?}");
    assert_eq!(shell.model.connection.reconnects, reconnects);
    expect_restart(&shell);

    shell.advance(RECONNECT_MAX);
    assert!(!connects(&shell.take_log()));
}

#[test]
fn a_restart_does_not_draw_after_the_session_ends() {
    let mut shell = connected(PowerSource::Stand);
    let frames = shell.frames.len();
    press(&mut shell, "restart");
    expect_restart(&shell);
    let log = shell.take_log();
    assert!(!log.contains(&"Render".into()), "{log:?}");
    assert_eq!(shell.frames.len(), frames);
}

#[test]
fn a_touch_wake_while_a_restart_waits_does_not_draw() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    let frames = shell.frames.len();
    let seq = shell.model.full_refresh.seq;
    shell.take_log();

    // The wake asks for a full refresh, but the reset can stop it with the
    // panel half drawn.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    let log = shell.take_log();
    assert!(!log.contains(&"Render".into()), "{log:?}");
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.model.full_refresh.seq, seq);
}

#[test]
fn nothing_draws_after_the_core_asks_to_restart_the_chip() {
    let mut shell = connected(PowerSource::Stand);
    press(&mut shell, "restart");
    expect_restart(&shell);
    // The connection is offline, so only the Wi-Fi phase tells that the
    // chip restarts.
    assert!(!shell.model.connection.restart_waits());
    let frames = shell.frames.len();
    shell.take_log();

    shell.send(Event::BatteryChanged(Some(3)));
    let log = shell.take_log();
    assert!(!log.contains(&"Render".into()), "{log:?}");
    assert_eq!(shell.frames.len(), frames);
}

#[test]
fn a_sleep_while_a_restart_waits_keeps_the_outbox_the_marks_and_the_count() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    // The connection is not up, so the call waits in the outbox.
    shell.send(tap_all_lights());
    assert_eq!(shell.model.connection.outbox.len(), 1);
    assert_eq!(shell.model.pending.marks.len(), 1);

    // The first `SleepNow` waits one more idle period for the outbox. The
    // second one sleeps, and the restart stalls the sleep.
    shell.send(Event::SleepNow);
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    assert_eq!(power::sleep_step(&shell.model), Some(SleepStep::Stalled));
    assert_eq!(shell.model.connection.outbox.len(), 1);
    assert_eq!(shell.model.pending.marks.len(), 1);
    assert_eq!(shell.model.unsent, 0);

    // A touch wake before the reset finds them unchanged.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.model.connection.outbox.len(), 1);
    assert_eq!(shell.model.pending.marks.len(), 1);
    assert_eq!(shell.model.unsent, 0);
}

// The routing.

#[test]
fn a_retained_button_message_is_ignored_with_a_line() {
    for button in Button::ALL {
        let mut shell = house();
        // A message that the broker keeps comes with the retain flag at each
        // subscribe.
        shell.broker.retain(BUTTON, button.payload().as_bytes());
        shell.send(start(PowerSource::Battery));
        assert_eq!(shell.model.connection.link(), Link::Up, "{button:?}");
        let logs = shell.take_logs();
        let line = "info device: the button message on dashboard/hall/set/button is retained, \
                    so the core ignored it";
        assert!(logs.iter().any(|logged| logged == line), "{logs:?}");
        assert_eq!(shell.restarts, Vec::<String>::new(), "{button:?}");
        assert_eq!(disconnects(&shell), 0, "{button:?}");
        assert!(!shell.model.power.is_asleep(), "{button:?}");
    }
}

#[test]
fn an_unknown_button_payload_is_ignored_with_a_warning() {
    let mut shell = connected(PowerSource::Battery);
    press(&mut shell, "explode");
    assert_eq!(
        shell.take_logs(),
        ["warn device: the button payload explode is not known, so the core ignored it"]
    );

    // The line keeps at most 32 bytes of the payload.
    press(&mut shell, &"x".repeat(40));
    assert_eq!(
        shell.take_logs(),
        [format!(
            "warn device: the button payload {} is not known, so the core ignored it",
            "x".repeat(32)
        )]
    );
    assert_eq!(shell.restarts, Vec::<String>::new());
    assert!(!shell.model.power.is_asleep());
}

#[test]
fn an_unknown_button_payload_cannot_add_or_change_a_line() {
    let mut shell = connected(PowerSource::Battery);
    press(&mut shell, "a\nwarn device: forged\x1b[31m");
    let logs = shell.take_logs();
    assert_eq!(
        logs,
        [
            "warn device: the button payload a\\nwarn device: forged\\u{1b}[31m is not known, so \
             the core ignored it"
        ]
    );
    assert!(!logs[0].contains(['\n', '\x1b']), "{logs:?}");
}

#[test]
fn a_message_on_another_set_topic_changes_nothing() {
    let mut shell = connected(PowerSource::Stand);
    shell
        .broker
        .publish("dashboard/hall/set/other", b"ON", false);
    shell.run();
    assert_eq!(shell.take_logs(), Vec::<String>::new());
    assert_eq!(shell.take_log(), Vec::<String>::new());
}

#[test]
fn a_button_during_the_close_for_a_sleep_is_dropped() {
    let mut shell = connected(PowerSource::Battery);
    // The DISCONNECT of the sleep waits, so the phase stays `Closing`.
    shell.broker.hold_writes = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.connection.sleep_waiter().is_some());

    press(&mut shell, "restart");
    assert!(!shell.model.connection.restart_waits());
    assert_eq!(shell.restarts, Vec::<String>::new());
}

// Sleep now.

#[test]
fn the_sleep_button_on_battery_sleeps_at_once() {
    let mut shell = connected(PowerSource::Battery);
    press(&mut shell, "sleep");
    assert!(shell.model.power.is_asleep());
    assert!(shell.sleep.is_some());
    let logs = shell.take_logs();
    assert_eq!(
        logs[0], "debug device: Home Assistant asked to sleep, so the sleep starts",
        "{logs:?}"
    );
}

#[test]
fn the_sleep_button_with_open_marks_waits_for_them() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.ignore_calls = true;
    shell.send(tap_all_lights());
    assert_eq!(shell.model.pending.marks.len(), 1);
    assert!(shell.model.connection.outbox.is_empty());
    shell.take_logs();

    press(&mut shell, "sleep");
    assert!(shell.model.power.sleep_waits());
    assert_eq!(
        shell.take_logs(),
        [
            "debug device: Home Assistant asked to sleep, so the sleep waits for the pending marks",
            "debug power: the sleep waits for 1 pending mark",
        ]
    );
}

#[test]
fn the_sleep_button_with_calls_in_the_outbox_waits_one_more_idle_period() {
    let mut shell = connected(PowerSource::Battery);
    // The broker stops, so the call of the tap waits in the outbox.
    shell.broker.stop();
    shell.run();
    shell.send(tap_all_lights());
    assert_eq!(shell.model.connection.outbox.len(), 1);
    shell.take_logs();

    pressed(&mut shell, Button::Sleep);
    assert!(!shell.model.power.is_asleep());
    assert!(!shell.model.power.sleep_waits());
    assert_eq!(
        shell.take_logs(),
        [
            "debug device: Home Assistant asked to sleep, but service calls wait in the outbox, \
             so the sleep waits one more idle period"
        ]
    );
}

#[test]
fn the_sleep_button_while_asleep_writes_nothing() {
    let mut shell = connected(PowerSource::Battery);
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    let sleep = shell.model.power.sleep;
    shell.take_logs();

    pressed(&mut shell, Button::Sleep);
    assert!(shell.model.power.is_asleep());
    assert_eq!(shell.model.power.sleep, sleep);
    assert_eq!(shell.take_logs(), Vec::<String>::new());
}

#[test]
fn the_sleep_button_while_a_restart_waits_is_ignored_with_a_line() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    shell.take_logs();

    pressed(&mut shell, Button::Sleep);
    assert!(!shell.model.power.is_asleep());
    assert_eq!(
        shell.take_logs(),
        [
            "debug device: Home Assistant asked to sleep, but the chip restarts, so the core ignored it"
        ]
    );
    // The idle timer still runs, so the idle time ends as usual.
    shell.advance(power::IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());
}

#[test]
fn the_sleep_button_on_the_stand_writes_one_line_and_stays_awake() {
    let mut shell = connected(PowerSource::Stand);
    press(&mut shell, "sleep");
    assert!(!shell.model.power.is_asleep());
    assert_eq!(
        shell.take_logs(),
        [
            "info device: Home Assistant asked to sleep, but the device is on the stand, so it \
             stays awake"
        ]
    );
}

#[test]
fn the_sleep_button_while_the_sleep_waits_changes_nothing() {
    let mut shell = connected(PowerSource::Battery);
    shell.broker.ignore_calls = true;
    shell.send(tap_all_lights());
    assert_eq!(shell.model.pending.marks.len(), 1);
    shell.send(Event::SleepNow);
    assert!(shell.model.power.sleep_waits());
    shell.take_logs();

    press(&mut shell, "sleep");
    assert!(shell.model.power.sleep_waits());
    assert_eq!(
        shell.take_logs(),
        ["debug device: Home Assistant asked to sleep, and the sleep already waits"]
    );
}

// Full refresh.

#[test]
fn the_refresh_button_asks_for_a_full_refresh() {
    let mut shell = connected(PowerSource::Stand);
    let seq = shell.model.full_refresh.seq;
    let frames = shell.frames.len();
    press(&mut shell, "refresh");
    assert_eq!(shell.model.full_refresh.seq, seq + 1);
    assert_eq!(shell.model.refresh.since_deep, 0);
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Deep));
    let logs = shell.take_logs();
    assert_eq!(
        logs,
        ["debug device: Home Assistant asked for a full refresh"]
    );
}

#[test]
fn the_refresh_button_while_asleep_does_nothing() {
    let mut shell = connected(PowerSource::Battery);
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    let seq = shell.model.full_refresh.seq;
    shell.take_logs();

    pressed(&mut shell, Button::Refresh);
    assert_eq!(shell.model.full_refresh.seq, seq);
    assert_eq!(
        shell.take_logs(),
        ["debug device: the device sleeps, so it skips the full refresh"]
    );
}

#[test]
fn the_refresh_button_while_a_restart_waits_is_ignored_with_a_line() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    press(&mut shell, "restart");
    let seq = shell.model.full_refresh.seq;
    shell.take_logs();

    pressed(&mut shell, Button::Refresh);
    assert_eq!(shell.model.full_refresh.seq, seq);
    assert_eq!(
        shell.take_logs(),
        [
            "debug device: Home Assistant asked for a full refresh, but the chip restarts, so the \
             core ignored it"
        ]
    );
}

#[test]
fn a_button_is_not_a_touch() {
    let mut shell = connected(PowerSource::Battery);
    let idle = shell.model.power.idle;
    let light = shell.light;
    press(&mut shell, "refresh");
    // The idle timer did not start again, and the light did not turn on.
    assert_eq!(shell.model.power.idle, idle);
    assert_eq!(shell.light, light);
    shell.advance(DRAW_DELAY);
    assert_eq!(shell.light, light);
}
