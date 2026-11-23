//! The discovery message: one device-based discovery config of Home
//! Assistant (2024.11 and later), with abbreviated keys.
//!
//! The config has the device (`dev`), the origin (`o`) and twenty-one
//! components (`cmps`). Each component has the platform `p` and a
//! `uniq_id` of `dashboard_<id>_<key>`. Refer to `docs/mqtt.md`.
//!
//! | Component | Platform | Device class | State topic |
//! | --- | --- | --- | --- |
//! | `status` | `sensor` | `enum`: `online`, `asleep`, `offline`, `restarting`, icon `mdi:tablet-dashboard` | the status topic |
//! | `battery` | `sensor` | `battery`, in `%` | the state topic, key `battery` |
//! | `stand` | `binary_sensor` | `plug` | the state topic, key `stand` |
//! | `charging` | `binary_sensor` | `battery_charging`, diagnostic | the state topic, key `charging` |
//! | `charge` | `sensor` | `enum`: [`CHARGE_LABELS`], icon `mdi:battery-charging-outline`, diagnostic | the state topic, key `charge` |
//! | `voltage` | `sensor` | `voltage`, in `mV`, icon `mdi:flash-triangle-outline`, diagnostic | the state topic, key `voltage` |
//! | `current` | `sensor` | `current`, in `mA`, icon `mdi:current-dc`, diagnostic | the state topic, key `current` |
//! | `temperature` | `sensor` | `temperature`, in `°C`, diagnostic | the state topic, key `temperature` |
//! | `rssi` | `sensor` | `signal_strength`, in `dBm`, diagnostic | the state topic, key `rssi` |
//! | `bssid` | `sensor` | none, icon `mdi:access-point`, diagnostic | the state topic, key `bssid` |
//! | `ip` | `sensor` | none, icon `mdi:ip-network`, diagnostic | the state topic, key `ip` |
//! | `reconnects` | `sensor` | none, `total_increasing`, icon `mdi:lan-connect`, diagnostic | the state topic, key `reconnects` |
//! | `restarts` | `sensor` | none, `total_increasing`, icon `mdi:restart`, diagnostic | the state topic, key `restarts` |
//! | `reset` | `sensor` | `enum`: [`RESET_LABELS`], icon `mdi:power-cycle`, diagnostic | the state topic, key `reset` |
//! | `wake` | `sensor` | `enum`: [`WAKE_LABELS`], icon `mdi:sleep-off`, diagnostic | the state topic, key `wake` |
//! | `sleep` | `button` | none, icon `mdi:sleep` | command topic `dashboard/<id>/set/button`, payload `sleep` |
//! | `restart` | `button` | `restart`, config | the command topic, payload `restart` |
//! | `refresh` | `button` | none, icon `mdi:refresh`, config | the command topic, payload `refresh` |
//! | `light` | `light` | the JSON schema, brightness only, scale 100 | the light topic, command topic `dashboard/<id>/set/light` |
//! | `tap_level_stand` | `number` | 0 to 100 in `%`, icon `mdi:gesture-tap`, config, retained | `dashboard/<id>/set/tap_level_stand`, also its command topic |
//! | `tap_level_battery` | `number` | as `tap_level_stand` | `dashboard/<id>/set/tap_level_battery`, also its command topic |
//!
//! The status sensor has no availability, so it shows `offline` too. The
//! sensors are available unless the status is `offline`. Thus they keep
//! their last value while the device is asleep or restarts.
//!
//! The buttons are available only while the status is `online`. Home
//! Assistant compares the result of the availability template with
//! `online` and `offline`, and ignores each other result. So the template
//! must turn `asleep` and `restarting` into `offline`. The Sleep now button also needs the
//! device on battery: it has a second availability on the `stand` key of
//! the state topic, with the mode `all`. On the stand, a sleep does
//! nothing, so Home Assistant does not show the button as available.
//!
//! The light is available only while the status is `online`, as the
//! buttons are. Home Assistant shows it as unavailable while the device
//! sleeps, because the device cannot obey a command then. The light has
//! the JSON schema, so one message has the state and the brightness. Its
//! color mode is `brightness` only, and the brightness scale is 100, so the
//! brightness is in percent on the wire.
//!
//! Each tap level is a number with the retain flag (`ret`). Home Assistant
//! publishes the value retained on its command topic, and the core reads
//! it from the broker at each connect. The state topic is the command
//! topic, so Home Assistant shows the value that the broker keeps. The
//! numbers are available unless the status is `offline`, so a user can
//! set a tap level while the device sleeps. The minimum is 0, because 0 is
//! the tap level at which a tap does not turn the light on.
//!
//! The state message keeps the `snake_case` values of
//! [`CHARGE_OPTIONS`](super::CHARGE_OPTIONS),
//! [`RESET_OPTIONS`](super::RESET_OPTIONS) and
//! [`WAKE_OPTIONS`](super::WAKE_OPTIONS). The value template of each enum
//! component maps them to its labels. The battery, stand, charging,
//! temperature and signal components use the default icon of their device
//! class. The other components set an icon, because they have no device
//! class or the icon of the class does not tell what they are.
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

use super::{BUTTON_SET, Button, LIGHT_SET, TAP_LEVEL_BATTERY_SET, TAP_LEVEL_STAND_SET};
use crate::{
    Firmware, Settings,
    capabilities::radio::mac_text,
    features::connection::{STATUS_ASLEEP, STATUS_OFFLINE, STATUS_ONLINE, STATUS_RESTARTING},
};

/// The manufacturer of the board.
const MANUFACTURER: &str = "LilyGo";
/// The model of the board.
const MODEL: &str = "T5 E-Paper S3 Pro";
/// The name of the software in the origin.
const ORIGIN: &str = "crux-ha-dashboard";

/// The availability template of each sensor except the status: it is
/// available unless the status is `offline`. It gives `online` for
/// `online`, `asleep` and `restarting`.
const AVAILABLE_UNLESS_OFFLINE: &str = "{{ 'offline' if value == 'offline' else 'online' }}";

/// The availability template of each button: it is available only while
/// the status is `online`. It gives `offline` for `asleep`, `offline` and
/// `restarting`.
const AVAILABLE_WHILE_ONLINE: &str = "{{ 'online' if value == 'online' else 'offline' }}";

/// The availability template on the state topic of the Sleep now button:
/// it is available only while the device is on battery.
const AVAILABLE_ON_BATTERY: &str = "{{ 'online' if value_json.stand == 'OFF' else 'offline' }}";

/// The entity category of the buttons for maintenance.
const CONFIG: &str = "config";

/// The labels of the charge options that Home Assistant shows, in the order
/// of [`CHARGE_OPTIONS`](super::CHARGE_OPTIONS).
const CHARGE_LABELS: [&str; 4] = ["Not charging", "Pre-charging", "Fast charging", "Done"];

/// The value template of the charge component. It maps each wire value to
/// its label. A null or unknown value gives `None`, so Home Assistant shows
/// the state as unknown.
const CHARGE_TEMPLATE: &str = "{{ {'not_charging': 'Not charging', 'pre_charging': 'Pre-charging', 'fast_charging': 'Fast charging', 'done': 'Done'}[value_json.charge] | default(None) }}";

/// The labels of the reset options that Home Assistant shows, in the order
/// of [`RESET_OPTIONS`](super::RESET_OPTIONS).
const RESET_LABELS: [&str; 6] = [
    "Power on", "Software", "Panic", "Watchdog", "Brownout", "Other",
];

/// The value template of the reset component. It maps each wire value to
/// its label, as [`CHARGE_TEMPLATE`] does.
const RESET_TEMPLATE: &str = "{{ {'power_on': 'Power on', 'software': 'Software', 'panic': 'Panic', 'watchdog': 'Watchdog', 'brownout': 'Brownout', 'other': 'Other'}[value_json.reset] | default(None) }}";

/// The labels of the wake options that Home Assistant shows, in the order
/// of [`WAKE_OPTIONS`](super::WAKE_OPTIONS).
const WAKE_LABELS: [&str; 3] = ["Boot", "Touch", "Stand"];

/// The value template of the wake component. It maps each wire value to
/// its label, as [`CHARGE_TEMPLATE`] does.
const WAKE_TEMPLATE: &str =
    "{{ {'boot': 'Boot', 'touch': 'Touch', 'stand': 'Stand'}[value_json.wake] | default(None) }}";

/// The entity category of the diagnostics.
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
    let button_topic = settings.set_topic(BUTTON_SET);
    let light_topic = settings.light_topic();
    let light_command = settings.set_topic(LIGHT_SET);
    let tap_stand = settings.set_topic(TAP_LEVEL_STAND_SET);
    let tap_battery = settings.set_topic(TAP_LEVEL_BATTERY_SET);
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
        cmps: components(&Make {
            id: &id,
            status_topic: &status_topic,
            state_topic: &state_topic,
            button_topic: &button_topic,
            light_topic: &light_topic,
            light_command: &light_command,
            tap_stand: &tap_stand,
            tap_battery: &tap_battery,
        }),
    };
    // The fields are text, and the writer is a `Vec`, so the serialization
    // cannot fail.
    serde_json::to_vec(&config).expect("a payload of text serializes")
}

/// Returns the components of the device.
fn components<'a>(make: &Make<'a>) -> Components<'a> {
    Components {
        power: power_components(make),
        diagnostics: diagnostic_components(make),
        buttons: button_components(make),
        light: LightComponents {
            light: make.light(),
            tap_level_stand: make.tap_level(
                TAP_LEVEL_STAND_SET,
                "Tap light on the stand",
                make.tap_stand,
            ),
            tap_level_battery: make.tap_level(
                TAP_LEVEL_BATTERY_SET,
                "Tap light on battery",
                make.tap_battery,
            ),
        },
    }
}

/// Returns the buttons, in the order of [`Button::ALL`].
fn button_components<'a>(make: &Make<'a>) -> ButtonComponents<'a> {
    ButtonComponents {
        sleep: SleepButton {
            p: "button",
            name: "Sleep now",
            uniq_id: make.unique(Button::Sleep.payload()),
            ic: "mdi:sleep",
            cmd_t: make.button_topic,
            pl_prs: Button::Sleep.payload(),
            avty: [
                Availability {
                    t: make.status_topic,
                    val_tpl: AVAILABLE_WHILE_ONLINE,
                },
                Availability {
                    t: make.state_topic,
                    val_tpl: AVAILABLE_ON_BATTERY,
                },
            ],
            avty_mode: "all",
        },
        restart: make.button(Button::Restart, "Restart", Some("restart"), None),
        refresh: make.button(Button::Refresh, "Full refresh", None, Some("mdi:refresh")),
    }
}

/// Returns the status, the battery, the stand and the power diagnostics.
fn power_components<'a>(make: &Make<'a>) -> PowerComponents<'a> {
    PowerComponents {
        status: Status {
            p: "sensor",
            name: "Status",
            uniq_id: make.unique("status"),
            dev_cla: "enum",
            ic: "mdi:tablet-dashboard",
            ops: [
                STATUS_ONLINE,
                STATUS_ASLEEP,
                STATUS_OFFLINE,
                STATUS_RESTARTING,
            ],
            stat_t: make.status_topic,
        },
        battery: make.measurement(("battery", "Battery", "battery", None, "%"), None),
        stand: make.binary("stand", "On stand", "plug", None),
        charging: make.binary("charging", "Charging", "battery_charging", DIAGNOSTIC),
        charge: make.choice(
            ("charge", "Charge state", "mdi:battery-charging-outline"),
            &CHARGE_LABELS,
            CHARGE_TEMPLATE,
        ),
        voltage: make.measurement(
            (
                "voltage",
                "Battery voltage",
                "voltage",
                Some("mdi:flash-triangle-outline"),
                "mV",
            ),
            DIAGNOSTIC,
        ),
        current: make.measurement(
            (
                "current",
                "Battery current",
                "current",
                Some("mdi:current-dc"),
                "mA",
            ),
            DIAGNOSTIC,
        ),
        temperature: make.measurement(
            (
                "temperature",
                "Gauge temperature",
                "temperature",
                None,
                "°C",
            ),
            DIAGNOSTIC,
        ),
    }
}

/// Returns the Wi-Fi and boot diagnostics.
fn diagnostic_components<'a>(make: &Make<'a>) -> DiagnosticComponents<'a> {
    DiagnosticComponents {
        rssi: make.measurement(
            ("rssi", "Wi-Fi signal", "signal_strength", None, "dBm"),
            DIAGNOSTIC,
        ),
        bssid: make.plain(("bssid", "Access point", "mdi:access-point"), None),
        ip: make.plain(("ip", "IP address", "mdi:ip-network"), None),
        reconnects: make.plain(
            ("reconnects", "Reconnects", "mdi:lan-connect"),
            Some("total_increasing"),
        ),
        restarts: make.plain(
            ("restarts", "Restarts", "mdi:restart"),
            Some("total_increasing"),
        ),
        reset: make.choice(
            ("reset", "Last reset", "mdi:power-cycle"),
            &RESET_LABELS,
            RESET_TEMPLATE,
        ),
        wake: make.choice(
            ("wake", "Wake reason", "mdi:sleep-off"),
            &WAKE_LABELS,
            WAKE_TEMPLATE,
        ),
    }
}

/// Makes the components of one device. Each sensor except the status
/// reads its key of the state topic, and is available unless the status is
/// `offline`. Each button publishes on the button topic.
struct Make<'a> {
    /// The unique id of the device.
    id: &'a str,
    status_topic: &'a str,
    state_topic: &'a str,
    button_topic: &'a str,
    /// The state topic of the light.
    light_topic: &'a str,
    /// The command topic of the light.
    light_command: &'a str,
    /// The topic of the tap level on the stand.
    tap_stand: &'a str,
    /// The topic of the tap level on battery.
    tap_battery: &'a str,
}

/// The key, the name, the device class, the icon and the unit of a
/// measurement.
type MeasurementId = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
    &'static str,
);

/// The key, the name and the icon of a component.
type Named = (&'static str, &'static str, &'static str);

impl<'a> Make<'a> {
    /// Returns the unique id of the component `key`.
    fn unique(&self, key: &str) -> String {
        format!("{}_{key}", self.id)
    }

    /// Returns the value template that reads the key `key`.
    fn value(key: &str) -> String {
        format!("{{{{ value_json.{key} }}}}")
    }

    /// Returns a sensor with a unit and the state class `measurement`.
    fn measurement(
        &self,
        (key, name, dev_cla, ic, unit_of_meas): MeasurementId,
        ent_cat: Option<&'static str>,
    ) -> Measurement<'a> {
        Measurement {
            p: "sensor",
            name,
            uniq_id: self.unique(key),
            dev_cla,
            ic,
            unit_of_meas,
            stat_cla: "measurement",
            stat_t: self.state_topic,
            val_tpl: Self::value(key),
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            ent_cat,
        }
    }

    /// Returns a binary sensor with the default payloads `ON` and `OFF`.
    fn binary(
        &self,
        key: &str,
        name: &'static str,
        dev_cla: &'static str,
        ent_cat: Option<&'static str>,
    ) -> Binary<'a> {
        Binary {
            p: "binary_sensor",
            name,
            uniq_id: self.unique(key),
            dev_cla,
            stat_t: self.state_topic,
            val_tpl: Self::value(key),
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            ent_cat,
        }
    }

    /// Returns a diagnostic sensor with no device class and no unit, with
    /// the state class `stat_cla` if it has one.
    fn plain(&self, (key, name, ic): Named, stat_cla: Option<&'static str>) -> Plain<'a> {
        Plain {
            p: "sensor",
            name,
            uniq_id: self.unique(key),
            ic,
            stat_cla,
            stat_t: self.state_topic,
            val_tpl: Self::value(key),
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            ent_cat: "diagnostic",
        }
    }

    /// Returns a button for maintenance, in the entity category `config`,
    /// that is available only while the status is `online`.
    fn button(
        &self,
        button: Button,
        name: &'static str,
        dev_cla: Option<&'static str>,
        ic: Option<&'static str>,
    ) -> ButtonComponent<'a> {
        ButtonComponent {
            p: "button",
            name,
            uniq_id: self.unique(button.payload()),
            dev_cla,
            ic,
            ent_cat: CONFIG,
            cmd_t: self.button_topic,
            pl_prs: button.payload(),
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_WHILE_ONLINE,
        }
    }

    /// Returns the front light: the JSON schema, with the brightness in
    /// percent. It is available only while the status is `online`.
    fn light(&self) -> LightComponent<'a> {
        LightComponent {
            p: "light",
            name: "Front light",
            uniq_id: self.unique(LIGHT_SET),
            schema: "json",
            sup_clrm: ["brightness"],
            bri_scl: 100,
            stat_t: self.light_topic,
            cmd_t: self.light_command,
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_WHILE_ONLINE,
        }
    }

    /// Returns the number of the tap level `key` on `topic`, from 0 to 100
    /// in percent. Home Assistant publishes it retained on `topic`, and
    /// reads its state from `topic`. It is available unless the status is
    /// `offline`.
    fn tap_level(&self, key: &str, name: &'static str, topic: &'a str) -> Number<'a> {
        Number {
            p: "number",
            name,
            uniq_id: self.unique(key),
            ic: "mdi:gesture-tap",
            ent_cat: CONFIG,
            cmd_t: topic,
            stat_t: topic,
            ret: true,
            min: 0,
            max: 100,
            step: 1,
            unit_of_meas: "%",
            mode: "slider",
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_UNLESS_OFFLINE,
        }
    }

    /// Returns a diagnostic enum sensor with the labels `ops` and the value
    /// template `val_tpl` that maps the wire values to them.
    fn choice(
        &self,
        (key, name, ic): Named,
        ops: &'static [&'static str],
        val_tpl: &'static str,
    ) -> Choice<'a> {
        Choice {
            p: "sensor",
            name,
            uniq_id: self.unique(key),
            dev_cla: "enum",
            ic,
            ops,
            stat_t: self.state_topic,
            val_tpl,
            avty_t: self.status_topic,
            avty_tpl: AVAILABLE_UNLESS_OFFLINE,
            ent_cat: "diagnostic",
        }
    }
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

/// The components, in the order of the payload. The three groups are one
/// map in the JSON.
#[derive(Serialize)]
struct Components<'a> {
    #[serde(flatten)]
    power: PowerComponents<'a>,
    #[serde(flatten)]
    diagnostics: DiagnosticComponents<'a>,
    #[serde(flatten)]
    buttons: ButtonComponents<'a>,
    #[serde(flatten)]
    light: LightComponents<'a>,
}

#[derive(Serialize)]
struct LightComponents<'a> {
    light: LightComponent<'a>,
    tap_level_stand: Number<'a>,
    tap_level_battery: Number<'a>,
}

/// The front light, with the JSON schema.
#[derive(Serialize)]
struct LightComponent<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    schema: &'static str,
    sup_clrm: [&'static str; 1],
    bri_scl: u8,
    stat_t: &'a str,
    cmd_t: &'a str,
    avty_t: &'a str,
    avty_tpl: &'static str,
}

/// A number of a tap level, with the retain flag.
#[derive(Serialize)]
struct Number<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    ic: &'static str,
    ent_cat: &'static str,
    cmd_t: &'a str,
    stat_t: &'a str,
    ret: bool,
    min: u8,
    max: u8,
    step: u8,
    unit_of_meas: &'static str,
    mode: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
}

#[derive(Serialize)]
struct ButtonComponents<'a> {
    sleep: SleepButton<'a>,
    restart: ButtonComponent<'a>,
    refresh: ButtonComponent<'a>,
}

/// The Sleep now button. It has two availabilities, and both must be
/// `online`.
#[derive(Serialize)]
struct SleepButton<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    ic: &'static str,
    cmd_t: &'a str,
    pl_prs: &'static str,
    avty: [Availability<'a>; 2],
    avty_mode: &'static str,
}

/// One availability of a component: a topic and its template.
#[derive(Serialize)]
struct Availability<'a> {
    t: &'a str,
    val_tpl: &'static str,
}

/// A button for maintenance.
#[derive(Serialize)]
struct ButtonComponent<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_cla: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ic: Option<&'static str>,
    ent_cat: &'static str,
    cmd_t: &'a str,
    pl_prs: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
}

#[derive(Serialize)]
struct PowerComponents<'a> {
    status: Status<'a>,
    battery: Measurement<'a>,
    stand: Binary<'a>,
    charging: Binary<'a>,
    charge: Choice<'a>,
    voltage: Measurement<'a>,
    current: Measurement<'a>,
    temperature: Measurement<'a>,
}

#[derive(Serialize)]
struct DiagnosticComponents<'a> {
    rssi: Measurement<'a>,
    bssid: Plain<'a>,
    ip: Plain<'a>,
    reconnects: Plain<'a>,
    restarts: Plain<'a>,
    reset: Choice<'a>,
    wake: Choice<'a>,
}

#[derive(Serialize)]
struct Status<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    ic: &'static str,
    ops: [&'static str; 4],
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
    val_tpl: String,
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
    val_tpl: String,
    avty_t: &'a str,
    avty_tpl: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ent_cat: Option<&'static str>,
}

/// A sensor with no device class and no unit, for a text or a count. It
/// is a diagnostic.
#[derive(Serialize)]
struct Plain<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    ic: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stat_cla: Option<&'static str>,
    stat_t: &'a str,
    val_tpl: String,
    avty_t: &'a str,
    avty_tpl: &'static str,
    ent_cat: &'static str,
}

/// An enum sensor with labels, for example the phase of the charge.
#[derive(Serialize)]
struct Choice<'a> {
    p: &'static str,
    name: &'static str,
    uniq_id: String,
    dev_cla: &'static str,
    ic: &'static str,
    ops: &'static [&'static str],
    stat_t: &'a str,
    val_tpl: &'static str,
    avty_t: &'a str,
    avty_tpl: &'static str,
    ent_cat: &'static str,
}
