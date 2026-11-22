//! Tests of the device feature: the discovery message, the state message
//! and the device publish queue.
//!
//! The payload tests check the exact JSON. The flow tests run the core in
//! the test shell, against the fake broker.

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use serde_json::{Value, json};

use super::{BIRTH_ONLINE, BIRTH_TOPIC, IN_FLIGHT, Snapshot, discovery};
use crate::{
    Event, PowerSource,
    capabilities::{
        log::{Area, Uptime},
        power::Wake,
    },
    features::{
        connection::{Link, RECONNECT_MIN},
        logs::{self, Kept, Line, Message, Source},
        power::IDLE_TIMEOUT,
    },
    mqtt::{Qos, codec::server::Packet},
    tests::{broker::publish_of, firmware, flows::house, settings, shell::Shell, start},
};

/// The discovery topic of the test settings.
const CONFIG: &str = "homeassistant/device/dashboard_hall/config";

/// The state topic of the test settings.
const STATE: &str = "dashboard/hall/state";

/// The status topic of the test settings.
const STATUS: &str = "dashboard/hall/status";

/// The log topic of the test settings.
const LOG: &str = "dashboard/hall/log";

/// The discovery payload for the test settings and firmware.
const DISCOVERY_JSON: &str = concat!(
    r#"{"dev":{"ids":"dashboard_hall","name":"Dashboard hall","mf":"LilyGo","#,
    r#""mdl":"T5 E-Paper S3 Pro","sw":"0.1.0+test","cns":[["mac","28:84:85:37:b8:a0"]]},"#,
    r#""o":{"name":"crux-ha-dashboard","sw":"0.1.0+test"},"#,
    r#""cmps":{"#,
    r#""status":{"p":"sensor","name":"Status","uniq_id":"dashboard_hall_status","#,
    r#""dev_cla":"enum","ops":["online","asleep","offline"],"stat_t":"dashboard/hall/status"},"#,
    r#""battery":{"p":"sensor","name":"Battery","uniq_id":"dashboard_hall_battery","#,
    r#""dev_cla":"battery","unit_of_meas":"%","stat_cla":"measurement","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.battery }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}"},"#,
    r#""stand":{"p":"binary_sensor","name":"On stand","uniq_id":"dashboard_hall_stand","#,
    r#""dev_cla":"plug","stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.stand }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}"}}}"#,
);

// The payloads.

#[test]
fn the_discovery_message_has_the_exact_json() {
    let payload = discovery::payload(&settings(), &firmware());
    assert_eq!(String::from_utf8(payload).unwrap(), DISCOVERY_JSON);
    assert_eq!(discovery::topic(&settings()), CONFIG);
}

#[test]
fn the_discovery_message_is_json_with_the_keys_and_under_4_kib() {
    let payload = discovery::payload(&settings(), &firmware());
    assert!(payload.len() < 4 * 1024, "{} bytes", payload.len());
    let config: Value = serde_json::from_slice(&payload).expect("JSON");

    let device = &config["dev"];
    assert_eq!(device["ids"], "dashboard_hall");
    assert_eq!(device["name"], "Dashboard hall");
    assert_eq!(device["mf"], "LilyGo");
    assert_eq!(device["mdl"], "T5 E-Paper S3 Pro");
    assert_eq!(device["sw"], "0.1.0+test");
    assert_eq!(device["cns"], json!([["mac", "28:84:85:37:b8:a0"]]));
    assert_eq!(
        config["o"],
        json!({"name": "crux-ha-dashboard", "sw": "0.1.0+test"})
    );

    let components = config["cmps"].as_object().expect("components");
    assert_eq!(components.len(), 3);
    for (key, platform, class) in [
        ("status", "sensor", "enum"),
        ("battery", "sensor", "battery"),
        ("stand", "binary_sensor", "plug"),
    ] {
        let component = &components[key];
        assert_eq!(component["p"], platform, "{key}");
        assert_eq!(component["dev_cla"], class, "{key}");
        assert_eq!(component["uniq_id"], format!("dashboard_hall_{key}"));
    }
    let status = &components["status"];
    assert_eq!(status["ops"], json!(["online", "asleep", "offline"]));
    assert_eq!(status["stat_t"], STATUS);
    assert!(
        status.get("avty_t").is_none(),
        "the status has no availability"
    );
    for key in ["battery", "stand"] {
        let component = &components[key];
        assert_eq!(component["stat_t"], STATE, "{key}");
        assert_eq!(component["val_tpl"], format!("{{{{ value_json.{key} }}}}"));
        assert_eq!(component["avty_t"], STATUS, "{key}");
    }
    assert_eq!(components["battery"]["unit_of_meas"], "%");
    assert_eq!(components["battery"]["stat_cla"], "measurement");
}

#[test]
fn the_state_message_has_the_exact_json() {
    let text = |battery, stand| String::from_utf8(Snapshot { battery, stand }.to_json()).unwrap();
    assert_eq!(text(Some(95), false), r#"{"battery":95,"stand":"OFF"}"#);
    assert_eq!(text(Some(100), true), r#"{"battery":100,"stand":"ON"}"#);
    // The shell did not read the battery.
    assert_eq!(text(None, true), r#"{"battery":null,"stand":"ON"}"#);
}

// The flows.

/// Returns the payload of each message on `topic` that the broker got, in
/// order, as text.
fn on(shell: &Shell, topic: &str) -> Vec<String> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == topic)
        .map(|publish| String::from_utf8(publish.payload.clone()).unwrap())
        .collect()
}

/// Returns the number of discovery messages that the broker got.
fn discoveries(shell: &Shell) -> usize {
    on(shell, CONFIG).len()
}

/// Ends the connection and connects again after the wait.
fn reconnect(shell: &mut Shell) {
    shell.broker.drop_connection();
    shell.run();
    assert_ne!(shell.model.connection.link(), Link::Up);
    shell.advance(RECONNECT_MIN);
    assert_eq!(shell.model.connection.link(), Link::Up);
}

/// Publishes the birth message of Home Assistant with `payload`, not
/// retained, as Home Assistant does when it starts.
fn birth(shell: &mut Shell, payload: &str) {
    shell.broker.publish(BIRTH_TOPIC, payload.as_bytes(), false);
    shell.run();
}

/// Keeps a line of the log with the time `at` in seconds, as if the shell
/// answered its `Log` request.
fn stamp(shell: &mut Shell, at_secs: u64, text: &str) {
    let line = Line {
        at: Uptime(at_secs * 1_000),
        level: Kept::Info,
        source: Source::Core(Area::Wifi),
        message: Message::new(text),
    };
    shell.send(Event::Logs(logs::Event::Stamped(line)));
}

/// Returns the topics of the publishes in a write.
fn topics(write: &[Packet]) -> Vec<String> {
    write
        .iter()
        .filter_map(publish_of)
        .map(|publish| publish.topic)
        .collect()
}

#[test]
fn the_discovery_and_the_state_are_retained_at_qos_1() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    for topic in [CONFIG, STATE] {
        let publishes: Vec<_> = shell
            .broker
            .published
            .iter()
            .filter(|publish| publish.topic == topic)
            .collect();
        assert_eq!(publishes.len(), 1, "{topic}");
        assert!(publishes[0].retain, "{topic}");
        assert_eq!(publishes[0].qos, Qos::AtLeastOnce, "{topic}");
    }
    assert_eq!(shell.broker.retained(CONFIG), Some(DISCOVERY_JSON));
    assert_eq!(
        shell.broker.retained(STATE),
        Some(r#"{"battery":null,"stand":"ON"}"#)
    );
}

#[test]
fn discovery_goes_once_for_each_boot_and_not_after_a_wake() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    assert_eq!(discoveries(&shell), 1);
    assert_eq!(on(&shell, STATE).len(), 1);

    // A sleep on battery, and a wake. The wake is not a start, so only the
    // state goes again.
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(discoveries(&shell), 1);
    assert_eq!(on(&shell, STATE).len(), 2);

    // A reconnect is not a start either.
    reconnect(&mut shell);
    assert_eq!(discoveries(&shell), 1);

    // A new `Start` is a boot, so the discovery goes again.
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(discoveries(&shell), 2);
}

#[test]
fn a_birth_message_that_is_not_retained_sends_the_discovery_again() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(discoveries(&shell), 1);

    // Home Assistant restarted.
    birth(&mut shell, BIRTH_ONLINE);
    assert_eq!(discoveries(&shell), 2);
    assert_eq!(shell.broker.retained(CONFIG), Some(DISCOVERY_JSON));

    // The last will of Home Assistant is not a birth.
    birth(&mut shell, "offline");
    assert_eq!(discoveries(&shell), 2);
}

#[test]
fn a_retained_birth_message_does_not_send_the_discovery_again() {
    let mut shell = house();
    // The broker keeps a birth message, so it sends it with the retain
    // flag at each subscribe.
    shell.broker.retain(BIRTH_TOPIC, BIRTH_ONLINE.as_bytes());
    shell.send(start(PowerSource::Stand));
    assert!(
        shell
            .broker
            .delivered
            .iter()
            .any(|topic| topic == BIRTH_TOPIC)
    );
    assert_eq!(discoveries(&shell), 1);

    reconnect(&mut shell);
    assert_eq!(discoveries(&shell), 1);
}

#[test]
fn a_birth_message_while_the_discovery_is_in_flight_changes_nothing() {
    let mut shell = house();
    shell.broker.hold_puback = true;
    shell.send(start(PowerSource::Stand));
    assert_eq!(discoveries(&shell), 1);

    // The message in flight is retained, so Home Assistant gets it when it
    // subscribes. The birth message does not send it again.
    birth(&mut shell, BIRTH_ONLINE);
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert_eq!(discoveries(&shell), 1);
}

#[test]
fn a_session_that_ends_with_the_discovery_in_flight_sends_it_again() {
    let mut shell = house();
    shell.broker.hold_puback = true;
    shell.send(start(PowerSource::Stand));
    assert_eq!(discoveries(&shell), 1);

    // The link fails before the PUBACK.
    shell.broker.hold_puback = false;
    shell.broker.held_pubacks.clear();
    reconnect(&mut shell);
    assert_eq!(discoveries(&shell), 2);

    // The broker acknowledged it this time, so the next connection sends
    // no discovery.
    reconnect(&mut shell);
    assert_eq!(discoveries(&shell), 2);
}

#[test]
fn the_state_goes_on_each_connection_and_after_each_change() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(on(&shell, STATE), [r#"{"battery":null,"stand":"ON"}"#]);

    shell.send(Event::BatteryChanged(Some(95)));
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    // The same level is no change, so it sends nothing.
    shell.send(Event::BatteryChanged(Some(95)));
    reconnect(&mut shell);
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));

    assert_eq!(
        on(&shell, STATE),
        [
            r#"{"battery":null,"stand":"ON"}"#,
            r#"{"battery":95,"stand":"ON"}"#,
            r#"{"battery":95,"stand":"OFF"}"#,
            // The new connection.
            r#"{"battery":95,"stand":"OFF"}"#,
            r#"{"battery":95,"stand":"ON"}"#,
        ]
    );
}

#[test]
fn a_change_while_the_state_is_in_flight_goes_after_the_puback() {
    let mut shell = house();
    shell.broker.hold_puback = true;
    shell.send(start(PowerSource::Stand));
    assert_eq!(on(&shell, STATE).len(), 1);

    // At most one state message waits for its PUBACK.
    shell.send(Event::BatteryChanged(Some(60)));
    shell.send(Event::BatteryChanged(Some(59)));
    assert_eq!(on(&shell, STATE).len(), 1);

    // The PUBACK comes. The newest snapshot goes once.
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert_eq!(
        on(&shell, STATE),
        [
            r#"{"battery":null,"stand":"ON"}"#,
            r#"{"battery":59,"stand":"ON"}"#,
        ]
    );
}

#[test]
fn nothing_goes_while_the_device_sleeps() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    let states = on(&shell, STATE).len();

    // A tap turns the light on, so the first step of the sleep waits for the
    // light while the connection is still up.
    shell.send(Event::Tapped(None));
    shell.hold_light = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    assert_eq!(shell.model.connection.link(), Link::Up);
    let writes = shell.writes.len();

    // A change of the battery and a birth message in that step send
    // nothing.
    shell.send(Event::BatteryChanged(Some(40)));
    birth(&mut shell, BIRTH_ONLINE);
    assert_eq!(shell.writes.len(), writes);

    // The sleep continues. A change while the device sleeps sends nothing.
    shell.hold_light = false;
    shell.answer_lights();
    assert!(shell.sleep.is_some());
    shell.send(Event::BatteryChanged(Some(39)));
    assert_eq!(on(&shell, STATE).len(), states);
    assert_eq!(discoveries(&shell), 1);

    // The wake sends the state, and the discovery that the birth made due.
    shell.advance(Duration::from_secs(60));
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(
        on(&shell, STATE)[states..],
        [r#"{"battery":39,"stand":"OFF"}"#]
    );
    assert_eq!(discoveries(&shell), 2);
}

#[test]
fn the_queue_sends_the_discovery_then_the_state_then_the_log() {
    let mut shell = house();
    stamp(&mut shell, 0, "a line before the start");
    shell.send(start(PowerSource::Stand));

    // The CONNECT, the CONNACK write, the discovery and the state, then the
    // line after the clock answer.
    assert_eq!(topics(&shell.writes[1]), [STATUS]);
    assert_eq!(topics(&shell.writes[2]), [CONFIG, STATE]);
    assert_eq!(topics(&shell.writes[3]), [LOG]);
}

#[test]
fn all_device_publishes_share_one_limit_that_leaves_the_reserved_places() {
    let mut shell = house();
    shell.broker.hold_puback = true;
    for at in 0..logs::CAPACITY as u64 {
        stamp(&mut shell, at, &format!("line {at}"));
    }
    shell.send(start(PowerSource::Stand));

    // The discovery, the state and the lines wait for their PUBACKs, at
    // most `IN_FLIGHT` of them together. The lines go in order.
    let device = discoveries(&shell) + on(&shell, STATE).len() + on(&shell, LOG).len();
    assert_eq!(device, IN_FLIGHT);
    // The session takes 64. The status `online` also waits, and 7 places
    // stay free for `asleep` and the service calls.
    assert_eq!(IN_FLIGHT, 56);
    assert_eq!(shell.broker.held_pubacks.len(), 57);
    let texts: Vec<String> = on(&shell, LOG)
        .iter()
        .map(|payload| {
            let line: Value = serde_json::from_str(payload).unwrap();
            String::from(line["msg"].as_str().unwrap())
        })
        .collect();
    let expected: Vec<String> = (0..IN_FLIGHT as u64 - 2)
        .map(|at| format!("line {at}"))
        .collect();
    assert_eq!(texts, expected);

    // A change of the battery waits too, because the state message is in
    // flight.
    shell.send(Event::BatteryChanged(Some(70)));
    assert_eq!(on(&shell, STATE).len(), 1);

    // The PUBACKs come, and the rest goes.
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert_eq!(on(&shell, LOG).len(), logs::CAPACITY);
    assert_eq!(
        on(&shell, STATE).last().map(String::as_str),
        Some(r#"{"battery":70,"stand":"ON"}"#)
    );
    assert_eq!(shell.model.logs.lines().len(), 0);
}

#[test]
fn the_connack_write_waits_for_the_sent_of_the_connect_and_has_no_device_publish() {
    let mut shell = house();
    // The shell sends the CONNECT, but its answer `Sent` waits. The broker
    // answers with the CONNACK first.
    shell.broker.hold_writes = true;
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.writes.len(), 1);
    shell.broker.answer_before_sent();
    shell.run();
    assert_eq!(shell.model.connection.link(), Link::Up);
    // The CONNACK write waits for the `Sent`. A change of the battery in
    // that time does not add the state message to it.
    shell.send(Event::BatteryChanged(Some(50)));
    assert_eq!(shell.writes.len(), 1);

    shell.broker.release_writes();
    shell.run();

    let connack = &shell.writes[1];
    assert_eq!(topics(connack), [STATUS]);
    assert!(
        connack
            .iter()
            .all(|packet| matches!(packet, Packet::Publish { .. } | Packet::Subscribe { .. })),
        "{connack:?}"
    );
    assert_eq!(topics(&shell.writes[2]), [CONFIG, STATE]);
}

#[test]
fn a_dry_run_publishes_no_discovery_and_no_state() {
    let mut shell = house();
    let mut start = start(PowerSource::Stand);
    if let Event::Start(start) = &mut start {
        start.settings.read_only = true;
    }
    stamp(&mut shell, 0, "a line before the start");
    shell.send(start);
    shell.send(Event::BatteryChanged(Some(80)));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(discoveries(&shell), 0);
    assert_eq!(on(&shell, STATE), Vec::<String>::new());
    // The log still goes.
    assert_eq!(on(&shell, LOG).len(), 1);

    // The end of the dry run sends the discovery and the state.
    shell.send(Event::SetReadOnly(false));
    assert_eq!(discoveries(&shell), 1);
    assert_eq!(on(&shell, STATE), [r#"{"battery":80,"stand":"ON"}"#]);
}
