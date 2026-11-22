//! The discovery message: one device-based discovery config of Home
//! Assistant (2024.11 and later), with abbreviated keys.
//!
//! The config has the device (`dev`), the origin (`o`) and three
//! components (`cmps`). Each component has the platform `p` and a
//! `uniq_id` of `dashboard_<id>_<key>`. Refer to `docs/mqtt.md`.
//!
//! | Component | Platform | Device class | State topic |
//! | --- | --- | --- | --- |
//! | `status` | `sensor` | `enum`: `online`, `asleep`, `offline` | the status topic |
//! | `battery` | `sensor` | `battery`, in `%` | the state topic, key `battery` |
//! | `stand` | `binary_sensor` | `plug` | the state topic, key `stand` |
//!
//! The status sensor has no availability, so it shows `offline` too. The
//! other two are available unless the status is `offline`. Thus they keep
//! their last value while the device is asleep.
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

/// The availability template of the battery and the stand sensors: they
/// are available unless the status is `offline`.
const AVAILABLE_UNLESS_OFFLINE: &str = "{{ 'offline' if value == 'offline' else 'online' }}";

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
    let unique = |key: &str| format!("{id}_{key}");
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
        cmps: Components {
            status: Status {
                p: "sensor",
                name: "Status",
                uniq_id: unique("status"),
                dev_cla: "enum",
                ops: [STATUS_ONLINE, STATUS_ASLEEP, STATUS_OFFLINE],
                stat_t: &status_topic,
            },
            battery: Battery {
                p: "sensor",
                name: "Battery",
                uniq_id: unique("battery"),
                dev_cla: "battery",
                unit_of_meas: "%",
                stat_cla: "measurement",
                stat_t: &state_topic,
                val_tpl: "{{ value_json.battery }}",
                avty_t: &status_topic,
                avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            },
            stand: Stand {
                p: "binary_sensor",
                name: "On stand",
                uniq_id: unique("stand"),
                dev_cla: "plug",
                stat_t: &state_topic,
                val_tpl: "{{ value_json.stand }}",
                avty_t: &status_topic,
                avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            },
        },
    };
    // The fields are text, and the writer is a `Vec`, so the serialization
    // cannot fail.
    serde_json::to_vec(&config).expect("a payload of text serializes")
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
    battery: Battery<'a>,
    stand: Stand<'a>,
}

#[derive(Serialize)]
struct Status<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    ops: [&'static str; 3],
    stat_t: &'a str,
}

#[derive(Serialize)]
struct Battery<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    unit_of_meas: &'static str,
    stat_cla: &'static str,
    stat_t: &'a str,
    val_tpl: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
}

#[derive(Serialize)]
struct Stand<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    stat_t: &'a str,
    val_tpl: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
}
