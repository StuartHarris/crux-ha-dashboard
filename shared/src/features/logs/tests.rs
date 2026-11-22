//! Tests of the logs feature: the time of a line, the ring, the cut of a
//! long message, and the publish of the lines to Home Assistant.

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use crux_core::App;
use serde_json::Value;

use super::{
    CAPACITY, CUT_MARK, LOG_IN_FLIGHT, Line, MAX_MESSAGE, MAX_SOURCE, Message, Payload,
    ShellSource, Source, State, dropped_json, info, warn,
};
use crate::{
    Dashboard, EffectTestExt, Event, Model, PowerSource,
    capabilities::{
        log::{Area, Level, Log, Uptime},
        power::Wake,
    },
    features::{
        connection::{Link, RECONNECT_MIN},
        power::IDLE_TIMEOUT,
    },
    mqtt::codec::server::Packet,
    tests::{broker::publish_of, flows::house, shell::Shell, start, tap_all_lights},
};

/// The log topic of the test settings.
const LOG_TOPIC: &str = "dashboard/hall/log";

/// The status topic of the test settings.
const STATUS: &str = "dashboard/hall/status";

fn line(at: u64, message: &str) -> Line {
    Line {
        at: Uptime(at),
        level: Level::Info,
        source: Source::Core(Area::Wifi),
        message: Message::new(message),
    }
}

fn texts(state: &State) -> Vec<&str> {
    state.lines().map(|line| line.message.as_str()).collect()
}

/// Returns the time and the text of each line in the ring of `model`
/// with the area `power`.
fn power_lines(model: &Model) -> Vec<(u64, String)> {
    model
        .logs
        .lines()
        .filter(|line| line.source == Source::Core(Area::Power))
        .map(|line| (line.at.0, String::from(line.message.as_str())))
        .collect()
}

#[test]
fn a_line_has_the_time_of_the_answer_of_the_shell() {
    let app = Dashboard;
    let mut model = Model::default();

    let mut cmd = info(Area::Wifi, "the scan found no better access point");
    let event = cmd
        .resolve_log(|log| {
            assert_eq!(
                *log,
                Log {
                    level: Level::Info,
                    area: Area::Wifi,
                    message: "the scan found no better access point".into(),
                }
            );
            Uptime(1_000)
        })
        .expect_one_event();
    cmd.expect_done();
    assert!(model.logs.lines().next().is_none());
    app.update(event, &mut model).expect_done();

    // The clock of the shell moves on before the next line.
    let mut cmd = warn(Area::Mqtt, "the broker refused one or more topic filters");
    let event = cmd.resolve_log(|_| Uptime(2_500)).expect_one_event();
    app.update(event, &mut model).expect_done();

    let lines: Vec<_> = model
        .logs
        .lines()
        .map(|line| (line.at, line.level, line.source.clone()))
        .collect();
    assert_eq!(
        lines,
        [
            (Uptime(1_000), Level::Info, Source::Core(Area::Wifi)),
            (Uptime(2_500), Level::Warn, Source::Core(Area::Mqtt)),
        ]
    );
    assert_eq!(
        texts(&model.logs),
        [
            "the scan found no better access point",
            "the broker refused one or more topic filters"
        ]
    );
    assert_eq!(model.logs.dropped(), 0);
}

#[test]
fn a_line_waits_for_the_answer_of_the_shell() {
    let mut cmd = info(Area::Power, "sleep 1 closes the connection");
    let effect = cmd.expect_one_effect();

    // The command waits while the shell keeps the request.
    cmd.expect_no_effect_or_events();
    assert!(!cmd.is_done());

    // The test helpers drop a `Log` request with no answer. The command
    // then ends with no event, so the core keeps no line.
    drop(effect);
    cmd.expect_no_events();
    cmd.expect_done();
}

#[test]
fn a_full_ring_drops_the_oldest_line_and_counts_it() {
    let mut state = State::default();
    for at in 0..CAPACITY as u64 {
        state.push(line(at, &format!("line {at}")));
    }
    assert_eq!(state.lines().len(), CAPACITY);
    assert_eq!(state.dropped(), 0);

    state.push(line(64, "line 64"));
    state.push(line(65, "line 65"));

    assert_eq!(state.lines().len(), CAPACITY);
    assert_eq!(state.dropped(), 2);
    let first = state.lines().next().expect("a line");
    assert_eq!(first.message.as_str(), "line 2");
    let last = state.lines().next_back().expect("a line");
    assert_eq!(last.message.as_str(), "line 65");
}

#[test]
fn a_long_message_is_cut_on_a_character_boundary() {
    // `é` has 2 bytes. It starts at the byte 196, so the cut for the
    // mark (3 bytes) at the byte 197 is in the character.
    let text = format!("{}é{}", "a".repeat(196), "b".repeat(20));
    assert!(!text.is_char_boundary(MAX_MESSAGE - CUT_MARK.len()));

    let message = Message::new(&text);

    assert_eq!(message.as_str(), format!("{}{CUT_MARK}", "a".repeat(196)));
    assert!(message.as_str().len() <= MAX_MESSAGE);
}

#[test]
fn a_message_that_fits_is_not_cut() {
    let text = "a".repeat(MAX_MESSAGE);
    assert_eq!(Message::new(&text).as_str(), text);
    assert_eq!(Message::new("").as_str(), "");
}

#[test]
fn the_shell_writes_the_whole_message_and_the_ring_keeps_the_cut_message() {
    let app = Dashboard;
    let mut model = Model::default();
    let text = "x".repeat(MAX_MESSAGE + 50);

    let mut cmd = warn(Area::Wifi, text.clone());
    let event = cmd
        .resolve_log(|log| {
            assert_eq!(log.message, text);
            Uptime(7)
        })
        .expect_one_event();
    app.update(event, &mut model).expect_done();

    let kept = model.logs.lines().next().expect("a line");
    assert_eq!(kept.message, Message::new(&text));
    assert_eq!(kept.message.as_str().len(), MAX_MESSAGE);
}

// The payload.

#[test]
fn an_info_line_has_its_age_in_whole_seconds() {
    let line = Line {
        at: Uptime(3_000),
        level: Level::Info,
        source: Source::Core(Area::Wifi),
        message: Message::new("the scan found no better access point, so stay"),
    };

    let json = Payload::of(&line, Uptime(15_999)).to_json();

    assert_eq!(
        String::from_utf8(json).unwrap(),
        r#"{"level":"info","source":"wifi","age":12,"msg":"the scan found no better access point, so stay"}"#
    );
}

#[test]
fn a_warn_line_escapes_its_message() {
    let line = Line {
        at: Uptime(0),
        level: Level::Warn,
        source: Source::Core(Area::Mqtt),
        message: Message::new("a \"quoted\" topic\nand a new line"),
    };

    let json = Payload::of(&line, Uptime(61_000)).to_json();

    assert_eq!(
        String::from_utf8(json).unwrap(),
        r#"{"level":"warn","source":"mqtt","age":61,"msg":"a \"quoted\" topic\nand a new line"}"#
    );
}

#[test]
fn a_line_newer_than_the_clock_has_the_age_0() {
    let line = Line {
        at: Uptime(9_000),
        level: Level::Info,
        source: Source::Core(Area::Power),
        message: Message::new("sleep 1 closes the connection"),
    };

    let json = Payload::of(&line, Uptime(5_000)).to_json();

    assert_eq!(
        String::from_utf8(json).unwrap(),
        r#"{"level":"info","source":"power","age":0,"msg":"sleep 1 closes the connection"}"#
    );
}

#[test]
fn the_drop_message_is_a_warn_line_of_the_log_with_the_age_0() {
    assert_eq!(
        String::from_utf8(dropped_json(3)).unwrap(),
        r#"{"level":"warn","source":"log","age":0,"msg":"3 older lines were dropped"}"#
    );
}

#[test]
fn the_drop_message_for_one_line_is_singular() {
    assert_eq!(
        String::from_utf8(dropped_json(1)).unwrap(),
        r#"{"level":"warn","source":"log","age":0,"msg":"1 older line was dropped"}"#
    );
}

// The publish, with the test shell and the fake broker.

/// Keeps a line with the time `at` in seconds, as if the shell answered its
/// `Log` request, and answers the effects until the core waits.
fn stamp(shell: &mut Shell, at_secs: u64, text: &str) {
    let line = Line {
        at: Uptime(at_secs * 1_000),
        level: Level::Info,
        source: Source::Core(Area::Wifi),
        message: Message::new(text),
    };
    shell.send(Event::Logs(super::Event::Stamped(line)));
}

/// Returns the payload of each message on the log topic that the broker
/// got, in order, as JSON.
fn published(shell: &Shell) -> Vec<Value> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == LOG_TOPIC)
        .map(|publish| serde_json::from_slice(&publish.payload).expect("JSON"))
        .collect()
}

/// Returns the `msg` of each message on the log topic that the broker got,
/// in order.
fn published_texts(shell: &Shell) -> Vec<String> {
    published(shell)
        .iter()
        .map(|payload| payload["msg"].as_str().expect("a msg").into())
        .collect()
}

/// Returns the topics of the publishes in a write.
fn topics(write: &[Packet]) -> Vec<String> {
    write
        .iter()
        .filter_map(publish_of)
        .map(|publish| publish.topic)
        .collect()
}

/// Starts on the stand, and connects. No line waits.
fn online() -> Shell {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(shell.model.logs.lines().len(), 0);
    shell
}

#[test]
fn the_lines_of_a_battery_sleep_go_out_after_the_wake_with_their_ages() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.advance(Duration::from_secs(5));

    // Off the stand, the device sleeps after the idle timeout. Each step
    // of the sleep writes a line while the connection closes or is down, so
    // no line goes to Home Assistant before the sleep.
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    let slept_at = u64::try_from(shell.now().as_millis()).unwrap();
    let first = power_lines(&shell.model);
    assert_eq!(
        first,
        [
            (slept_at, "sleep 1 closes the connection".into()),
            (slept_at, "sleep 1 turns the radio off".into()),
            (slept_at, "sleep 1 draws the last image and sleeps".into()),
        ]
    );
    assert_eq!(published(&shell), Vec::<Value>::new());

    // An hour later, a touch wakes the device. After the connect, the
    // lines go with their ages on the clock of the shell.
    shell.advance(Duration::from_secs(3_600));
    let writes = shell.writes.len();
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    let woke_at = u64::try_from(shell.now().as_millis()).unwrap();
    assert_eq!((woke_at - slept_at) / 1_000, 3_600);
    let payloads: Vec<String> = shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == LOG_TOPIC)
        .map(|publish| String::from_utf8(publish.payload.clone()).unwrap())
        .collect();
    assert_eq!(
        payloads,
        [
            r#"{"level":"info","source":"power","age":3600,"msg":"sleep 1 closes the connection"}"#,
            r#"{"level":"info","source":"power","age":3600,"msg":"sleep 1 turns the radio off"}"#,
            r#"{"level":"info","source":"power","age":3600,"msg":"sleep 1 draws the last image and sleeps"}"#,
            // The scan after the wake writes a new line. It goes at once.
            r#"{"level":"info","source":"wifi","age":0,"msg":"the scan found no access point that is 10 dB stronger than 02:00:00:00:00:01 on channel 6 (-55 dBm), so stay"}"#,
        ]
    );
    assert!(
        shell
            .broker
            .published
            .iter()
            .all(|publish| publish.qos == crate::mqtt::Qos::AtLeastOnce
                || publish.topic != LOG_TOPIC)
    );
    assert!(
        shell
            .broker
            .published
            .iter()
            .filter(|publish| publish.topic == LOG_TOPIC)
            .all(|publish| !publish.retain)
    );

    // The writes of the wake: the CONNECT, the CONNACK write, the write of
    // the 3 lines of the sleep (326 bytes), and the write of the line of
    // the scan.
    let bytes = &shell.write_bytes[writes..];
    assert_eq!(bytes.len(), 4, "{bytes:?}");
    assert_eq!(bytes[2], 326, "{bytes:?}");

    // The broker acknowledged each line, so the ring is empty.
    assert_eq!(shell.model.logs.lines().len(), 0);
    assert_eq!(shell.model.logs.dropped(), 0);
}

#[test]
fn the_connack_write_is_unchanged_and_the_log_goes_in_a_later_write() {
    let mut shell = house();
    stamp(&mut shell, 0, "a line before the start");
    stamp(&mut shell, 1, "a second line before the start");

    shell.send(start(PowerSource::Stand));

    // The first write is the CONNECT. The CONNACK write has `online` and
    // the subscribe only.
    let connack = &shell.writes[1];
    assert_eq!(topics(connack), ["dashboard/hall/status"]);
    assert!(
        connack
            .iter()
            .all(|packet| matches!(packet, Packet::Publish { .. } | Packet::Subscribe { .. }))
    );
    // The lines go in the next write.
    assert_eq!(topics(&shell.writes[2]), [LOG_TOPIC, LOG_TOPIC]);
    assert_eq!(
        published_texts(&shell),
        ["a line before the start", "a second line before the start"]
    );
    assert_eq!(shell.clocks, 1);
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn the_drop_message_comes_first_and_the_count_resets_only_at_its_puback() {
    let mut shell = house();
    for at in 0..CAPACITY as u64 + 3 {
        stamp(&mut shell, at, &format!("line {at}"));
    }
    assert_eq!(shell.model.logs.dropped(), 3);

    shell.broker.hold_puback = true;
    shell.send(start(PowerSource::Stand));

    let payloads = published(&shell);
    assert_eq!(
        payloads[0],
        serde_json::json!({"level": "warn", "source": "log", "age": 0, "msg": "3 older lines were dropped"})
    );
    assert_eq!(payloads[1]["msg"], "line 3");
    // No PUBACK came, so the count and the lines stay.
    assert_eq!(shell.model.logs.dropped(), 3);
    assert_eq!(shell.model.logs.lines().len(), CAPACITY);

    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();

    assert_eq!(shell.model.logs.dropped(), 0);
    assert_eq!(shell.model.logs.lines().len(), 0);
    // Each line went once, in order, after the drop message.
    let texts = published_texts(&shell);
    let lines: Vec<String> = (3..CAPACITY as u64 + 3)
        .map(|at| format!("line {at}"))
        .collect();
    assert_eq!(texts[1..], lines[..]);
}

#[test]
fn a_session_that_ends_before_the_puback_sends_the_lines_again_in_order() {
    let mut shell = online();
    shell.broker.hold_puback = true;
    stamp(&mut shell, 1, "first");
    stamp(&mut shell, 2, "second");
    assert_eq!(published_texts(&shell), ["first", "second"]);

    // The link fails before the PUBACKs. The lines stay in the ring.
    shell.broker.hold_puback = false;
    shell.broker.held_pubacks.clear();
    shell.broker.drop_connection();
    shell.run();
    assert_ne!(shell.model.connection.link(), Link::Up);
    let kept: Vec<&str> = shell
        .model
        .logs
        .lines()
        .map(|line| line.message.as_str())
        .collect();
    assert_eq!(kept[..2], ["first", "second"]);

    // The next connection sends them again, first and in the same order,
    // before the lines of the failure.
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.model.connection.link(), Link::Up);
    let texts = published_texts(&shell);
    assert_eq!(texts[..4], ["first", "second", "first", "second"]);
    assert!(texts.len() > 4, "the failure wrote lines too: {texts:?}");
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn a_full_session_keeps_the_line_and_sends_it_after_an_ack() {
    const _: () = assert!(CAPACITY > LOG_IN_FLIGHT);
    let mut shell = online();
    shell.broker.hold_puback = true;

    for at in 0..=LOG_IN_FLIGHT as u64 {
        stamp(&mut shell, at, &format!("line {at}"));
    }

    // The log is at its limit of publishes that wait for a PUBACK. The last
    // line waits.
    assert_eq!(published(&shell).len(), LOG_IN_FLIGHT);
    assert_eq!(shell.model.logs.lines().len(), LOG_IN_FLIGHT + 1);

    // The PUBACKs come. The last line goes.
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();

    let texts = published_texts(&shell);
    assert_eq!(texts.len(), LOG_IN_FLIGHT + 1);
    assert_eq!(texts.last().unwrap(), &format!("line {LOG_IN_FLIGHT}"));
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn a_log_at_its_limit_does_not_block_asleep_or_a_service_call() {
    let mut shell = house();
    // No PUBACK comes, also for `online`.
    shell.broker.hold_puback = true;
    shell.send(start(PowerSource::Battery));
    for at in 0..CAPACITY as u64 {
        stamp(&mut shell, at, &format!("line {at}"));
    }
    assert_eq!(published(&shell).len(), LOG_IN_FLIGHT);

    // A tap on the All lights toggle still sends its service call.
    shell.send(tap_all_lights());
    assert_eq!(shell.broker.service_calls.len(), 1);

    // A sleep still publishes `asleep` before the DISCONNECT.
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    assert_eq!(shell.broker.retained(STATUS), Some("asleep"));
    let packets = &shell.broker.packets;
    let asleep = packets
        .iter()
        .position(|packet| {
            publish_of(packet)
                .is_some_and(|publish| publish.topic == STATUS && publish.payload == b"asleep")
        })
        .expect("the publish of asleep");
    let disconnect = packets
        .iter()
        .position(|packet| matches!(packet, Packet::Disconnect))
        .expect("a DISCONNECT");
    assert!(asleep < disconnect, "{asleep} {disconnect}");
}

#[test]
fn each_line_of_a_battery_sleep_goes_once_after_the_wake() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    // A tap turns the light on, so the first step of the sleep turns it off
    // while the connection is still up.
    shell.send(Event::Tapped(None));
    assert!(shell.light > 0);

    // The first step waits for the light while the connection is up. The
    // PUBACKs during the sleep are lost, as when they race the DISCONNECT.
    shell.broker.hold_puback = true;
    shell.hold_light = true;
    shell.send(Event::SleepNow);
    shell.hold_light = false;
    shell.answer_lights();
    assert!(shell.sleep.is_some());
    assert_eq!(published(&shell), Vec::<Value>::new());
    shell.broker.held_pubacks.clear();
    shell.broker.hold_puback = false;

    shell.advance(Duration::from_secs(600));
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);

    // Each line of the sleep went once, after the wake.
    let sleep_lines: Vec<String> = published_texts(&shell)
        .into_iter()
        .filter(|text| text.starts_with("sleep 1 "))
        .collect();
    assert_eq!(
        sleep_lines,
        [
            "sleep 1 turns the front light off",
            "sleep 1 closes the connection",
            "sleep 1 turns the radio off",
            "sleep 1 draws the last image and sleeps",
        ]
    );
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn a_wake_before_the_close_sends_the_lines_of_the_sleep() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.send(Event::Tapped(None));

    // The first step of the sleep waits for the light. Its line waits too.
    shell.hold_light = true;
    shell.send(Event::SleepNow);
    assert_eq!(published(&shell), Vec::<Value>::new());

    // A tap wakes the device before the close. The session is still up, so
    // the line goes now.
    shell.hold_light = false;
    shell.send(Event::Tapped(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(
        published_texts(&shell),
        ["sleep 1 turns the front light off"]
    );
}

#[test]
fn nothing_goes_while_the_device_closes_for_a_sleep_or_sleeps() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    // A line while the connection is up goes at once, so the core sends
    // lines when the sleep starts.
    stamp(&mut shell, 1, "a line before the sleep");
    assert_eq!(published_texts(&shell), ["a line before the sleep"]);

    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    // The steps of the sleep wrote lines during the close and after it.
    // They wait in the ring.
    assert_eq!(published_texts(&shell), ["a line before the sleep"]);
    let kept: Vec<&str> = shell
        .model
        .logs
        .lines()
        .map(|line| line.message.as_str())
        .collect();
    assert_eq!(
        kept,
        [
            "sleep 1 closes the connection",
            "sleep 1 turns the radio off",
            "sleep 1 draws the last image and sleeps"
        ]
    );
    let writes = shell.writes.len();

    // A line while the device sleeps waits too.
    stamp(&mut shell, 100, "a line while the device sleeps");

    assert_eq!(published_texts(&shell), ["a line before the sleep"]);
    assert_eq!(shell.writes.len(), writes);
    assert_eq!(shell.model.logs.lines().len(), 4);
}

#[test]
fn the_line_of_a_change_of_access_point_goes_on_the_next_connection() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.radio.move_strongest();

    // The wake joins the cached access point, and the scan finds a better
    // one. The core writes the line of the change when the close starts.
    shell.wake(Wake::Timer);
    assert_eq!(shell.broker.connections, 3);

    // The line went once, on the third connection: after the DISCONNECT
    // of the second connection.
    let change = "change to 02:00:00:00:00:02 on channel 11 (-45 dBm), because the signal \
                  of 02:00:00:00:00:01 on channel 6 is -75 dBm";
    let texts = published_texts(&shell);
    assert_eq!(
        texts.iter().filter(|text| *text == change).count(),
        1,
        "{texts:?}"
    );
    let packets = &shell.broker.packets;
    // The last DISCONNECT is the close for the change. The first one is the
    // close for the sleep.
    let disconnects = packets
        .iter()
        .filter(|packet| matches!(packet, Packet::Disconnect))
        .count();
    assert_eq!(disconnects, 2);
    let disconnect = packets
        .iter()
        .rposition(|packet| matches!(packet, Packet::Disconnect))
        .expect("a DISCONNECT");
    let line = packets
        .iter()
        .position(|packet| {
            publish_of(packet).is_some_and(|publish| {
                publish.topic == LOG_TOPIC && publish.payload.ends_with(b"-75 dBm\"}")
            })
        })
        .expect("the line of the change");
    assert!(disconnect < line, "{disconnect} {line}");
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn a_clock_answer_for_an_older_connection_does_nothing() {
    let mut shell = house();
    shell.hold_clock = true;
    stamp(&mut shell, 0, "a line before the start");
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.held_clocks(), 1);

    // The link fails before the answer. The next connection asks again.
    shell.broker.drop_connection();
    shell.run();
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(shell.held_clocks(), 2);

    // The answer for the first connection comes late. Nothing goes.
    let writes = shell.writes.len();
    shell.answer_clock(0);
    assert_eq!(published(&shell), Vec::<Value>::new());
    assert_eq!(shell.writes.len(), writes);

    // The answer for the current connection sends the lines.
    shell.answer_clock(0);
    let texts = published_texts(&shell);
    assert_eq!(texts[0], "a line before the start");
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn a_line_while_the_core_sends_goes_at_once_with_the_age_0() {
    let mut shell = online();
    stamp(&mut shell, 0, "the first line");
    assert_eq!(shell.clocks, 1);

    // Ten seconds later, a new line has the time of the clock of the shell.
    shell.advance(Duration::from_secs(10));
    let at = shell.now().as_secs();
    stamp(&mut shell, at, "a new line");

    assert_eq!(shell.clocks, 1, "no new clock request");
    let payloads = published(&shell);
    assert_eq!(payloads[1]["msg"], "a new line");
    assert_eq!(payloads[1]["age"], 0);
    assert_eq!(shell.model.logs.lines().len(), 0);
}

// The lines of the shell.

/// Returns a `ShellLogged` event with no lost lines.
fn shell_line(at: u64, level: Level, source: &str, message: &str) -> Event {
    Event::ShellLogged {
        at: Uptime(at),
        level,
        source: source.into(),
        message: message.into(),
        dropped: 0,
    }
}

#[test]
fn a_shell_line_goes_into_the_ring_with_its_source_and_level() {
    let app = Dashboard;
    let mut model = Model::default();

    // The link is down, so the line only goes into the ring. The event does
    // not render.
    app.update(
        shell_line(4_000, Level::Error, "panel", "power on failed: Timeout"),
        &mut model,
    )
    .expect_done();

    let lines: Vec<&Line> = model.logs.lines().collect();
    assert_eq!(
        lines,
        [&Line {
            at: Uptime(4_000),
            level: Level::Error,
            source: Source::Shell(ShellSource::new("panel")),
            message: Message::new("power on failed: Timeout"),
        }]
    );
    assert_eq!(model.logs.dropped(), 0);
}

#[test]
fn a_shell_line_keeps_a_cut_source_and_a_cut_message() {
    let app = Dashboard;
    let mut model = Model::default();
    let source = "s".repeat(MAX_SOURCE + 8);
    let message = "m".repeat(MAX_MESSAGE + 8);

    app.update(shell_line(0, Level::Warn, &source, &message), &mut model)
        .expect_done();

    let line = model.logs.lines().next().expect("a line");
    let name = line.source.name();
    assert_eq!(name.len(), MAX_SOURCE);
    assert!(name.ends_with(CUT_MARK), "{name}");
    assert_eq!(line.message.as_str().len(), MAX_MESSAGE);
    assert!(line.message.as_str().ends_with(CUT_MARK));
}

#[test]
fn a_shell_line_goes_out_with_its_source_and_level() {
    let mut shell = online();
    let frames = shell.frames.len();
    let at = u64::try_from(shell.now().as_millis()).unwrap();
    shell.send(shell_line(
        at,
        Level::Warn,
        "stream",
        "10 failed: TCP connect failed: ConnectionReset (by the host)",
    ));
    shell.send(shell_line(
        at,
        Level::Error,
        "panel",
        "refresh failed: Busy",
    ));

    let payloads: Vec<String> = shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == LOG_TOPIC)
        .map(|publish| String::from_utf8(publish.payload.clone()).unwrap())
        .collect();
    assert_eq!(
        payloads,
        [
            r#"{"level":"warn","source":"stream","age":0,"msg":"10 failed: TCP connect failed: ConnectionReset (by the host)"}"#,
            r#"{"level":"error","source":"panel","age":0,"msg":"refresh failed: Busy"}"#,
        ]
    );
    // The broker acknowledged both lines, so the ring is empty. The lines
    // and their PUBACKs did not render.
    assert_eq!(shell.model.logs.lines().len(), 0);
    assert_eq!(shell.frames.len(), frames);
}

#[test]
fn the_lines_that_the_shell_lost_add_to_the_drop_count() {
    let mut shell = house();
    for at in 0..CAPACITY as u64 {
        stamp(&mut shell, at, &format!("line {at}"));
    }
    assert_eq!(shell.model.logs.dropped(), 0);

    // The ring is full, so the line of the shell removes the oldest line.
    // The shell also lost 5 lines of its own.
    shell.send(Event::ShellLogged {
        at: Uptime(70_000),
        level: Level::Warn,
        source: "touch".into(),
        message: "read failed".into(),
        dropped: 5,
    });
    assert_eq!(shell.model.logs.dropped(), 6);
    assert_eq!(shell.model.logs.lines().len(), CAPACITY);

    // The drop message tells the sum. Then the lines go from the oldest
    // line that the ring kept, and the line of the shell is last.
    shell.send(start(PowerSource::Stand));
    let payloads = published(&shell);
    assert_eq!(payloads[0]["msg"], "6 older lines were dropped");
    assert_eq!(payloads[1]["msg"], "line 1");
    let last = payloads.last().expect("a line");
    assert_eq!(last["source"], "touch");
    assert_eq!(last["msg"], "read failed");
    assert_eq!(shell.model.logs.dropped(), 0);
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn a_shell_line_during_a_sleep_waits_for_the_wake_and_does_not_wake() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    let kept = shell.model.logs.lines().len();
    let writes = shell.writes.len();

    // The line goes into the ring. The core does not render, does not wake
    // and does not answer the `Sleep` request.
    let slept_at = u64::try_from(shell.now().as_millis()).unwrap();
    Dashboard
        .update(
            shell_line(
                slept_at,
                Level::Warn,
                "power",
                "cannot read the BQ27220 (Nack)",
            ),
            &mut shell.model,
        )
        .expect_done();
    assert!(shell.model.power.is_asleep());
    assert!(shell.sleep.is_some());
    assert_eq!(shell.model.logs.lines().len(), kept + 1);
    assert_eq!(shell.writes.len(), writes);
    assert_eq!(published(&shell), Vec::<Value>::new());

    // A minute later, a touch wakes the device. After the connect, the line
    // goes with its age.
    shell.advance(Duration::from_secs(60));
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    let line = published(&shell)
        .into_iter()
        .find(|payload| payload["source"] == "power" && payload["level"] == "warn")
        .expect("the line of the shell");
    assert_eq!(line["msg"], "cannot read the BQ27220 (Nack)");
    assert_eq!(line["age"], 60);
}
