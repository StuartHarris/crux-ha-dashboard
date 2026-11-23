//! The device feature: the dashboard as a device of Home Assistant, and the
//! queue of the device publishes.
//!
//! # The device publish queue
//!
//! The device publishes are the messages that the dashboard sends about
//! itself: the discovery message, the state of its front light, the state
//! of its sensors and the lines of its log. Each one is a publisher.
//! [`drain`] is the queue. [`connection::pump`] calls it after the outbox,
//! while the connection is up.
//!
//! The queue asks each publisher for its next message, in this order:
//! discovery, light, state, log. It gives each message to the session, and
//! tells the publisher the packet id. It stops when one of these is true:
//!
//! - Each publisher has no message now.
//! - [`IN_FLIGHT`] device publishes wait for a PUBACK. The queue then
//!   leaves [`RESERVED`] places for the status and the service calls.
//! - The session takes no more publishes now ([`Refused::Full`]). The next
//!   `Sent` or `Acked` runs [`connection::pump`] again, and the queue
//!   continues.
//!
//! The queue publishes nothing while the connection is not up, nothing
//! while the write of the CONNACK waits to go, and nothing from the start
//! of a sleep, except in the hourly report. In a report, the queue sends
//! as when the device is awake, and the report ends when the queue is
//! settled ([`settled`]). The connection can still be up in the first
//! step of a sleep, but its DISCONNECT soon follows, so a PUBACK is often
//! lost. Each PUBACK goes to the publisher that owns its packet id
//! ([`acked`]).
//!
//! The queue holds no state of its own. Each publisher keeps the state of
//! its messages, so the queue cannot contradict a publisher.
//!
//! | Publisher | Its state | Where the state is |
//! | --- | --- | --- |
//! | discovery | [`Discovery`] | [`State`], in the model |
//! | light | [`light::Sync`] | the [`Live`](connection::Live) of the session |
//! | state | [`StateSync`] | the [`Live`](connection::Live) of the session |
//! | log | [`logs::Flight`] | the [`Live`](connection::Live) of the session |
//!
//! # Discovery
//!
//! The discovery message tells Home Assistant about the device and its
//! sensors. Refer to [`discovery`] for the payload. The message is retained,
//! so the broker keeps it through a sleep and through a restart of Home
//! Assistant.
//!
//! | Discovery | Event | Next | What the core does |
//! | --- | --- | --- | --- |
//! | any | `Start` | `Due` | the queue publishes it after the connect |
//! | `Due` | the session takes it | `InFlight` | nothing |
//! | `InFlight` | its PUBACK | `Sent` | nothing |
//! | `InFlight` | the session ends | `Due` | the next connection publishes it |
//! | `Sent` | a birth message of Home Assistant that is not retained | `Due` | the queue publishes it again |
//! | `Due` or `InFlight` | a birth message | (no change) | nothing |
//!
//! - A wake on battery is not a `Start`, so it sends no discovery.
//! - Home Assistant publishes its birth message ([`BIRTH_TOPIC`],
//!   [`BIRTH_ONLINE`]) when it starts. The core ignores a retained birth
//!   message, because it is from an earlier start of Home Assistant.
//! - A birth message during `InFlight` changes nothing. The message in
//!   flight is retained, so Home Assistant gets it when it subscribes.
//! - The same birth message also makes the state of the light and the state
//!   message due again in the session that is live. Refer to
//!   [`light::Sync::again`] and [`StateSync`]. The broker can have
//!   restarted with Home Assistant, and then it lost both retained messages.
//!
//! # Dry run
//!
//! During a dry run ([`Settings::read_only`](crate::Settings::read_only)),
//! the core publishes no discovery message, no state of the light and no
//! state message. The core still obeys the commands of the light. The
//! bring-up binary of the firmware uses a dry run, so it does not appear in
//! Home Assistant as a device. The log still goes. When the dry run stops,
//! the queue publishes the discovery message (if it is due), the state of
//! the light and the state.
//!
//! # State
//!
//! The state message has the values of the sensors as one JSON object. It
//! is retained. Refer to [`Snapshot`].
//!
//! The state goes once on each connection. After that, the core sorts each
//! change of the snapshot ([`changed`]) into one of two kinds:
//!
//! - An urgent change goes at once: a change of the stand, a start or an
//!   end of a charge, the first level of the battery, a level of the
//!   battery that moves by more than [`BATTERY_FLAP`], a change of the
//!   access point or of the address (also to no link), a change of the boot
//!   facts (the restarts and the reset), and a change of the wake reason.
//! - Each other change goes at most once in [`STATE_MIN_INTERVAL`]: the
//!   voltage, the current, the temperature, the phase of the charge in a
//!   charge or out of it, a level of the battery that moves by
//!   [`BATTERY_FLAP`], the signal of the access point, and the count of
//!   reconnects. Thus a value that flaps between two readings cannot
//!   publish each poll of the shell.
//!
//! The access point, the address, the restarts and the reset do not change
//! during a session. A new session publishes the state at once, so only
//! their urgency in [`Snapshot::is_urgent_after`] keeps them out of the
//! throttle. The signal changes during a session at a scan that sees the
//! access point, and at a read of the signal before a change that is not
//! urgent. Refer to [`wifi`](crate::features::wifi).
//!
//! The [`Window`] is the throttle. While it is `Open`, a change that is
//! not urgent can go. First the core reads the signal of the access point
//! ([`wifi::read_signal`]), and the window is then `Fresh`. The change goes
//! with the new signal, and the core starts a timer of
//! [`STATE_MIN_INTERVAL`]. The window is then `Throttled`, and each change
//! that is not urgent waits. When the timer expires, the window is `Open`
//! again. The snapshot of that time then goes after a read, if it is not
//! equal to the last one, and the core starts a new timer. An urgent change
//! goes at once also while the window is `Throttled`, with the values that
//! wait, and the timer continues.
//!
//! | `StateSync` | Event | Next | What the core does |
//! | --- | --- | --- | --- |
//! | (none) | a new session | `Unsent` | the queue publishes the snapshot |
//! | `Unsent` | the session takes it | `InFlight`, `Open` | nothing |
//! | `InFlight` | a value changes | `InFlight` | nothing yet |
//! | `InFlight` | its PUBACK | `Sent` | the queue publishes if the snapshot is due |
//! | `Sent` | a birth message of Home Assistant that is not retained | `Unsent` | the queue publishes the snapshot, and the core clears the timer |
//! | `InFlight` | a birth message | `InFlight`, `again` | after the PUBACK, `Unsent` |
//! | `Sent` | an urgent change | `InFlight` | the queue publishes it, and the window does not change |
//! | `Sent`, `Open` | a change that is not urgent, and the core can read the signal | `Sent`, `Open` | the core reads the signal, and the change waits |
//! | `Sent`, `Open` | a change that is not urgent, and the core cannot read the signal | `InFlight`, `Throttled` | the queue publishes it, and the core starts the timer |
//! | `Open` | the end of the read, or a scan that gives a new signal | `Fresh` | the queue publishes if the snapshot is due |
//! | `Sent`, `Fresh` | a change that is not urgent | `InFlight`, `Throttled` | the queue publishes it, and the core starts the timer |
//! | `Sent`, `Fresh` | the snapshot is equal to the last one | `Sent`, `Open` | nothing |
//! | `Sent`, `Throttled` | a change that is not urgent | `Sent`, `Throttled` | nothing yet |
//! | `Throttled` | the timer expires | `Open` | the queue publishes if the snapshot is due |
//! | any | the session ends | (none) | the core clears the timer |
//!
//! The read cannot start a loop. Only a change that is not urgent and can
//! go now starts a read, and the window is then `Fresh` until the change
//! goes. The new signal is not urgent, so it does not defeat the throttle.
//! Thus the core reads once for each state message with a change that is
//! not urgent. If the change goes back during the read, nothing goes, and
//! the next change reads again. The read ends at its answer, with or
//! without a signal, or at its time limit. If a newer radio
//! request stops the read, the window stays `Open`, and the next change
//! that is not urgent goes with the signal that the link has, or reads
//! again.
//!
//! - At most one state message waits for a PUBACK.
//! - While the device sleeps, the queue publishes nothing. The next
//!   connection publishes the state.
//! - The timer ends with the session. Each sleep closes the session before
//!   its first `Sleep` request, so no timer of the throttle waits in a
//!   sleep. A timer of the core cannot wake the chip, and the next
//!   connection publishes the state at once.
//!
//! # Buttons
//!
//! Home Assistant shows three buttons of the device: Sleep now, Restart and
//! Full refresh. A press publishes the payload of the [`Button`] on
//! `dashboard/<id>/set/button`. The core subscribes to
//! `dashboard/<id>/set/+` ([`ANY_SET`]), and [`route`] takes each message on
//! a `set` topic before the entities get it.
//!
//! - The core ignores a retained button message, and writes an Info line.
//!   A retained `restart` would restart the chip at each connect.
//! - The core ignores a payload that it does not know, and writes a
//!   warning.
//! - The core ignores a message on another `set` topic.
//!
//! The light and its two tap levels have `set` topics too: [`LIGHT_SET`],
//! [`TAP_LEVEL_STAND_SET`] and [`TAP_LEVEL_BATTERY_SET`]. The core ignores
//! a retained light command, as it does a retained button message. It
//! applies a retained tap level, because Home Assistant keeps the tap
//! levels on the broker. Refer to [`light`].
//!
//! A button acts through [`Event::Pressed`], so it acts after the update
//! that received the message, outside [`connection::pump`]. It is not a
//! touch: it does not start the idle timer again, and it does not turn the
//! front light on. Refer to [`pressed`] for what each button does.

mod discovery;

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{mem, time::Duration};

use serde::Serialize;

use crate::{
    Boot, Cmd, Model, PowerSource, ResetReason, Settings,
    capabilities::{
        log::Area,
        power::{Charge, Reading},
        radio::{ip_text, mac_text},
    },
    features::{
        connection::{self, Link},
        light::{self, Level},
        logs,
        power::{self, IdleEnd, WakeReason},
        refresh::redraw,
        timer::Running,
        wifi,
    },
    mqtt::{MAX_IN_FLIGHT, Message, PacketId, Publish, Qos, Refused},
};

/// The number of places for publishes at `QoS` 1 that the device publishes
/// leave free in each session.
///
/// The session takes at most [`MAX_IN_FLIGHT`] publishes that wait for a
/// PUBACK. If the device publishes took all of them, a sleep could not
/// publish `asleep` and a restart could not publish `restarting` (the core
/// drops a refused status), and a service call would wait for a PUBACK of
/// the log. With these places free, the device publishes never block the
/// status or a few taps.
pub const RESERVED: usize = 8;

/// The largest number of device publishes that wait for a PUBACK in a
/// session: the discovery message, the light message, the state message and
/// the messages of the log, all together.
pub const IN_FLIGHT: usize = MAX_IN_FLIGHT - RESERVED;

/// The topic of the birth message of Home Assistant.
pub const BIRTH_TOPIC: &str = "homeassistant/status";

/// The payload of the birth message when Home Assistant starts.
pub const BIRTH_ONLINE: &str = "online";

/// The name of the `set` topic filter that matches each `set` topic of the
/// device. Refer to [`Settings::set_topic`](crate::Settings::set_topic).
pub const ANY_SET: &str = "+";

/// The name of the `set` topic of the buttons.
pub const BUTTON_SET: &str = "button";

/// The name of the `set` topic of the light.
pub const LIGHT_SET: &str = "light";

/// The name of the `set` topic of the tap level on the stand.
pub const TAP_LEVEL_STAND_SET: &str = "tap_level_stand";

/// The name of the `set` topic of the tap level on battery.
pub const TAP_LEVEL_BATTERY_SET: &str = "tap_level_battery";

/// The longest text of an unknown button payload in the log, in bytes.
const PAYLOAD_IN_LOG: usize = 32;

/// A button of the device in Home Assistant. Refer to the
/// [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Sleep now. The payload is `sleep`.
    Sleep,
    /// Restart the chip. The payload is `restart`.
    Restart,
    /// A full refresh of the panel. The payload is `refresh`.
    Refresh,
}

impl Button {
    /// Each button, in the order of the discovery message.
    pub const ALL: [Self; 3] = [Self::Sleep, Self::Restart, Self::Refresh];

    /// Returns the payload that Home Assistant publishes for a press.
    #[must_use]
    pub const fn payload(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Restart => "restart",
            Self::Refresh => "refresh",
        }
    }

    /// Returns the button of `payload`, or `None` for a payload that is
    /// not known.
    fn of(payload: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|button| button.payload().as_bytes() == payload)
    }
}

/// The shortest time between two state messages for changes that are not
/// urgent. Refer to the [module docs](self).
pub const STATE_MIN_INTERVAL: Duration = Duration::from_secs(60);

/// The largest move of the level of the battery, in percent, that is not
/// urgent. The gauge can flap between two levels, so a move of 1% goes at
/// most once in [`STATE_MIN_INTERVAL`].
pub const BATTERY_FLAP: u8 = 1;

/// The internal events of the device feature. The core makes them from its
/// timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The timer of the throttle window with this id expired. Refer to
    /// [`Window`].
    WindowEnded(u32),
    /// Home Assistant pressed a button of the device. Refer to [`route`].
    Pressed(Button),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Device(event)
    }
}

/// Handles an internal event of the device feature.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::WindowEnded(id) => window_ended(id, model),
        Event::Pressed(button) => pressed(button, model),
    }
}

/// The state of the device feature.
#[derive(Debug, Default)]
pub struct State {
    /// The publish of the discovery message.
    discovery: Discovery,
    /// The last id that the core took for a timer of the throttle window.
    /// Each new timer takes the next id, so an expiry from an older timer
    /// (also of an older session) does not match.
    window: u32,
}

impl State {
    /// Returns the state of the discovery message.
    #[must_use]
    pub const fn discovery(&self) -> Discovery {
        self.discovery
    }
}

/// The publish of the discovery message. Refer to the table in the
/// [module docs](self).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Discovery {
    /// The queue publishes the message on the next connection, or now if
    /// the connection is up.
    #[default]
    Due,
    /// The message waits for the PUBACK with this packet id.
    InFlight(PacketId),
    /// The broker acknowledged the message.
    Sent,
}

/// The publish of the state message on one session. Refer to the table in
/// the [module docs](self).
///
/// The sync is in the [`Live`](connection::Live) of the session, so each
/// session starts with `Unsent`. The end of the session clears the timer of
/// the window ([`StateSync::clear_timer`]).
#[derive(Debug, Default)]
pub enum StateSync {
    /// The session did not publish the state.
    #[default]
    Unsent,
    /// The state message waits for its PUBACK.
    InFlight {
        /// The packet id of the message.
        packet: PacketId,
        /// The snapshot in the message.
        sent: Snapshot,
        /// The throttle of the changes that are not urgent.
        window: Window,
        /// A birth message of Home Assistant came while the message waits,
        /// so the state goes again after the PUBACK.
        again: bool,
    },
    /// The broker acknowledged the state message with this snapshot.
    Sent {
        /// The snapshot in the last message.
        sent: Snapshot,
        /// The throttle of the changes that are not urgent.
        window: Window,
    },
}

impl StateSync {
    /// Clears the timer of the window, if it runs. The connection calls
    /// this function when the session ends, so the timer does not wait in a
    /// sleep.
    pub(crate) fn clear_timer(&mut self) {
        if let Self::InFlight { window, .. } | Self::Sent { window, .. } = self
            && let Window::Throttled(timer) = mem::take(window)
        {
            timer.clear();
        }
    }

    /// A birth message of Home Assistant came: the state goes again, also
    /// if it did not change. A message in flight goes again after its
    /// PUBACK. A sent state becomes `Unsent`, and the window closes.
    fn again(&mut self) {
        match self {
            Self::Unsent => {}
            Self::InFlight { again, .. } => *again = true,
            Self::Sent { .. } => {
                self.clear_timer();
                *self = Self::Unsent;
            }
        }
    }

    /// Returns the window, or `None` before the first state message.
    const fn window_mut(&mut self) -> Option<&mut Window> {
        match self {
            Self::Unsent => None,
            Self::InFlight { window, .. } | Self::Sent { window, .. } => Some(window),
        }
    }

    /// Takes the window, and leaves `Unsent`.
    fn take_window(&mut self) -> Window {
        match mem::take(self) {
            Self::Unsent => Window::Open,
            Self::InFlight { window, .. } | Self::Sent { window, .. } => window,
        }
    }
}

/// The throttle of the changes of the state that are not urgent. Refer to
/// the [module docs](self).
#[derive(Debug, Default)]
pub enum Window {
    /// The next change that is not urgent can go. It goes after a read of
    /// the signal, or at once if the core cannot read the signal.
    #[default]
    Open,
    /// The core read the signal in this window, so the next change that is
    /// not urgent goes at once.
    Fresh,
    /// A change that is not urgent went less than [`STATE_MIN_INTERVAL`]
    /// ago. The changes that are not urgent wait for this timer.
    Throttled(Running),
}

/// The values of the sensors in the state message.
///
/// The core compares the snapshot of the model with the last snapshot that
/// it sent. If they are not equal, it publishes the new snapshot, at once
/// or when the window is open ([`Snapshot::is_urgent_after`]). To add a sensor,
/// add a field here and a key to the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The charge of the battery in percent, or `None` if the shell did not
    /// read it.
    pub battery: Option<u8>,
    /// The device is on the stand.
    pub stand: bool,
    /// The power diagnostics.
    pub reading: Reading,
    /// The Wi-Fi link, or `None` if the device has no link with an address.
    pub wifi: Option<Wifi>,
    /// The number of MQTT sessions since `Start` that failed while the
    /// device was awake. Refer to
    /// [`connection::State::reconnects`].
    pub reconnects: u32,
    /// The facts about the start of the chip.
    pub boot: Boot,
    /// What woke the device last.
    pub wake: WakeReason,
}

/// The Wi-Fi facts of a link in the state message. Refer to
/// [`Linked`](crate::features::wifi::Linked).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wifi {
    /// The BSSID of the access point.
    pub bssid: [u8; 6],
    /// The IPv4 address.
    pub ip: [u8; 4],
    /// The last signal of the access point, in dBm, if the shell could read
    /// it.
    pub rssi: Option<i32>,
}

impl Snapshot {
    /// Returns the snapshot of the model now, or `None` before `Start`.
    #[must_use]
    pub fn of(model: &Model) -> Option<Self> {
        let boot = model.startup.boot()?;
        let wifi = model.wifi.phase.linked().map(|linked| Wifi {
            bssid: linked.access_point.bssid,
            ip: linked.ip,
            rssi: linked.rssi,
        });
        Some(Self {
            battery: model.power.battery,
            stand: model.power.source() == PowerSource::Stand,
            reading: model.power.reading,
            wifi,
            reconnects: model.connection.reconnects,
            boot,
            wake: model.power.wake(),
        })
    }

    /// Returns `true` if the change from the snapshot `sent` to this one
    /// goes at once. Refer to the [module docs](self).
    ///
    /// The signal of the access point and the count of reconnects are not
    /// urgent. A change of the access point, of the address, of the boot
    /// facts or of the wake reason is urgent.
    #[must_use]
    pub fn is_urgent_after(&self, sent: &Self) -> bool {
        let battery = match (sent.battery, self.battery) {
            (Some(before), Some(now)) => before.abs_diff(now) > BATTERY_FLAP,
            (before, now) => before != now,
        };
        let address = |snapshot: &Self| snapshot.wifi.map(|wifi| (wifi.bssid, wifi.ip));
        battery
            || self.stand != sent.stand
            || self.charging() != sent.charging()
            || address(self) != address(sent)
            || self.boot != sent.boot
            || self.wake != sent.wake
    }

    /// Returns `true` if the charger charges the battery, or `None` if the
    /// shell did not read the charger.
    fn charging(&self) -> Option<bool> {
        self.reading.charge.map(Charge::is_charging)
    }

    /// Returns the payload of the state message, for example
    /// `{"battery":95,"stand":"OFF","charging":"OFF","charge":"not_charging",`
    /// `"voltage":3870,"current":-40,"temperature":24.5,"rssi":-62,`
    /// `"bssid":"24:5a:4c:01:02:03","ip":"192.168.1.50","reconnects":0,`
    /// `"restarts":0,"reset":"power_on","wake":"boot"}`. An unknown value is
    /// `null`.
    #[must_use]
    pub(crate) fn to_json(&self) -> Vec<u8> {
        let on_off = |on| if on { "ON" } else { "OFF" };
        let reading = &self.reading;
        let payload = StatePayload {
            battery: self.battery,
            stand: on_off(self.stand),
            charging: self.charging().map(on_off),
            charge: reading.charge.map(charge_option),
            voltage: reading.voltage_mv,
            current: reading.current_ma,
            temperature: reading.tenth_celsius.map(|tenths| f64::from(tenths) / 10.0),
            rssi: self.wifi.and_then(|wifi| wifi.rssi),
            bssid: self.wifi.map(|wifi| mac_text(wifi.bssid)),
            ip: self.wifi.map(|wifi| ip_text(wifi.ip)),
            reconnects: self.reconnects,
            restarts: self.boot.restarts,
            reset: reset_option(self.boot.reset),
            wake: wake_option(self.wake),
        };
        // The fields are numbers and text, and the writer is a `Vec`, so
        // the serialization cannot fail.
        serde_json::to_vec(&payload).expect("a payload of numbers and text serializes")
    }
}

/// The options of the enum sensor of the phase of the charge, in the order
/// of [`Charge`].
pub(crate) const CHARGE_OPTIONS: [&str; 4] =
    ["not_charging", "pre_charging", "fast_charging", "done"];

/// Returns the option of the enum sensor for the phase `charge`.
const fn charge_option(charge: Charge) -> &'static str {
    match charge {
        Charge::NotCharging => CHARGE_OPTIONS[0],
        Charge::PreCharging => CHARGE_OPTIONS[1],
        Charge::FastCharging => CHARGE_OPTIONS[2],
        Charge::Done => CHARGE_OPTIONS[3],
    }
}

/// The options of the enum sensor of the last reset, in the order of
/// [`ResetReason`].
pub(crate) const RESET_OPTIONS: [&str; 6] = [
    "power_on", "software", "panic", "watchdog", "brownout", "other",
];

/// Returns the option of the enum sensor for the reset `reset`.
const fn reset_option(reset: ResetReason) -> &'static str {
    match reset {
        ResetReason::PowerOn => RESET_OPTIONS[0],
        ResetReason::Software => RESET_OPTIONS[1],
        ResetReason::Panic => RESET_OPTIONS[2],
        ResetReason::Watchdog => RESET_OPTIONS[3],
        ResetReason::Brownout => RESET_OPTIONS[4],
        ResetReason::Other => RESET_OPTIONS[5],
    }
}

/// The options of the enum sensor of the wake reason, in the order of
/// [`WakeReason`].
pub(crate) const WAKE_OPTIONS: [&str; 3] = ["boot", "touch", "stand"];

/// Returns the option of the enum sensor for the wake reason `wake`.
const fn wake_option(wake: WakeReason) -> &'static str {
    match wake {
        WakeReason::Boot => WAKE_OPTIONS[0],
        WakeReason::Touch => WAKE_OPTIONS[1],
        WakeReason::Stand => WAKE_OPTIONS[2],
    }
}

/// The JSON payload of the state message. Refer to `docs/mqtt.md`.
#[derive(Serialize)]
struct StatePayload {
    battery: Option<u8>,
    /// `ON` or `OFF`, the default payloads of a binary sensor.
    stand: &'static str,
    /// `ON` or `OFF`.
    charging: Option<&'static str>,
    /// One of [`CHARGE_OPTIONS`].
    charge: Option<&'static str>,
    /// In mV.
    voltage: Option<u16>,
    /// In mA.
    current: Option<i16>,
    /// In degrees Celsius. The shell rounds the temperature to 0.5 °C, so
    /// the value is exact as a binary fraction, and it has one decimal in
    /// the JSON.
    temperature: Option<f64>,
    /// In dBm.
    rssi: Option<i32>,
    /// Lowercase hex with colons.
    bssid: Option<String>,
    /// Four decimal numbers with dots.
    ip: Option<String>,
    reconnects: u32,
    restarts: u32,
    /// One of [`RESET_OPTIONS`].
    reset: &'static str,
    /// One of [`WAKE_OPTIONS`].
    wake: &'static str,
}

/// A message that a publisher offers to the queue, and what the publisher
/// keeps for it until the session takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    /// The discovery message.
    Discovery,
    /// The state message of the light at this level (`None` is OFF).
    Light(Option<Level>),
    /// The state message with this snapshot.
    State {
        /// The snapshot in the message.
        snapshot: Snapshot,
        /// The message has a change that is not urgent, so the window
        /// becomes `Throttled`.
        throttles: bool,
    },
    /// A message of the log.
    Log(logs::Item),
}

/// The answer of a publisher to the queue.
pub(crate) enum Next {
    /// The publisher has no message now.
    Nothing,
    /// The publisher waits for an answer before it has a message, for
    /// example the time for the ages of the lines of the log. The command
    /// asks for the answer.
    Wait(Cmd),
    /// The next message of the publisher.
    Offer(Publish, Item),
}

/// A publisher of device messages, in the order of the queue.
#[derive(Clone, Copy)]
enum Publisher {
    Discovery,
    Light,
    State,
    Log,
}

impl Publisher {
    /// The publishers, in the order in which the queue asks them.
    const ORDER: [Self; 4] = [Self::Discovery, Self::Light, Self::State, Self::Log];

    /// Returns the next message of the publisher.
    fn next(self, model: &mut Model) -> Next {
        match self {
            Self::Discovery => next_discovery(model),
            Self::Light => light::next(model),
            Self::State => next_state(model),
            Self::Log => logs::next(model),
        }
    }
}

/// Makes the discovery message due at `Start`.
pub const fn start(model: &mut Model) {
    model.device.discovery = Discovery::Due;
}

/// Handles a message on [`BIRTH_TOPIC`] or on a `set` topic of the device,
/// and returns its command. Returns `None` for a message on another topic,
/// which goes to the entities.
///
/// A birth message that is not retained makes a sent discovery message due
/// again. It does the same for the state of the light and for the state
/// message of the session. [`connection::pump`] then publishes them,
/// because the core routes each message inside the pump.
///
/// A button message gives [`Event::Pressed`]. A live light command gives
/// [`light::Event::Commanded`], and a tap level gives
/// [`light::Event::TapLevelSet`]. Refer to the [module docs](self) for the
/// messages that the core ignores.
pub(crate) fn route(message: &Message, model: &mut Model) -> Option<Cmd> {
    if message.topic == BIRTH_TOPIC {
        let birth = !message.retain && message.payload == BIRTH_ONLINE.as_bytes();
        if birth {
            if model.device.discovery == Discovery::Sent {
                model.device.discovery = Discovery::Due;
            }
            if let Some(live) = model.connection.live_mut() {
                live.light.again();
                live.state.again();
            }
        }
        return Some(Cmd::done());
    }
    let settings = model.startup.settings()?;
    if message.topic == settings.set_topic(BUTTON_SET) {
        return Some(button_message(message));
    }
    if message.topic == settings.set_topic(LIGHT_SET) {
        return Some(light_message(message));
    }
    for (name, source) in [
        (TAP_LEVEL_STAND_SET, PowerSource::Stand),
        (TAP_LEVEL_BATTERY_SET, PowerSource::Battery),
    ] {
        if message.topic == settings.set_topic(name) {
            return Some(tap_level_message(source, message));
        }
    }
    // Later `set` topics come here. The core ignores them for now.
    message
        .topic
        .starts_with(&settings.set_topic(""))
        .then(Cmd::done)
}

/// Handles a message on the button topic.
fn button_message(message: &Message) -> Cmd {
    if message.retain {
        return logs::info(
            Area::Device,
            format!(
                "the button message on {} is retained, so the core ignored it",
                message.topic
            ),
        );
    }
    Button::of(&message.payload).map_or_else(
        || {
            logs::warn(
                Area::Device,
                format!(
                    "the button payload {} is not known, so the core ignored it",
                    shown(&message.payload)
                ),
            )
        },
        |button| Cmd::event(Event::Pressed(button).into()),
    )
}

/// Handles a message on the light topic. The core ignores a retained
/// command, because it is not from now. Refer to [`light`].
fn light_message(message: &Message) -> Cmd {
    if message.retain {
        return logs::info(
            Area::Device,
            format!(
                "the light command on {} is retained, so the core ignored it",
                message.topic
            ),
        );
    }
    light::command(&message.payload).map_or_else(
        || {
            logs::warn(
                Area::Device,
                format!(
                    "the light command {} is not valid, so the core ignored it",
                    shown(&message.payload)
                ),
            )
        },
        |command| Cmd::event(light::Event::Commanded(command).into()),
    )
}

/// Handles a message on the tap level topic of `source`, retained or live.
fn tap_level_message(source: PowerSource, message: &Message) -> Cmd {
    light::tap_level(&message.payload).map_or_else(
        || {
            logs::warn(
                Area::Device,
                format!(
                    "the tap level {} on {} is not valid, so the core ignored it",
                    shown(&message.payload),
                    message.topic
                ),
            )
        },
        |level| Cmd::event(light::Event::TapLevelSet { source, level }.into()),
    )
}

/// Returns `payload` as text for the log. The core keeps at most
/// [`PAYLOAD_IN_LOG`] bytes of the payload, and then escapes each control
/// character, quote and backslash, as Rust does for a string in `{:?}`.
/// Thus a payload cannot add a line to the log, and it cannot send a
/// terminal code.
fn shown(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut end = text.len().min(PAYLOAD_IN_LOG);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].escape_debug().to_string()
}

/// Home Assistant pressed `button`.
///
/// | Button | The device | What the core does | Line |
/// | --- | --- | --- | --- |
/// | Sleep now | a restart waits | nothing | Debug |
/// | Sleep now | awake on battery | [`power::sleep_now`]: the sleep starts | Debug |
/// | Sleep now | awake on battery, open pending marks | [`power::sleep_now`]: the sleep waits for the marks | Debug |
/// | Sleep now | awake on battery, calls in the outbox | [`power::sleep_now`]: the sleep waits one more idle period | Debug |
/// | Sleep now | awake on battery, a sleep waits for the marks | nothing | Debug |
/// | Sleep now | asleep (the first steps of a sleep, or a report) | nothing | none |
/// | Sleep now | on the stand | nothing | Info |
/// | Restart | any | [`connection::close_for_restart`] | none in the core |
/// | Full refresh | a restart waits | nothing | Debug |
/// | Full refresh | awake | [`redraw`] with a full refresh | Debug |
/// | Full refresh | asleep, also in a report | nothing, because the last image of the sleep is a full refresh | Debug |
///
/// For a restart, the shell writes the warning
/// `device: restart the chip: Home Assistant asked`.
fn pressed(button: Button, model: &mut Model) -> Cmd {
    match button {
        Button::Sleep => sleep_pressed(model),
        Button::Restart => connection::close_for_restart(model),
        Button::Refresh => {
            if model.connection.restart_waits() {
                // The reset can stop the refresh with the panel half
                // drawn, so `redraw` does nothing.
                logs::debug(
                    Area::Device,
                    "Home Assistant asked for a full refresh, but the chip restarts, so the core \
                     ignored it",
                )
            } else if model.power.is_asleep() {
                logs::debug(
                    Area::Device,
                    "the device sleeps, so it skips the full refresh",
                )
            } else {
                logs::debug(Area::Device, "Home Assistant asked for a full refresh")
                    .and(redraw(model, true))
            }
        }
    }
}

/// Home Assistant pressed Sleep now. The line tells what the core did.
fn sleep_pressed(model: &mut Model) -> Cmd {
    // A restart wins over a sleep. `sleep_now` would also stop the idle
    // timer, so the core does not call it.
    if model.connection.restart_waits() {
        return logs::debug(
            Area::Device,
            "Home Assistant asked to sleep, but the chip restarts, so the core ignored it",
        );
    }
    let (outcome, sleep) = power::sleep_now(model);
    let line = match outcome {
        IdleEnd::Sleeps => logs::debug(
            Area::Device,
            "Home Assistant asked to sleep, so the sleep starts",
        ),
        IdleEnd::WaitsForMarks => logs::debug(
            Area::Device,
            "Home Assistant asked to sleep, so the sleep waits for the pending marks",
        ),
        IdleEnd::WaitsForOutbox => logs::debug(
            Area::Device,
            "Home Assistant asked to sleep, but service calls wait in the outbox, so the sleep \
             waits one more idle period",
        ),
        IdleEnd::Nothing if model.power.source() == PowerSource::Stand => logs::info(
            Area::Device,
            "Home Assistant asked to sleep, but the device is on the stand, so it stays awake",
        ),
        IdleEnd::Nothing if model.power.sleep_waits() => logs::debug(
            Area::Device,
            "Home Assistant asked to sleep, and the sleep already waits",
        ),
        // The device already sleeps.
        IdleEnd::Nothing => Cmd::done(),
    };
    line.and(sleep)
}

/// A value in the state message can have changed: publish the state, if
/// the connection is up.
///
/// The queue publishes only a snapshot that is not equal to the last one,
/// and a change that is not urgent only when the window is open. Refer to
/// the [module docs](self).
pub fn changed(model: &mut Model) -> Cmd {
    if model.connection.link() == Link::Up {
        connection::pump(model)
    } else {
        Cmd::done()
    }
}

/// A read of the signal ended ([`wifi::read_signal`]), or a scan gave a new
/// signal. An open window becomes `Fresh`, and the queue publishes the
/// change that waits. Refer to the [module docs](self).
pub(crate) fn signal_read(model: &mut Model) -> Cmd {
    if let Some(window) = model
        .connection
        .live_mut()
        .and_then(|live| live.state.window_mut())
        && matches!(window, Window::Open)
    {
        *window = Window::Fresh;
    }
    changed(model)
}

/// The timer of the window with the id `id` expired. If it is the timer of
/// the window now, the window opens, and the queue publishes the snapshot
/// if it changed. The core ignores the expiry of an older timer.
fn window_ended(id: u32, model: &mut Model) -> Cmd {
    let Some(window) = model
        .connection
        .live_mut()
        .and_then(|live| live.state.window_mut())
    else {
        return Cmd::done();
    };
    if !matches!(window, Window::Throttled(timer) if timer.id() == id) {
        return Cmd::done();
    }
    // The timer completed, so it needs no clear.
    *window = Window::Open;
    changed(model)
}

/// The session of the connection ended. A discovery message in flight
/// becomes due again, because its packet id ends with the session.
pub(crate) const fn session_ended(model: &mut Model) {
    if let Discovery::InFlight(_) = model.device.discovery {
        model.device.discovery = Discovery::Due;
    }
}

/// The queue of the device publishes: publishes the next message of each
/// publisher while the session takes them. Returns the commands of the
/// publishers that wait and of the timers that start, and `true` if the
/// session took a publish. Refer to
/// the [module docs](self).
///
/// [`connection::pump`] calls this function after the outbox. The session
/// then sends the publishes in the next write. While the bytes of the
/// write of the CONNACK wait in the session, the queue publishes nothing,
/// so no device publish is in that write. Refer to
/// [`Live::connack_write_waits`](connection::Live::connack_write_waits).
pub(crate) fn drain(model: &mut Model) -> (Cmd, bool) {
    let mut waits = Vec::new();
    let mut published = false;
    let connack_write_waits = model
        .connection
        .live_mut()
        .is_some_and(|live| live.connack_write_waits());
    if model.connection.link() != Link::Up || !model.wants_online() || connack_write_waits {
        return (Cmd::done(), false);
    }
    for publisher in Publisher::ORDER {
        loop {
            if in_flight(model) >= IN_FLIGHT {
                return (Cmd::all(waits), published);
            }
            let (publish, item) = match publisher.next(model) {
                Next::Nothing => break,
                Next::Wait(command) => {
                    waits.push(command);
                    break;
                }
                Next::Offer(publish, item) => (publish, item),
            };
            let Some(live) = model.connection.live_mut() else {
                return (Cmd::all(waits), published);
            };
            match live.session.publish(publish) {
                Ok(Some(packet)) => {
                    waits.push(taken(item, packet, model));
                    published = true;
                }
                Err(Refused::TooLong) => never(item, model),
                // A publish at `QoS` 1 always has a packet id. The queue
                // continues after the next `Sent` or `Acked`.
                Ok(None) | Err(Refused::Full | Refused::NotConnected) => {
                    return (Cmd::all(waits), published);
                }
            }
        }
    }
    (Cmd::all(waits), published)
}

/// The broker acknowledged the publish with the packet id `id`. The
/// publisher that owns the id handles it. The core ignores an id that no
/// publisher owns, for example the id of a service call.
pub(crate) fn acked(id: PacketId, model: &mut Model) {
    if model.device.discovery == Discovery::InFlight(id) {
        model.device.discovery = Discovery::Sent;
        return;
    }
    if let Some(live) = model.connection.live_mut()
        && matches!(live.state, StateSync::InFlight { packet, .. } if packet == id)
    {
        if let StateSync::InFlight {
            sent,
            window,
            again,
            ..
        } = mem::take(&mut live.state)
        {
            live.state = StateSync::Sent { sent, window };
            if again {
                live.state.again();
            }
        }
        return;
    }
    if light::acked(id, model) {
        return;
    }
    logs::acked(id, model);
}

/// Returns `true` if the device publish queue has nothing more to publish
/// on this session, and no device publish waits for a PUBACK. A report
/// then can end. Refer to [`power::report`](crate::features::power).
///
/// - Discovery: the broker acknowledged the message, or the message is not
///   due on this device (a dry run, or no firmware version yet).
/// - The light: the broker acknowledged its state. In a dry run, the queue
///   publishes no state of the light.
/// - The state message: the broker acknowledged it. A change that waits
///   for the throttle window counts as settled, because the report does
///   not wait for the window. In a dry run, the queue publishes no state.
/// - The log: no line and no drop count waits, and no `LogClock` request
///   waits.
///
/// The caller checks the connection: the session is up, and the write of
/// the CONNACK went.
pub(crate) fn settled(model: &mut Model) -> bool {
    let read_only = model
        .startup
        .settings()
        .is_none_or(|settings| settings.read_only);
    // With no snapshot (before `Start`), the queue publishes no state.
    let no_state = read_only || Snapshot::of(model).is_none();
    let discovery = match model.device.discovery {
        Discovery::Sent => true,
        Discovery::InFlight(_) => false,
        Discovery::Due => read_only || model.startup.firmware().is_none(),
    };
    if !discovery || in_flight(model) > 0 || !logs::settled(model) {
        return false;
    }
    let Some(live) = model.connection.live_mut() else {
        return false;
    };
    let light = read_only || matches!(live.light, light::Sync::Sent(_));
    let state = no_state || matches!(live.state, StateSync::Sent { .. });
    light && state
}

/// Returns the number of device publishes that wait for a PUBACK.
fn in_flight(model: &mut Model) -> usize {
    let discovery = usize::from(matches!(model.device.discovery, Discovery::InFlight(_)));
    let state = model.connection.live_mut().map_or(0, |live| {
        usize::from(matches!(live.state, StateSync::InFlight { .. }))
    });
    discovery + light::in_flight(model) + state + logs::in_flight(model)
}

/// The session took the message `item` with the packet id `packet`.
/// Returns the command of the timer of the window, if the message makes
/// the window `Throttled`.
fn taken(item: Item, packet: PacketId, model: &mut Model) -> Cmd {
    match item {
        Item::Discovery => model.device.discovery = Discovery::InFlight(packet),
        Item::Light(sent) => light::taken(sent, packet, model),
        Item::State {
            snapshot,
            throttles,
        } => {
            model.device.window = model.device.window.wrapping_add(1);
            let id = model.device.window;
            let Some(live) = model.connection.live_mut() else {
                return Cmd::done();
            };
            let mut window = live.state.take_window();
            let timer = if throttles {
                let (command, running) =
                    Running::start(id, STATE_MIN_INTERVAL, Event::WindowEnded(id).into());
                // A change that is not urgent goes only while the window is
                // open, so no old timer runs. This is a guard.
                if let Window::Throttled(old) =
                    mem::replace(&mut window, Window::Throttled(running))
                {
                    old.clear();
                }
                command
            } else {
                Cmd::done()
            };
            live.state = StateSync::InFlight {
                packet,
                sent: snapshot,
                window,
                again: false,
            };
            return timer;
        }
        Item::Log(item) => logs::taken(item, packet, model),
    }
    Cmd::done()
}

/// The session can never take the message `item`, because it is too long
/// for a packet.
///
/// The discovery, the light and the state messages are short, so this does
/// not occur for them. If it occurs, the core does not offer the message
/// again, so the queue cannot repeat it without end.
fn never(item: Item, model: &mut Model) {
    match item {
        Item::Discovery => model.device.discovery = Discovery::Sent,
        Item::Light(sent) => light::never(sent, model),
        Item::State { snapshot, .. } => {
            if let Some(live) = model.connection.live_mut() {
                let window = live.state.take_window();
                live.state = StateSync::Sent {
                    sent: snapshot,
                    window,
                };
            }
        }
        Item::Log(item) => logs::never(item, model),
    }
}

/// Returns the discovery message if it is due. During a dry run, returns
/// nothing.
fn next_discovery(model: &Model) -> Next {
    if model.device.discovery != Discovery::Due {
        return Next::Nothing;
    }
    let (Some(settings), Some(firmware)) = (model.startup.settings(), model.startup.firmware())
    else {
        return Next::Nothing;
    };
    if settings.read_only {
        return Next::Nothing;
    }
    let publish = Publish {
        topic: discovery::topic(settings),
        payload: discovery::payload(settings, firmware),
        qos: Qos::AtLeastOnce,
        retain: true,
    };
    Next::Offer(publish, Item::Discovery)
}

/// During a dry run, returns nothing. Else returns the state message if
/// the session did not publish it, or if the snapshot changed since the
/// last state message and the change is due. An urgent change is always
/// due. A change that is not urgent is due only while the window is open.
/// While a state message waits for its PUBACK, returns nothing.
///
/// In an `Open` window, a change that is not urgent waits for a read of the
/// signal first, if the core can read it. If the snapshot did not change in
/// a `Fresh` window, the window is `Open` again, so the next change reads
/// the signal again.
fn next_state(model: &mut Model) -> Next {
    let Some(snapshot) = Snapshot::of(model) else {
        return Next::Nothing;
    };
    let Some(topic) = model
        .startup
        .settings()
        .filter(|settings| !settings.read_only)
        .map(Settings::state_topic)
    else {
        return Next::Nothing;
    };
    let Some(live) = model.connection.live_mut() else {
        return Next::Nothing;
    };
    let throttles = match &mut live.state {
        StateSync::Unsent => false,
        StateSync::InFlight { .. } => return Next::Nothing,
        StateSync::Sent { sent, window } => {
            if *sent == snapshot {
                if matches!(window, Window::Fresh) {
                    *window = Window::Open;
                }
                return Next::Nothing;
            }
            if snapshot.is_urgent_after(sent) {
                false
            } else {
                match window {
                    Window::Throttled(_) => return Next::Nothing,
                    Window::Open => match wifi::read_signal(model) {
                        Some(read) => return Next::Wait(read),
                        None => true,
                    },
                    Window::Fresh => true,
                }
            }
        }
    };
    let publish = Publish {
        topic,
        payload: snapshot.to_json(),
        qos: Qos::AtLeastOnce,
        retain: true,
    };
    Next::Offer(
        publish,
        Item::State {
            snapshot,
            throttles,
        },
    )
}

#[cfg(test)]
mod tests;
