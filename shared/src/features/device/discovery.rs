//! The discovery message: one device-based discovery config of Home
//! Assistant (2024.11 and later), with abbreviated keys.
//!
//! The config has the device (`dev`), the origin (`o`) and eight
//! components (`cmps`). Each component has the platform `p` and a
//! `uniq_id` of `dashboard_<id>_<key>`. Refer to `docs/mqtt.md`.
//!
//! | Component | Platform | Device class | State topic |
//! | --- | --- | --- | --- |
//! | `status` | `sensor` | `enum`: `online`, `asleep`, `offline`, icon `mdi:tablet-dashboard` | the status topic |
//! | `battery` | `sensor` | `battery`, in `%` | the state topic, key `battery` |
//! | `stand` | `binary_sensor` | `plug` | the state topic, key `stand` |
//! | `charging` | `binary_sensor` | `battery_charging`, diagnostic | the state topic, key `charging` |
//! | `charge` | `sensor` | `enum`: [`CHARGE_LABELS`], icon `mdi:battery-charging-outline`, diagnostic | the state topic, key `charge` |
//! | `voltage` | `sensor` | `voltage`, in `mV`, icon `mdi:flash-triangle-outline`, diagnostic | the state topic, key `voltage` |
//! | `current` | `sensor` | `current`, in `mA`, icon `mdi:current-dc`, diagnostic | the state topic, key `current` |
//! | `temperature` | `sensor` | `temperature`, in `°C`, diagnostic | the state topic, key `temperature` |
//!
//! The status sensor has no availability, so it shows `offline` too. The
//! other components are available unless the status is `offline`. Thus
//! they keep their last value while the device is asleep.
//!
//! The state message keeps the `snake_case` values of [`CHARGE_OPTIONS`](super::CHARGE_OPTIONS). The
//! value template of the `charge` component maps them to the labels of
//! [`CHARGE_LABELS`]. The other components use the default icon of their
//! device class.
//!
//! The voltage is in mV, not in V. The core then sends a whole number, so
//! the payload is exact, and the snapshot can compare it. Home Assistant
//! can show the sensor in V, because it converts the units of a voltage.
//!
//! Each component has its own options, and the config has no shared
//! options at its root. A shared availability would apply to the status
//! sensor too.

use alloc::{format, string::String, vec::Vec};

use serde::Serialize;

use crate::{
    Firmware, Settings,
    features::connection::{STATUS_ASLEEP, STATUS_OFFLINE, STATUS_ONLINE},
};

/// The manufacturer of the board.
const MANUFACTURER: &str = "LilyGo";
/// The model of the board.
const MODEL: &str = "T5 E-Paper S3 Pro";
/// The name of the software in the origin.
const ORIGIN: &str = "crux-ha-dashboard";

/// The availability template of each component except the status: it is
/// available unless the status is `offline`.
const AVAILABLE_UNLESS_OFFLINE: &str = "{{ 'offline' if value == 'offline' else 'online' }}";

/// The labels of the charge options that Home Assistant shows, in the order
/// of [`CHARGE_OPTIONS`](super::CHARGE_OPTIONS).
const CHARGE_LABELS: [&str; 4] = ["Not charging", "Pre-charging", "Fast charging", "Done"];

/// The value template of the charge component. It maps each wire value to
/// its label. A null or unknown value gives `None`, so Home Assistant shows
/// the state as unknown.
const CHARGE_TEMPLATE: &str = "{{ {'not_charging': 'Not charging', 'pre_charging': 'Pre-charging', 'fast_charging': 'Fast charging', 'done': 'Done'}[value_json.charge] | default(None) }}";

/// The entity category of the power diagnostics.
const DIAGNOSTIC: Option<&str> = Some("diagnostic");

/// Returns the discovery topic of the device, for example
/// `homeassistant/device/dashboard_hall/config`.
pub fn topic(settings: &Settings) -> String {
    format!("homeassistant/device/{}/config", settings.unique_id())
}

/// Returns the JSON payload of the discovery message.
pub fn payload(settings: &Settings, firmware: &Firmware) -> Vec<u8> {
    let id = settings.unique_id();
    let status_topic = settings.status_topic();
    let state_topic = settings.state_topic();
    let mac = mac_text(firmware.mac);
    let config = Config {
        dev: Device {
            ids: &id,
            name: format!("Dashboard {}", settings.device_id),
            mf: MANUFACTURER,
            mdl: MODEL,
            sw: &firmware.version,
            cns: [["mac", &mac]],
        },
        o: Origin {
            name: ORIGIN,
            sw: &firmware.version,
        },
        cmps: components(&id, &status_topic, &state_topic),
    };
    // The fields are text, and the writer is a `Vec`, so the serialization
    // cannot fail.
    serde_json::to_vec(&config).expect("a payload of text serializes")
}

/// Returns the components of the device with the unique id `id`.
fn components<'a>(id: &str, status_topic: &'a str, state_topic: &'a str) -> Components<'a> {
    let unique = |key: &str| format!("{id}_{key}");
    let measurement = |key: &str, name, dev_cla, ic, unit_of_meas, val_tpl, ent_cat| Measurement {
        p: "sensor",
        name,
        uniq_id: unique(key),
        dev_cla,
        ic,
        unit_of_meas,
        stat_cla: "measurement",
        stat_t: state_topic,
        val_tpl,
        avty_t: status_topic,
        avty_tpl: AVAILABLE_UNLESS_OFFLINE,
        ent_cat,
    };
    let binary = |key: &str, name, dev_cla, val_tpl, ent_cat| Binary {
        p: "binary_sensor",
        name,
        uniq_id: unique(key),
        dev_cla,
        stat_t: state_topic,
        val_tpl,
        avty_t: status_topic,
        avty_tpl: AVAILABLE_UNLESS_OFFLINE,
        ent_cat,
    };
    Components {
        status: Status {
            p: "sensor",
            name: "Status",
            uniq_id: unique("status"),
            dev_cla: "enum",
            ic: "mdi:tablet-dashboard",
            ops: [STATUS_ONLINE, STATUS_ASLEEP, STATUS_OFFLINE],
            stat_t: status_topic,
        },
        battery: measurement(
            "battery",
            "Battery",
            "battery",
            None,
            "%",
            "{{ value_json.battery }}",
            None,
        ),
        stand: binary("stand", "On stand", "plug", "{{ value_json.stand }}", None),
        charging: binary(
            "charging",
            "Charging",
            "battery_charging",
            "{{ value_json.charging }}",
            DIAGNOSTIC,
        ),
        charge: Charge {
            p: "sensor",
            name: "Charge state",
            uniq_id: unique("charge"),
            dev_cla: "enum",
            ic: "mdi:battery-charging-outline",
            ops: CHARGE_LABELS,
            stat_t: state_topic,
            val_tpl: CHARGE_TEMPLATE,
            avty_t: status_topic,
            avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            ent_cat: "diagnostic",
        },
        voltage: measurement(
            "voltage",
            "Battery voltage",
            "voltage",
            Some("mdi:flash-triangle-outline"),
            "mV",
            "{{ value_json.voltage }}",
            DIAGNOSTIC,
        ),
        current: measurement(
            "current",
            "Battery current",
            "current",
            Some("mdi:current-dc"),
            "mA",
            "{{ value_json.current }}",
            DIAGNOSTIC,
        ),
        temperature: measurement(
            "temperature",
            "Gauge temperature",
            "temperature",
            None,
            "°C",
            "{{ value_json.temperature }}",
            DIAGNOSTIC,
        ),
    }
}

/// Returns the MAC address as lowercase hex with colons, for example
/// `28:84:85:37:b8:a0`.
fn mac_text(mac: [u8; 6]) -> String {
    let bytes: Vec<String> = mac.iter().map(|byte| format!("{byte:02x}")).collect();
    bytes.join(":")
}

#[derive(Serialize)]
struct Config<'a> {
    dev: Device<'a>,
    o: Origin<'a>,
    cmps: Components<'a>,
}

#[derive(Serialize)]
struct Device<'a> {
    ids: &'a str,
    name: String,
    mf: &'static str,
    mdl: &'static str,
    sw: &'a str,
    /// The connections: each one is a type and a value.
    cns: [[&'a str; 2]; 1],
}

#[derive(Serialize)]
struct Origin<'a> {
    name: &'static str,
    sw: &'a str,
}

#[derive(Serialize)]
struct Components<'a> {
    status: Status<'a>,
    battery: Measurement<'a>,
    stand: Binary<'a>,
    charging: Binary<'a>,
    charge: Charge<'a>,
    voltage: Measurement<'a>,
    current: Measurement<'a>,
    temperature: Measurement<'a>,
}

#[derive(Serialize)]
struct Status<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    ic: &'static str,
    ops: [&'static str; 3],
    stat_t: &'a str,
}

/// A sensor with a unit and the state class `measurement`.
#[derive(Serialize)]
struct Measurement<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ic: Option<&'static str>,
    unit_of_meas: &'static str,
    stat_cla: &'static str,
    stat_t: &'a str,
    val_tpl: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ent_cat: Option<&'static str>,
}

/// A binary sensor with the default payloads `ON` and `OFF`.
#[derive(Serialize)]
struct Binary<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    stat_t: &'a str,
    val_tpl: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ent_cat: Option<&'static str>,
}

/// The enum sensor of the phase of the charge.
#[derive(Serialize)]
struct Charge<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    ic: &'static str,
    ops: [&'static str; 4],
    stat_t: &'a str,
    val_tpl: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
    ent_cat: &'static str,
}
