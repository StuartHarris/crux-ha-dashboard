//! The device feature: the dashboard as a device of Home Assistant, and the
//! queue of the device publishes.
//!
//! # The device publish queue
//!
//! The device publishes are the messages that the dashboard sends about
//! itself: the discovery message, the state of its sensors and the lines of
//! its log. Each one is a publisher. [`drain`] is the queue.
//! [`connection::pump`] calls it after the outbox, while the connection is
//! up.
//!
//! The queue asks each publisher for its next message, in this order:
//! discovery, state, log. It gives each message to the session, and tells
//! the publisher the packet id. It stops when one of these is true:
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
//! of a sleep. The connection can still be up in the first
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
//!
//! # Dry run
//!
//! During a dry run ([`Settings::read_only`](crate::Settings::read_only)),
//! the core publishes no discovery message and no state message. The
//! bring-up binary of the firmware uses a dry run, so it does not appear in
//! Home Assistant as a device. The log still goes. When the dry run stops,
//! the queue publishes the discovery message (if it is due) and the state.
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
//! access point. Refer to [`wifi`](crate::features::wifi).
//!
//! The [`Window`] is the throttle. While it is `Open`, a change that is
//! not urgent goes at once, and the core starts a timer of
//! [`STATE_MIN_INTERVAL`]. The window is then `Throttled`, and each change
//! that is not urgent waits. When the timer expires, the window is `Open`
//! again. The snapshot of that time then goes, if it is not equal to the
//! last one, and the core starts a new timer. An urgent change goes at
//! once also while the window is `Throttled`, with the values that wait,
//! and the timer continues.
//!
//! | `StateSync` | Event | Next | What the core does |
//! | --- | --- | --- | --- |
//! | (none) | a new session | `Unsent` | the queue publishes the snapshot |
//! | `Unsent` | the session takes it | `InFlight`, `Open` | nothing |
//! | `InFlight` | a value changes | `InFlight` | nothing yet |
//! | `InFlight` | its PUBACK | `Sent` | the queue publishes if the snapshot is due |
//! | `Sent` | an urgent change | `InFlight` | the queue publishes it, and the window does not change |
//! | `Sent`, `Open` | a change that is not urgent | `InFlight`, `Throttled` | the queue publishes it, and the core starts the timer |
//! | `Sent`, `Throttled` | a change that is not urgent | `Sent`, `Throttled` | nothing yet |
//! | `Throttled` | the timer expires | `Open` | the queue publishes if the snapshot is due |
//! | any | the session ends | (none) | the core clears the timer |
//!
//! - At most one state message waits for a PUBACK.
//! - While the device sleeps, the queue publishes nothing. The next
//!   connection publishes the state.
//! - The timer ends with the session. Each sleep closes the session before
//!   its first `Sleep` request, so no timer of the throttle waits in a
//!   sleep. A timer of the core cannot wake the chip, and the next
//!   connection publishes the state at once.

mod discovery;

use alloc::{string::String, vec::Vec};
use core::{mem, time::Duration};

use serde::Serialize;

use crate::{
    Boot, Cmd, Model, PowerSource, ResetReason,
    capabilities::{
        power::{Charge, Reading},
        radio::{ip_text, mac_text},
    },
    features::{
        connection::{self, Link},
        logs,
        power::WakeReason,
        timer::Running,
    },
    mqtt::{MAX_IN_FLIGHT, Message, PacketId, Publish, Qos, Refused},
};

/// The number of places for publishes at `QoS` 1 that the device publishes
/// leave free in each session.
///
/// The session takes at most [`MAX_IN_FLIGHT`] publishes that wait for a
/// PUBACK. If the device publishes took all of them, a sleep could not
/// publish `asleep` (the core drops a refused `asleep`), and a service call
/// would wait for a PUBACK of the log. With these places free, the device
/// publishes never block the status or a few taps.
pub const RESERVED: usize = 8;

/// The largest number of device publishes that wait for a PUBACK in a
/// session: the discovery message, the state message and the messages of
/// the log, all together.
pub const IN_FLIGHT: usize = MAX_IN_FLIGHT - RESERVED;

/// The topic of the birth message of Home Assistant.
pub const BIRTH_TOPIC: &str = "homeassistant/status";

/// The payload of the birth message when Home Assistant starts.
pub const BIRTH_ONLINE: &str = "online";

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
    /// The next change that is not urgent goes at once.
    #[default]
    Open,
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
    State,
    Log,
}

impl Publisher {
    /// The publishers, in the order in which the queue asks them.
    const ORDER: [Self; 3] = [Self::Discovery, Self::State, Self::Log];

    /// Returns the next message of the publisher.
    fn next(self, model: &mut Model) -> Next {
        match self {
            Self::Discovery => next_discovery(model),
            Self::State => next_state(model),
            Self::Log => logs::next(model),
        }
    }
}

/// Makes the discovery message due at `Start`.
pub const fn start(model: &mut Model) {
    model.device.discovery = Discovery::Due;
}

/// Handles a message on [`BIRTH_TOPIC`], and returns `true`. Returns
/// `false` for a message on another topic.
///
/// A birth message that is not retained makes a sent discovery message due
/// again. [`connection::pump`] then publishes it, because the core routes
/// each message inside the pump.
pub(crate) fn route(message: &Message, model: &mut Model) -> bool {
    if message.topic != BIRTH_TOPIC {
        return false;
    }
    let birth = !message.retain && message.payload == BIRTH_ONLINE.as_bytes();
    if birth && model.device.discovery == Discovery::Sent {
        model.device.discovery = Discovery::Due;
    }
    true
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
    if model.connection.link() != Link::Up || model.power.is_asleep() || connack_write_waits {
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
        if let StateSync::InFlight { sent, window, .. } = mem::take(&mut live.state) {
            live.state = StateSync::Sent { sent, window };
        }
        return;
    }
    logs::acked(id, model);
}

/// Returns the number of device publishes that wait for a PUBACK.
fn in_flight(model: &mut Model) -> usize {
    let discovery = usize::from(matches!(model.device.discovery, Discovery::InFlight(_)));
    let state = model.connection.live_mut().map_or(0, |live| {
        usize::from(matches!(live.state, StateSync::InFlight { .. }))
    });
    discovery + state + logs::in_flight(model)
}

/// The session took the message `item` with the packet id `packet`.
/// Returns the command of the timer of the window, if the message makes
/// the window `Throttled`.
fn taken(item: Item, packet: PacketId, model: &mut Model) -> Cmd {
    match item {
        Item::Discovery => model.device.discovery = Discovery::InFlight(packet),
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
/// The discovery and the state messages are short, so this does not occur
/// for them. If it occurs, the core does not offer the message again, so
/// the queue cannot repeat it without end.
fn never(item: Item, model: &mut Model) {
    match item {
        Item::Discovery => model.device.discovery = Discovery::Sent,
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
fn next_state(model: &mut Model) -> Next {
    let Some(snapshot) = Snapshot::of(model) else {
        return Next::Nothing;
    };
    let Some(settings) = model
        .startup
        .settings()
        .filter(|settings| !settings.read_only)
    else {
        return Next::Nothing;
    };
    let Some(live) = model.connection.live_mut() else {
        return Next::Nothing;
    };
    let throttles = match &live.state {
        StateSync::Unsent => false,
        StateSync::InFlight { .. } => return Next::Nothing,
        StateSync::Sent { sent, window } => {
            if *sent == snapshot {
                return Next::Nothing;
            }
            let urgent = snapshot.is_urgent_after(sent);
            if !urgent && matches!(window, Window::Throttled(_)) {
                return Next::Nothing;
            }
            !urgent
        }
    };
    let publish = Publish {
        topic: settings.state_topic(),
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
