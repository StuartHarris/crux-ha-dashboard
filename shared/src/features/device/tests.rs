//! Tests of the device feature: the discovery message, the state message
//! and the device publish queue.
//!
//! The payload tests check the exact JSON. The flow tests run the core in
//! the test shell, against the fake broker.

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use serde_json::{Value, json};

use super::{BIRTH_ONLINE, BIRTH_TOPIC, IN_FLIGHT, STATE_MIN_INTERVAL, Snapshot, Wifi, discovery};
use crate::{
    Charge, Event, PowerSource, Reading,
    capabilities::{
        log::{Area, Uptime},
        power::Wake,
    },
    features::{
        connection::{Link, RECONNECT_MIN},
        logs::{self, Kept, Line, Message, Source},
        power::{IDLE_TIMEOUT, WakeReason},
    },
    mqtt::{MAX_OUTGOING, Qos, codec::server::Packet},
    tests::{
        POWER_ON, broker::publish_of, firmware, flows::house, settings, shell::Shell, start, state,
    },
};

/// The discovery topic of the test settings.
const CONFIG: &str = "homeassistant/device/dashboard_hall/config";

/// The state topic of the test settings.
const STATE: &str = "dashboard/hall/state";

/// The status topic of the test settings.
const STATUS: &str = "dashboard/hall/status";

/// The log topic of the test settings.
const LOG: &str = "dashboard/hall/log";

/// A reading on battery.
const ON_BATTERY: Reading = Reading {
    charge: Some(Charge::NotCharging),
    voltage_mv: Some(3870),
    current_ma: Some(-40),
    tenth_celsius: Some(245),
};

/// A reading on the stand, in a fast charge.
const CHARGING: Reading = Reading {
    charge: Some(Charge::FastCharging),
    voltage_mv: Some(4150),
    current_ma: Some(450),
    tenth_celsius: Some(260),
};

/// The discovery payload for the test settings and firmware.
const DISCOVERY_JSON: &str = concat!(
    r#"{"dev":{"ids":"dashboard_hall","name":"Dashboard hall","mf":"LilyGo","#,
    r#""mdl":"T5 E-Paper S3 Pro","sw":"0.1.0+test","cns":[["mac","28:84:85:37:b8:a0"]]},"#,
    r#""o":{"name":"crux-ha-dashboard","sw":"0.1.0+test"},"#,
    r#""cmps":{"#,
    r#""status":{"p":"sensor","name":"Status","uniq_id":"dashboard_hall_status","#,
    r#""dev_cla":"enum","ic":"mdi:tablet-dashboard","ops":["online","asleep","offline"],"#,
    r#""stat_t":"dashboard/hall/status"},"#,
    r#""battery":{"p":"sensor","name":"Battery","uniq_id":"dashboard_hall_battery","#,
    r#""dev_cla":"battery","unit_of_meas":"%","stat_cla":"measurement","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.battery }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}"},"#,
    r#""stand":{"p":"binary_sensor","name":"On stand","uniq_id":"dashboard_hall_stand","#,
    r#""dev_cla":"plug","stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.stand }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}"},"#,
    r#""charging":{"p":"binary_sensor","name":"Charging","uniq_id":"dashboard_hall_charging","#,
    r#""dev_cla":"battery_charging","stat_t":"dashboard/hall/state","#,
    r#""val_tpl":"{{ value_json.charging }}","avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""charge":{"p":"sensor","name":"Charge state","uniq_id":"dashboard_hall_charge","#,
    r#""dev_cla":"enum","ic":"mdi:battery-charging-outline","#,
    r#""ops":["Not charging","Pre-charging","Fast charging","Done"],"#,
    r#""stat_t":"dashboard/hall/state","val_tpl":""#,
    r#"{{ {'not_charging': 'Not charging', 'pre_charging': 'Pre-charging', "#,
    r#"'fast_charging': 'Fast charging', 'done': 'Done'}[value_json.charge] | default(None) }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""voltage":{"p":"sensor","name":"Battery voltage","uniq_id":"dashboard_hall_voltage","#,
    r#""dev_cla":"voltage","ic":"mdi:flash-triangle-outline","unit_of_meas":"mV","stat_cla":"measurement","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.voltage }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""current":{"p":"sensor","name":"Battery current","uniq_id":"dashboard_hall_current","#,
    r#""dev_cla":"current","ic":"mdi:current-dc","unit_of_meas":"mA","stat_cla":"measurement","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.current }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""temperature":{"p":"sensor","name":"Gauge temperature","#,
    r#""uniq_id":"dashboard_hall_temperature","#,
    r#""dev_cla":"temperature","unit_of_meas":"°C","stat_cla":"measurement","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.temperature }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""rssi":{"p":"sensor","name":"Wi-Fi signal","uniq_id":"dashboard_hall_rssi","#,
    r#""dev_cla":"signal_strength","unit_of_meas":"dBm","stat_cla":"measurement","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.rssi }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""bssid":{"p":"sensor","name":"Access point","uniq_id":"dashboard_hall_bssid","#,
    r#""ic":"mdi:access-point","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.bssid }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""ip":{"p":"sensor","name":"IP address","uniq_id":"dashboard_hall_ip","#,
    r#""ic":"mdi:ip-network","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.ip }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""reconnects":{"p":"sensor","name":"Reconnects","uniq_id":"dashboard_hall_reconnects","#,
    r#""ic":"mdi:lan-connect","stat_cla":"total_increasing","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.reconnects }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""restarts":{"p":"sensor","name":"Restarts","uniq_id":"dashboard_hall_restarts","#,
    r#""ic":"mdi:restart","stat_cla":"total_increasing","#,
    r#""stat_t":"dashboard/hall/state","val_tpl":"{{ value_json.restarts }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""reset":{"p":"sensor","name":"Last reset","uniq_id":"dashboard_hall_reset","#,
    r#""dev_cla":"enum","ic":"mdi:power-cycle","#,
    r#""ops":["Power on","Software","Panic","Watchdog","Brownout","Other"],"#,
    r#""stat_t":"dashboard/hall/state","val_tpl":""#,
    r#"{{ {'power_on': 'Power on', 'software': 'Software', 'panic': 'Panic', "#,
    r#"'watchdog': 'Watchdog', 'brownout': 'Brownout', 'other': 'Other'}[value_json.reset] "#,
    r#"| default(None) }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"},"#,
    r#""wake":{"p":"sensor","name":"Wake reason","uniq_id":"dashboard_hall_wake","#,
    r#""dev_cla":"enum","ic":"mdi:sleep-off","ops":["Boot","Touch","Stand"],"#,
    r#""stat_t":"dashboard/hall/state","val_tpl":""#,
    r#"{{ {'boot': 'Boot', 'touch': 'Touch', 'stand': 'Stand'}[value_json.wake] "#,
    r#"| default(None) }}","#,
    r#""avty_t":"dashboard/hall/status","#,
    r#""avty_tpl":"{{ 'offline' if value == 'offline' else 'online' }}","#,
    r#""ent_cat":"diagnostic"}}}"#,
);

// The payloads.

#[test]
fn the_discovery_message_has_the_exact_json() {
    let payload = discovery::payload(&settings(), &firmware());
    assert_eq!(String::from_utf8(payload).unwrap(), DISCOVERY_JSON);
    assert_eq!(discovery::topic(&settings()), CONFIG);
}

#[test]
fn the_discovery_message_is_json_with_the_keys_and_under_6_kib() {
    let payload = discovery::payload(&settings(), &firmware());
    assert!(payload.len() < 6 * 1024, "{} bytes", payload.len());
    // The discovery and the state go in one write, and each write has at
    // most `MAX_OUTGOING` bytes. The two messages use about a third.
    let state = snapshot(Some(100), true, CHARGING).to_json();
    assert!(
        (payload.len() + state.len()) * 2 < MAX_OUTGOING,
        "{} bytes",
        payload.len() + state.len()
    );
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
    assert_eq!(components.len(), 15);
    for (key, platform, class) in [
        ("status", "sensor", Some("enum")),
        ("battery", "sensor", Some("battery")),
        ("stand", "binary_sensor", Some("plug")),
        ("charging", "binary_sensor", Some("battery_charging")),
        ("charge", "sensor", Some("enum")),
        ("voltage", "sensor", Some("voltage")),
        ("current", "sensor", Some("current")),
        ("temperature", "sensor", Some("temperature")),
        ("rssi", "sensor", Some("signal_strength")),
        ("bssid", "sensor", None),
        ("ip", "sensor", None),
        ("reconnects", "sensor", None),
        ("restarts", "sensor", None),
        ("reset", "sensor", Some("enum")),
        ("wake", "sensor", Some("enum")),
    ] {
        let component = &components[key];
        assert_eq!(component["p"], platform, "{key}");
        assert_eq!(
            component.get("dev_cla").and_then(Value::as_str),
            class,
            "{key}"
        );
        assert_eq!(component["uniq_id"], format!("dashboard_hall_{key}"));
    }
    let status = &components["status"];
    assert_eq!(status["ops"], json!(["online", "asleep", "offline"]));
    assert_eq!(status["stat_t"], STATUS);
    assert!(
        status.get("avty_t").is_none(),
        "the status has no availability"
    );
    for key in [
        "battery",
        "stand",
        "charging",
        "charge",
        "voltage",
        "current",
        "temperature",
        "rssi",
        "bssid",
        "ip",
        "reconnects",
        "restarts",
        "reset",
        "wake",
    ] {
        let component = &components[key];
        assert_eq!(component["stat_t"], STATE, "{key}");
        if !["charge", "reset", "wake"].contains(&key) {
            assert_eq!(component["val_tpl"], format!("{{{{ value_json.{key} }}}}"));
        }
        assert_eq!(component["avty_t"], STATUS, "{key}");
        assert_eq!(
            component["avty_tpl"],
            "{{ 'offline' if value == 'offline' else 'online' }}"
        );
    }
}

#[test]
fn the_components_have_their_units_state_classes_and_categories() {
    let payload = discovery::payload(&settings(), &firmware());
    let config: Value = serde_json::from_slice(&payload).expect("JSON");
    let components = config["cmps"].as_object().expect("components");
    for (key, unit) in [
        ("battery", "%"),
        ("voltage", "mV"),
        ("current", "mA"),
        ("temperature", "°C"),
        ("rssi", "dBm"),
    ] {
        assert_eq!(components[key]["unit_of_meas"], unit, "{key}");
        assert_eq!(components[key]["stat_cla"], "measurement", "{key}");
    }
    // The counts only go up, except at a start or a power-on.
    for key in ["reconnects", "restarts"] {
        assert_eq!(components[key]["stat_cla"], "total_increasing", "{key}");
        assert!(components[key].get("unit_of_meas").is_none(), "{key}");
    }
    for key in ["bssid", "ip", "reset", "wake"] {
        assert!(components[key].get("stat_cla").is_none(), "{key}");
    }
    // The power, Wi-Fi and boot diagnostics are in the diagnostic
    // category. The status, the battery and the stand are not.
    for key in [
        "charging",
        "charge",
        "voltage",
        "current",
        "temperature",
        "rssi",
        "bssid",
        "ip",
        "reconnects",
        "restarts",
        "reset",
        "wake",
    ] {
        assert_eq!(components[key]["ent_cat"], "diagnostic", "{key}");
    }
    for key in ["status", "battery", "stand"] {
        assert!(components[key].get("ent_cat").is_none(), "{key}");
    }
}

#[test]
fn the_enum_components_have_labels_and_each_component_has_its_icon() {
    let payload = discovery::payload(&settings(), &firmware());
    let config: Value = serde_json::from_slice(&payload).expect("JSON");
    let components = config["cmps"].as_object().expect("components");
    // The charge shows labels, and its template maps the snake_case wire
    // values to them. A null or unknown value gives `None`.
    assert_eq!(
        components["charge"]["ops"],
        json!(["Not charging", "Pre-charging", "Fast charging", "Done"])
    );
    assert_eq!(
        components["charge"]["val_tpl"],
        "{{ {'not_charging': 'Not charging', 'pre_charging': 'Pre-charging', \
         'fast_charging': 'Fast charging', 'done': 'Done'}[value_json.charge] \
         | default(None) }}"
    );
    assert_eq!(
        components["reset"]["ops"],
        json!([
            "Power on", "Software", "Panic", "Watchdog", "Brownout", "Other"
        ])
    );
    assert_eq!(
        components["reset"]["val_tpl"],
        "{{ {'power_on': 'Power on', 'software': 'Software', 'panic': 'Panic', \
         'watchdog': 'Watchdog', 'brownout': 'Brownout', 'other': 'Other'}[value_json.reset] \
         | default(None) }}"
    );
    assert_eq!(components["wake"]["ops"], json!(["Boot", "Touch", "Stand"]));
    assert_eq!(
        components["wake"]["val_tpl"],
        "{{ {'boot': 'Boot', 'touch': 'Touch', 'stand': 'Stand'}[value_json.wake] \
         | default(None) }}"
    );
    // Only these components set an icon. The others keep the icon of their
    // device class.
    for (key, icon) in [
        ("status", Some("mdi:tablet-dashboard")),
        ("battery", None),
        ("stand", None),
        ("charging", None),
        ("charge", Some("mdi:battery-charging-outline")),
        ("voltage", Some("mdi:flash-triangle-outline")),
        ("current", Some("mdi:current-dc")),
        ("temperature", None),
        ("rssi", None),
        ("bssid", Some("mdi:access-point")),
        ("ip", Some("mdi:ip-network")),
        ("reconnects", Some("mdi:lan-connect")),
        ("restarts", Some("mdi:restart")),
        ("reset", Some("mdi:power-cycle")),
        ("wake", Some("mdi:sleep-off")),
    ] {
        assert_eq!(
            components[key].get("ic").and_then(Value::as_str),
            icon,
            "{key}"
        );
    }
}

/// The Wi-Fi facts of a link in the tests.
const LINK: Wifi = Wifi {
    bssid: [0x24, 0x5a, 0x4c, 0x01, 0x02, 0x03],
    ip: [192, 168, 1, 50],
    rssi: Some(-62),
};

/// The JSON of [`LINK`], no reconnect, a power-on and no wake. It is a
/// macro, so `concat!` can use it.
macro_rules! link_json {
    () => {
        concat!(
            r#""rssi":-62,"bssid":"24:5a:4c:01:02:03","ip":"192.168.1.50","#,
            r#""reconnects":0,"restarts":0,"reset":"power_on","wake":"boot""#
        )
    };
}

/// Returns a snapshot with `battery`, `stand` and `reading`, the link
/// [`LINK`], no reconnect, a power-on and no wake.
const fn snapshot(battery: Option<u8>, stand: bool, reading: Reading) -> Snapshot {
    Snapshot {
        battery,
        stand,
        reading,
        wifi: Some(LINK),
        reconnects: 0,
        boot: POWER_ON,
        wake: WakeReason::Boot,
    }
}

#[test]
fn the_state_message_has_the_exact_json() {
    let text = |battery, stand, reading| {
        String::from_utf8(snapshot(battery, stand, reading).to_json()).unwrap()
    };
    // On battery, with a full reading.
    assert_eq!(
        text(Some(95), false, ON_BATTERY),
        concat!(
            r#"{"battery":95,"stand":"OFF","charging":"OFF","charge":"not_charging","#,
            r#""voltage":3870,"current":-40,"temperature":24.5,"#,
            link_json!(),
            "}"
        )
    );
    // On the stand, in a fast charge. A whole degree has one decimal.
    assert_eq!(
        text(Some(100), true, CHARGING),
        concat!(
            r#"{"battery":100,"stand":"ON","charging":"ON","charge":"fast_charging","#,
            r#""voltage":4150,"current":450,"temperature":26.0,"#,
            link_json!(),
            "}"
        )
    );
    // A pre-charge is a charge. A done charge is not.
    let phase = |charge| {
        let json: Value = serde_json::from_str(&text(
            None,
            true,
            Reading {
                charge: Some(charge),
                ..Reading::default()
            },
        ))
        .unwrap();
        (json["charging"].clone(), json["charge"].clone())
    };
    assert_eq!(
        phase(Charge::PreCharging),
        (json!("ON"), json!("pre_charging"))
    );
    assert_eq!(phase(Charge::Done), (json!("OFF"), json!("done")));
    // A temperature below 0 °C.
    let cold = Reading {
        tenth_celsius: Some(-15),
        ..Reading::default()
    };
    assert!(text(None, false, cold).contains(r#""temperature":-1.5,"#));
    // The shell did not read the battery or the diagnostics.
    assert_eq!(
        text(None, true, Reading::default()),
        concat!(
            r#"{"battery":null,"stand":"ON","charging":null,"charge":null,"#,
            r#""voltage":null,"current":null,"temperature":null,"#,
            link_json!(),
            "}"
        )
    );
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
        Some(state("null", "ON").as_str())
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
    assert_eq!(on(&shell, STATE), [state("null", "ON")]);

    shell.send(Event::BatteryChanged(Some(95)));
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    // The same level is no change, so it sends nothing.
    shell.send(Event::BatteryChanged(Some(95)));
    reconnect(&mut shell);
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));

    // The new connection is after a failed session, so it counts one
    // reconnect.
    let after_the_reconnect =
        |battery, stand| state(battery, stand).replace(r#""reconnects":0"#, r#""reconnects":1"#);
    assert_eq!(
        on(&shell, STATE),
        [
            state("null", "ON"),
            state("95", "ON"),
            state("95", "OFF"),
            // The new connection.
            after_the_reconnect("95", "OFF"),
            after_the_reconnect("95", "ON"),
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
    assert_eq!(on(&shell, STATE), [state("null", "ON"), state("59", "ON"),]);
}

#[test]
fn nothing_goes_while_the_device_sleeps() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    let before = on(&shell, STATE).len();

    // A tap turns the light on, so the first step of the sleep waits for the
    // light while the connection is still up.
    shell.send(Event::Tapped(None));
    shell.hold_light = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    assert_eq!(shell.model.connection.link(), Link::Up);
    let writes = shell.writes.len();

    // A change of the battery, a power reading and a birth message in that
    // step send nothing.
    shell.send(Event::BatteryChanged(Some(40)));
    shell.send(Event::PowerRead(ON_BATTERY));
    birth(&mut shell, BIRTH_ONLINE);
    assert_eq!(shell.writes.len(), writes);

    // The sleep continues. A change while the device sleeps sends nothing.
    shell.hold_light = false;
    shell.answer_lights();
    assert!(shell.sleep.is_some());
    shell.send(Event::BatteryChanged(Some(39)));
    assert_eq!(on(&shell, STATE).len(), before);
    assert_eq!(discoveries(&shell), 1);

    // The wake sends the state, and the discovery that the birth made due.
    shell.advance(Duration::from_secs(60));
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    let sent = &states(&shell)[before..];
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["battery"], 39);
    assert_eq!(sent[0]["stand"], "OFF");
    assert_eq!(sent[0]["current"], -40);
    assert_eq!(discoveries(&shell), 2);
}

#[test]
fn the_queue_sends_the_discovery_then_the_state_then_the_log() {
    let mut shell = house();
    stamp(&mut shell, 0, "a line before the start");
    shell.send(start(PowerSource::Stand));

    // The CONNECT, the CONNACK write, the discovery and the state, then the
    // line and the start line after the clock answer.
    assert_eq!(topics(&shell.writes[1]), [STATUS]);
    assert_eq!(topics(&shell.writes[2]), [CONFIG, STATE]);
    assert_eq!(topics(&shell.writes[3]), [LOG, LOG]);
}

#[test]
fn all_device_publishes_share_one_limit_that_leaves_the_reserved_places() {
    let mut shell = house();
    shell.broker.hold_puback = true;
    // The start line fills the last place of the ring.
    for at in 0..logs::CAPACITY as u64 - 1 {
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
        Some(state("70", "ON").as_str())
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
    // The log still goes: the line and the start line.
    assert_eq!(on(&shell, LOG).len(), 2);

    // The end of the dry run sends the discovery and the state.
    shell.send(Event::SetReadOnly(false));
    assert_eq!(discoveries(&shell), 1);
    assert_eq!(on(&shell, STATE), [state("80", "ON")]);
}

// The throttle of the state message.

/// Returns each state message that the broker got, as JSON.
fn states(shell: &Shell) -> Vec<Value> {
    on(shell, STATE)
        .iter()
        .map(|payload| serde_json::from_str(payload).unwrap())
        .collect()
}

/// Returns the value of `key` in each state message that the broker got.
fn state_values(shell: &Shell, key: &str) -> Vec<Value> {
    states(shell)
        .iter()
        .map(|state| state[key].clone())
        .collect()
}

/// Returns `CHARGING` with the voltage `millivolts`.
const fn charging_at(millivolts: u16) -> Reading {
    Reading {
        voltage_mv: Some(millivolts),
        ..CHARGING
    }
}

/// Returns `true` if the timer of the throttle window waits.
fn window_waits(shell: &Shell) -> bool {
    shell.waiting().contains(&STATE_MIN_INTERVAL)
}

#[test]
fn a_change_of_the_stand_or_of_the_charging_goes_at_once() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    // The first reading starts a charge, so it goes at once.
    shell.send(Event::PowerRead(CHARGING));
    // A change of only the voltage goes, and the window is throttled. The
    // next one waits.
    shell.send(Event::PowerRead(charging_at(4160)));
    shell.send(Event::PowerRead(charging_at(4170)));
    assert!(window_waits(&shell));
    assert_eq!(on(&shell, STATE).len(), 3);

    // The end of the charge goes at once, with the voltage that waits.
    shell.send(Event::PowerRead(Reading {
        charge: Some(Charge::Done),
        ..charging_at(4170)
    }));
    // A move off the stand goes at once.
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));

    assert_eq!(
        state_values(&shell, "charging"),
        [
            Value::Null,
            json!("ON"),
            json!("ON"),
            json!("OFF"),
            json!("OFF")
        ]
    );
    assert_eq!(
        state_values(&shell, "voltage"),
        [
            Value::Null,
            json!(4150),
            json!(4160),
            json!(4170),
            json!(4170)
        ]
    );
    assert_eq!(
        state_values(&shell, "stand"),
        [
            json!("ON"),
            json!("ON"),
            json!("ON"),
            json!("ON"),
            json!("OFF")
        ]
    );
}

#[test]
fn other_changes_go_at_most_once_in_the_window_and_the_last_one_goes_at_its_end() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::PowerRead(CHARGING));
    assert_eq!(on(&shell, STATE).len(), 2);
    assert!(!window_waits(&shell), "an urgent change starts no timer");

    // The window is open, so the change goes at once, and the timer starts.
    shell.send(Event::PowerRead(charging_at(4160)));
    assert_eq!(on(&shell, STATE).len(), 3);
    assert!(window_waits(&shell));

    // Each change in the window waits: the current, the temperature, the
    // voltage, and the phase of the charge in a charge.
    shell.advance(Duration::from_secs(20));
    shell.send(Event::PowerRead(Reading {
        current_ma: Some(460),
        ..charging_at(4160)
    }));
    shell.advance(Duration::from_secs(20));
    let last = Reading {
        charge: Some(Charge::PreCharging),
        voltage_mv: Some(4170),
        current_ma: Some(460),
        tenth_celsius: Some(265),
    };
    shell.send(Event::PowerRead(last));
    shell.advance(Duration::from_secs(19));
    assert_eq!(on(&shell, STATE).len(), 3);

    // The end of the window sends the last values once, and starts a new
    // timer.
    shell.advance(Duration::from_secs(1));
    assert_eq!(on(&shell, STATE).len(), 4);
    let state = &states(&shell)[3];
    assert_eq!(state["charge"], "pre_charging");
    assert_eq!(state["voltage"], 4170);
    assert_eq!(state["current"], 460);
    assert_eq!(state["temperature"], 26.5);
    assert!(window_waits(&shell));

    shell.send(Event::PowerRead(Reading {
        current_ma: Some(470),
        ..last
    }));
    assert_eq!(on(&shell, STATE).len(), 4);
    shell.advance(STATE_MIN_INTERVAL);
    assert_eq!(on(&shell, STATE).len(), 5);

    // A window with no change sends nothing at its end, and starts no new
    // timer.
    shell.advance(STATE_MIN_INTERVAL);
    assert_eq!(on(&shell, STATE).len(), 5);
    assert!(!window_waits(&shell));
}

#[test]
fn a_battery_that_flaps_by_1_goes_at_most_once_in_the_window() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::BatteryChanged(Some(50)));
    // A move of 1% is not urgent. The window is open, so it goes, and the
    // flaps after it wait.
    for level in [51, 50, 51, 50] {
        shell.send(Event::BatteryChanged(Some(level)));
    }
    assert_eq!(
        state_values(&shell, "battery"),
        [Value::Null, json!(50), json!(51)]
    );

    // The end of the window sends the level of that time.
    shell.advance(STATE_MIN_INTERVAL);
    // A move of more than 1% goes at once, also in the new window.
    shell.send(Event::BatteryChanged(Some(52)));
    assert!(window_waits(&shell));
    assert_eq!(
        state_values(&shell, "battery"),
        [Value::Null, json!(50), json!(51), json!(50), json!(52)]
    );
}

#[test]
fn the_window_ends_while_the_state_waits_for_its_puback() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::PowerRead(CHARGING));
    shell.broker.hold_puback = true;
    shell.send(Event::PowerRead(charging_at(4160)));
    shell.send(Event::PowerRead(charging_at(4170)));
    assert_eq!(on(&shell, STATE).len(), 3);

    // The window ends, but the state message waits for its PUBACK.
    shell.advance(STATE_MIN_INTERVAL);
    assert_eq!(on(&shell, STATE).len(), 3);
    assert!(!window_waits(&shell));

    // The PUBACK comes, and the window is open, so the last voltage goes.
    shell.broker.hold_puback = false;
    shell.broker.release_pubacks();
    shell.broker.flush();
    shell.run();
    assert_eq!(
        state_values(&shell, "voltage"),
        [Value::Null, json!(4150), json!(4160), json!(4170)]
    );
}

#[test]
fn the_sleep_clears_the_window_and_the_wake_sends_the_state_at_once() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.send(Event::PowerRead(ON_BATTERY));
    shell.send(Event::PowerRead(Reading {
        current_ma: Some(-50),
        ..ON_BATTERY
    }));
    shell.send(Event::PowerRead(Reading {
        current_ma: Some(-60),
        ..ON_BATTERY
    }));
    assert!(window_waits(&shell));
    let sent = on(&shell, STATE).len();
    shell.take_cleared();

    // The end of the idle time sleeps. The session ends before the `Sleep`
    // request, and its end clears the timer of the window. The test shell
    // checks that no timer waits at the `Sleep` request.
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert!(shell.take_cleared().contains(&STATE_MIN_INTERVAL));
    assert_eq!(shell.timers_at_sleep.last(), Some(&Vec::new()));
    assert_eq!(on(&shell, STATE).len(), sent);

    // A reading while the device sleeps only changes the model: no state,
    // no draw, no wake.
    let frames = shell.frames.len();
    let writes = shell.writes.len();
    let asleep = Reading {
        current_ma: Some(-10),
        ..ON_BATTERY
    };
    shell.send(Event::PowerRead(asleep));
    assert_eq!(shell.model.power.reading, asleep);
    assert!(shell.model.power.is_asleep());
    assert!(shell.sleep.is_some());
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.writes.len(), writes);

    // The wake sends the state at once, with the reading of the sleep.
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(on(&shell, STATE).len(), sent + 1);
    assert_eq!(states(&shell)[sent]["current"], -10);
}

#[test]
fn a_dry_run_starts_no_window() {
    let mut shell = house();
    let mut start = start(PowerSource::Stand);
    if let Event::Start(start) = &mut start {
        start.settings.read_only = true;
    }
    shell.send(start);
    shell.send(Event::PowerRead(CHARGING));
    shell.send(Event::PowerRead(charging_at(4160)));
    assert_eq!(on(&shell, STATE), Vec::<String>::new());
    assert!(!window_waits(&shell));
}

#[test]
fn a_reading_then_a_source_change_send_two_state_messages() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.send(Event::PowerRead(CHARGING));
    let before = on(&shell, STATE).len();

    // The plug comes out. The shell sends the new reading first, then the
    // new source, as the firmware does in one pass. The core publishes at
    // the end of each event, so the first message has the new charging and
    // the old stand. The second message waits for the PUBACK of the first.
    shell.queue(Event::PowerRead(ON_BATTERY));
    shell.queue(Event::PowerSourceChanged(PowerSource::Battery));
    shell.run();

    let sent = &states(&shell)[before..];
    assert_eq!(sent.len(), 2);
    assert_eq!(
        (&sent[0]["charging"], &sent[0]["stand"]),
        (&json!("OFF"), &json!("ON"))
    );
    assert_eq!(
        (&sent[1]["charging"], &sent[1]["stand"]),
        (&json!("OFF"), &json!("OFF"))
    );
}
