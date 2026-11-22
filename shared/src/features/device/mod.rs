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
//! The state message has the values of the sensors as one JSON object, for
//! example `{"battery":95,"stand":"OFF"}`. It is retained. Refer to
//! [`Snapshot`].
//!
//! | `StateSync` | Event | Next | What the core does |
//! | --- | --- | --- | --- |
//! | (none) | a new session | `Unsent` | the queue publishes the snapshot |
//! | `Unsent` | the session takes it | `InFlight` | nothing |
//! | `InFlight` | a value changes | `InFlight` | nothing yet |
//! | `InFlight` | its PUBACK | `Sent` | the queue publishes again if the snapshot changed |
//! | `Sent` | a value changes | `Sent` | the queue publishes the new snapshot |
//! | any | the session ends | (none) | nothing |
//!
//! - The state goes once on each connection, and then each time that a
//!   value in it changes ([`changed`]). At most one state message waits for
//!   a PUBACK.
//! - While the device sleeps, the queue publishes nothing. The next
//!   connection publishes the state.

mod discovery;

use alloc::vec::Vec;
use core::mem;

use serde::Serialize;

use crate::{
    Cmd, Model, PowerSource,
    features::{
        connection::{self, Link},
        logs,
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

/// The state of the device feature.
#[derive(Debug, Default)]
pub struct State {
    /// The publish of the discovery message.
    discovery: Discovery,
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
/// session starts with `Unsent`.
#[derive(Debug, Default, PartialEq, Eq)]
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
    },
    /// The broker acknowledged the state message with this snapshot.
    Sent(Snapshot),
}

/// The values of the sensors in the state message.
///
/// The core compares the snapshot of the model with the last snapshot that
/// it sent. If they are not equal, it publishes the new snapshot. To add a
/// sensor, add a field here and a key to the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The charge of the battery in percent, or `None` if the shell did not
    /// read it.
    pub battery: Option<u8>,
    /// The device is on the stand.
    pub stand: bool,
}

impl Snapshot {
    /// Returns the snapshot of the model now.
    #[must_use]
    pub fn of(model: &Model) -> Self {
        Self {
            battery: model.power.battery,
            stand: model.power.source() == PowerSource::Stand,
        }
    }

    /// Returns the payload of the state message, for example
    /// `{"battery":95,"stand":"OFF"}`. An unknown battery is `null`.
    #[must_use]
    pub(crate) fn to_json(&self) -> Vec<u8> {
        let payload = StatePayload {
            battery: self.battery,
            stand: if self.stand { "ON" } else { "OFF" },
        };
        // The fields are a number and text, and the writer is a `Vec`, so
        // the serialization cannot fail.
        serde_json::to_vec(&payload).expect("a payload of a number and text serializes")
    }
}

/// The JSON payload of the state message. Refer to `docs/mqtt.md`.
#[derive(Serialize)]
struct StatePayload {
    battery: Option<u8>,
    /// `ON` or `OFF`, the default payloads of a binary sensor.
    stand: &'static str,
}

/// A message that a publisher offers to the queue, and what the publisher
/// keeps for it until the session takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    /// The discovery message.
    Discovery,
    /// The state message with this snapshot.
    State(Snapshot),
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
/// the connection is up. The queue publishes only a snapshot that is not
/// equal to the last one.
pub fn changed(model: &mut Model) -> Cmd {
    if model.connection.link() == Link::Up {
        connection::pump(model)
    } else {
        Cmd::done()
    }
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
/// publishers that wait, and `true` if the session took a publish. Refer to
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
                    taken(item, packet, model);
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
        if let StateSync::InFlight { sent, .. } = mem::take(&mut live.state) {
            live.state = StateSync::Sent(sent);
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
fn taken(item: Item, packet: PacketId, model: &mut Model) {
    match item {
        Item::Discovery => model.device.discovery = Discovery::InFlight(packet),
        Item::State(sent) => {
            if let Some(live) = model.connection.live_mut() {
                live.state = StateSync::InFlight { packet, sent };
            }
        }
        Item::Log(item) => logs::taken(item, packet, model),
    }
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
        Item::State(sent) => {
            if let Some(live) = model.connection.live_mut() {
                live.state = StateSync::Sent(sent);
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
/// the session did not publish it, or if the
/// snapshot changed since the last state message. While a state message
/// waits for its PUBACK, returns nothing.
fn next_state(model: &mut Model) -> Next {
    let snapshot = Snapshot::of(model);
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
    let due = match &live.state {
        StateSync::Unsent => true,
        StateSync::InFlight { .. } => false,
        StateSync::Sent(sent) => *sent != snapshot,
    };
    if !due {
        return Next::Nothing;
    }
    let publish = Publish {
        topic: settings.state_topic(),
        payload: snapshot.to_json(),
        qos: Qos::AtLeastOnce,
        retain: true,
    };
    Next::Offer(publish, Item::State(snapshot))
}

#[cfg(test)]
mod tests;
