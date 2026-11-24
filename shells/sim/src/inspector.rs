//! The records that the shell keeps for the window: a log of the effects and
//! a log of the MQTT traffic.
//!
//! The shell writes to an [`Inspector`]. The window only reads it, so the
//! tests can examine the logs with no window.
//!
//! The core gives the shell only bytes. A [`Wire`] finds the MQTT packets in
//! a copy of the bytes of a stream, in each direction, and the inspector
//! adds a row to the MQTT log for each packet.
//!
//! Do not put a `Debug` text of an effect, a request, a packet or a `Start`
//! event in a log. These values contain the MQTT password.

use std::collections::VecDeque;

use jiff::ZonedDateTime;
use shared::mqtt::{
    MAX_PACKET,
    codec::{self, Inbound, server},
    framer::{Frame, Framer},
};

/// The largest number of entries in each log. A new entry removes the
/// oldest entry when the log is full.
pub const CAPACITY: usize = 500;
/// The largest number of characters of a payload in the MQTT log.
pub const PAYLOAD_LIMIT: usize = 120;

/// A list of entries, from the newest, with a maximum length.
#[derive(Debug)]
pub struct Log<T> {
    entries: VecDeque<T>,
    capacity: usize,
    dropped: u64,
}

impl<T> Log<T> {
    /// Makes an empty log that keeps at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Adds an entry as the newest. If the log is full, removes the oldest
    /// entry.
    pub fn push(&mut self, entry: T) {
        if self.capacity == 0 {
            self.dropped += 1;
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_back();
            self.dropped += 1;
        }
        self.entries.push_front(entry);
    }

    /// Returns the entries, from the newest.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter()
    }

    /// Returns the number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the number of entries that the log removed because it was
    /// full, since the last clear.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Removes all the entries and sets the dropped count to zero.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.dropped = 0;
    }
}

impl<T: Matches> Log<T> {
    /// Returns the entries that match `filter`, from the newest. An empty
    /// filter matches all entries.
    pub fn filtered<'a>(&'a self, filter: &'a str) -> impl Iterator<Item = &'a T> + 'a {
        let filter = filter.trim().to_lowercase();
        self.iter()
            .filter(move |entry| filter.is_empty() || entry.matches(&filter))
    }
}

/// An entry that a filter text can select.
pub trait Matches {
    /// Returns `true` if the entry contains `filter`, which is in lower
    /// case and is not empty.
    fn matches(&self, filter: &str) -> bool;
}

/// One effect that the core asked for, or one answer to an earlier effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectEntry {
    /// The time of the entry.
    pub time: ZonedDateTime,
    /// The name of the effect, for example `StreamWrite`.
    pub effect: &'static str,
    /// The facts of the request, for example the topic. It never contains
    /// a password.
    pub request: String,
    /// What the shell did, for example `sent` or `resolved: Connected`.
    pub outcome: String,
}

/// The effects that the core asks for many times. The effect log can hide
/// them, so that the other effects stay visible.
pub const ROUTINE_EFFECTS: [&str; 2] = ["Render", "StreamWrite"];

impl EffectEntry {
    /// Returns `true` if the effect is in [`ROUTINE_EFFECTS`].
    #[must_use]
    pub fn is_routine(&self) -> bool {
        ROUTINE_EFFECTS.contains(&self.effect)
    }
}

impl Log<EffectEntry> {
    /// Returns the effects that match `filter`, from the newest. If
    /// `hide_routine` is `true`, the routine effects are not included.
    pub fn shown<'a>(
        &'a self,
        filter: &'a str,
        hide_routine: bool,
    ) -> impl Iterator<Item = &'a EffectEntry> + 'a {
        self.filtered(filter)
            .filter(move |entry| !(hide_routine && entry.is_routine()))
    }
}

impl Matches for EffectEntry {
    fn matches(&self, filter: &str) -> bool {
        [self.effect, &self.request, &self.outcome]
            .iter()
            .any(|text| text.to_lowercase().contains(filter))
    }
}

/// The direction of an MQTT packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the broker to the shell.
    In,
    /// From the shell to the broker.
    Out,
}

/// The kind of an MQTT packet in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    /// A request to connect. The log does not show the credentials.
    Connect,
    /// The answer of the broker to the request to connect.
    ConnAck,
    /// A subscription to one or more topics.
    Subscribe,
    /// The answer of the broker to a subscription.
    SubAck,
    /// A message.
    Publish,
    /// A PINGREQ packet out, or a PINGRESP packet in.
    Ping,
    /// The end of the connection.
    Disconnect,
}

/// One MQTT packet, in or out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttEntry {
    /// The time of the entry.
    pub time: ZonedDateTime,
    /// The direction of the packet.
    pub direction: Direction,
    /// The kind of packet.
    pub packet: Packet,
    /// The topic, or an empty string if the packet has none. For a
    /// subscribe packet, the number of topics, for example `75 topics`.
    pub topic: String,
    /// The topics of a subscribe packet. The list is empty for the other
    /// packets.
    pub topics: Vec<String>,
    /// The payload as text, cut to [`PAYLOAD_LIMIT`] characters. For a
    /// connect packet, the broker and the client id. For a CONNACK or a
    /// SUBACK packet, the result.
    pub payload: String,
    /// The retained flag of a message.
    pub retained: bool,
}

impl Matches for MqttEntry {
    /// The MQTT filter selects by topic only. A subscribe packet matches if
    /// one of its topics matches.
    fn matches(&self, filter: &str) -> bool {
        self.topic.to_lowercase().contains(filter)
            || self
                .topics
                .iter()
                .any(|topic| topic.to_lowercase().contains(filter))
    }
}

/// Returns `payload` as text, cut to [`PAYLOAD_LIMIT`] characters. If the
/// text is longer, it ends with `…`.
#[must_use]
pub fn truncate(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut chars = text.chars();
    let mut short: String = chars.by_ref().take(PAYLOAD_LIMIT).collect();
    if chars.next().is_some() {
        short.push('…');
    }
    short
}

/// The logs of the shell.
#[derive(Debug)]
pub struct Inspector {
    /// The effects and their outcomes, from the newest.
    pub effects: Log<EffectEntry>,
    /// The MQTT packets, from the newest.
    pub mqtt: Log<MqttEntry>,
    /// The number of messages from the broker. A clear of the MQTT log does
    /// not change it.
    pub messages: u64,
    /// The number of subscribes that the broker acknowledged in full: each
    /// SUBSCRIBE packet of the stream has its SUBACK packet. A clear of the
    /// MQTT log does not change it.
    pub subscribed: u64,
}

impl Default for Inspector {
    fn default() -> Self {
        Self {
            effects: Log::new(CAPACITY),
            mqtt: Log::new(CAPACITY),
            messages: 0,
            subscribed: 0,
        }
    }
}

impl Inspector {
    /// Adds an entry to the effect log, with the time now.
    pub fn effect(&mut self, effect: &'static str, request: String, outcome: impl Into<String>) {
        self.effects.push(EffectEntry {
            time: ZonedDateTime::now(),
            effect,
            request,
            outcome: outcome.into(),
        });
    }

    /// Adds an entry to the MQTT log, with the time now.
    pub fn packet(
        &mut self,
        direction: Direction,
        packet: Packet,
        topic: impl Into<String>,
        payload: String,
        retained: bool,
    ) {
        self.mqtt.push(MqttEntry {
            time: ZonedDateTime::now(),
            direction,
            packet,
            topic: topic.into(),
            topics: Vec::new(),
            payload,
            retained,
        });
    }

    /// Adds one entry to the MQTT log for a subscribe packet with `topics`,
    /// with the time now.
    pub fn subscribe(&mut self, topics: Vec<String>) {
        self.mqtt.push(MqttEntry {
            time: ZonedDateTime::now(),
            direction: Direction::Out,
            packet: Packet::Subscribe,
            topic: format!("{} topics", topics.len()),
            topics,
            payload: String::new(),
            retained: false,
        });
    }

    /// Adds an entry to the MQTT log for each packet that the bytes from the
    /// shell complete on `wire`.
    pub fn sent(&mut self, wire: &mut Wire, bytes: &[u8]) {
        let Some(framer) = &mut wire.sent else {
            return;
        };
        framer.push(bytes.to_vec());
        let mut packets = Vec::new();
        loop {
            match framer.next_packet() {
                Ok(Some(Frame::Packet { first, body })) => {
                    let Ok(packet) = server::decode(first, body) else {
                        wire.stop_sent();
                        break;
                    };
                    packets.push(packet);
                }
                Ok(Some(Frame::Oversize { length, .. })) => {
                    self.packet(
                        Direction::Out,
                        Packet::Publish,
                        "",
                        format!("not shown: {length} bytes"),
                        false,
                    );
                }
                Ok(None) => break,
                Err(codec::Malformed) => {
                    wire.stop_sent();
                    break;
                }
            }
        }
        for packet in packets {
            if matches!(packet, server::Packet::Subscribe { .. }) {
                wire.subacks += 1;
            }
            self.sent_packet(&wire.target, packet);
        }
    }

    /// Adds an entry to the MQTT log for a packet from the shell. A CONNECT
    /// packet shows only the target and the client id, never the
    /// credentials.
    fn sent_packet(&mut self, target: &str, packet: server::Packet) {
        match packet {
            server::Packet::Connect { client_id, .. } => self.packet(
                Direction::Out,
                Packet::Connect,
                "",
                format!("{target} as {client_id}"),
                false,
            ),
            server::Packet::Publish {
                flags,
                topic,
                payload,
                ..
            } => self.packet(
                Direction::Out,
                Packet::Publish,
                topic,
                truncate(&payload),
                flags & RETAIN != 0,
            ),
            server::Packet::Subscribe { filters, .. } => {
                self.subscribe(filters.into_iter().map(|(topic, _)| topic).collect());
            }
            server::Packet::PingReq => {
                self.packet(Direction::Out, Packet::Ping, "", String::new(), false);
            }
            server::Packet::Disconnect => {
                self.packet(Direction::Out, Packet::Disconnect, "", String::new(), false);
            }
            server::Packet::PubAck(_) => {}
        }
    }

    /// Adds an entry to the MQTT log for each packet that the bytes from the
    /// broker complete on `wire`.
    pub fn received(&mut self, wire: &mut Wire, bytes: &[u8]) {
        let Some(framer) = &mut wire.received else {
            return;
        };
        framer.push(bytes.to_vec());
        let mut stop = false;
        loop {
            match framer.next_packet() {
                Ok(Some(Frame::Packet { first, body })) => {
                    let Ok(packet) = codec::decode(first, body) else {
                        stop = true;
                        break;
                    };
                    if matches!(packet, Inbound::SubAck { .. }) && wire.subacks > 0 {
                        wire.subacks -= 1;
                        if wire.subacks == 0 {
                            self.subscribed += 1;
                        }
                    }
                    self.received_packet(first, packet);
                }
                Ok(Some(Frame::Oversize { length, .. })) => {
                    self.messages += 1;
                    self.packet(
                        Direction::In,
                        Packet::Publish,
                        "",
                        format!("skipped: {length} bytes"),
                        false,
                    );
                }
                Ok(None) => break,
                Err(codec::Malformed) => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            wire.stop_received();
        }
    }

    /// Adds an entry to the MQTT log for a packet from the broker. `first`
    /// is the first byte of the fixed header.
    fn received_packet(&mut self, first: u8, packet: Inbound<'_>) {
        match packet {
            Inbound::ConnAck { code, .. } => {
                let result = if code == 0 {
                    "accepted".to_string()
                } else {
                    format!("refused: code {code}")
                };
                self.packet(Direction::In, Packet::ConnAck, "", result, false);
            }
            Inbound::Publish { topic, payload, .. } => {
                self.messages += 1;
                self.packet(
                    Direction::In,
                    Packet::Publish,
                    topic,
                    truncate(payload),
                    first & RETAIN != 0,
                );
            }
            Inbound::SubAck { codes, .. } => {
                let refused = codes.iter().fold(0, |count, code| {
                    count + usize::from(*code == codec::SUBACK_FAILURE)
                });
                let result = format!("{} topics, {refused} refused", codes.len());
                self.packet(Direction::In, Packet::SubAck, "", result, false);
            }
            Inbound::PingResp => {
                self.packet(Direction::In, Packet::Ping, "", String::new(), false);
            }
            Inbound::PubAck { .. } => {}
        }
    }
}

/// The retain flag in the first byte of a PUBLISH packet.
const RETAIN: u8 = 0x01;

/// Finds the MQTT packets in the bytes of one stream, in each direction, for
/// the MQTT log.
///
/// If the bytes in a direction are not valid MQTT, the wire does not read
/// that direction again. The core ends the session in that case.
#[derive(Debug)]
pub struct Wire {
    /// The host and the port of the stream, for the CONNECT entry.
    target: String,
    sent: Option<Framer>,
    received: Option<Framer>,
    /// The number of SUBSCRIBE packets that wait for their SUBACK packets.
    subacks: usize,
}

impl Wire {
    /// Makes a wire for a stream to `target`, for example `broker:1883`.
    #[must_use]
    pub const fn new(target: String) -> Self {
        Self {
            target,
            sent: Some(Framer::new(MAX_PACKET)),
            received: Some(Framer::new(MAX_PACKET)),
            subacks: 0,
        }
    }

    fn stop_sent(&mut self) {
        eprintln!("inspector: the bytes to the broker are not valid MQTT");
        self.sent = None;
    }

    fn stop_received(&mut self) {
        eprintln!("inspector: the bytes from the broker are not valid MQTT");
        self.received = None;
    }
}

#[cfg(test)]
mod tests {
    use jiff::ZonedDateTime;
    use shared::mqtt::{
        Connect, Credentials, LastWill, Qos,
        codec::{self, Flags, server},
    };

    use super::{
        Direction, EffectEntry, Inspector, Log, MqttEntry, PAYLOAD_LIMIT, Packet, Wire, truncate,
    };

    fn effect(name: &'static str, request: &str) -> EffectEntry {
        EffectEntry {
            time: ZonedDateTime::now(),
            effect: name,
            request: request.into(),
            outcome: "sent".into(),
        }
    }

    fn message(topic: &str) -> MqttEntry {
        MqttEntry {
            time: ZonedDateTime::now(),
            direction: Direction::In,
            packet: Packet::Publish,
            topic: topic.into(),
            topics: Vec::new(),
            payload: "on".into(),
            retained: true,
        }
    }

    #[test]
    fn a_full_log_drops_the_oldest_entry() {
        let mut log = Log::new(3);
        for number in 1..=5 {
            log.push(number);
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.iter().copied().collect::<Vec<_>>(), [5, 4, 3]);
        log.clear();
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn a_log_with_no_capacity_stays_empty() {
        let mut log = Log::new(0);
        log.push(1);
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn the_effect_filter_ignores_case_and_looks_at_all_the_text() {
        let mut log = Log::new(10);
        log.push(effect("StreamClose", "dashboard/sim/cmd"));
        log.push(effect("Render", ""));
        log.push(effect("RadioJoin", ""));
        let names = |filter| {
            log.filtered(filter)
                .map(|entry| entry.effect)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(""), ["RadioJoin", "Render", "StreamClose"]);
        assert_eq!(names("  RENDER "), ["Render"]);
        assert_eq!(names("cmd"), ["StreamClose"]);
        assert_eq!(names("SENT"), ["RadioJoin", "Render", "StreamClose"]);
        assert_eq!(names("nothing"), Vec::<&str>::new());
    }

    #[test]
    fn the_mqtt_filter_looks_at_the_topic_only() {
        let mut log = Log::new(10);
        log.push(message("homeassistant/statestream/light/kitchen/state"));
        log.push(message("dashboard/sim/status"));
        let topics = |filter| {
            log.filtered(filter)
                .map(|entry| entry.topic.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            topics("Kitchen"),
            ["homeassistant/statestream/light/kitchen/state"]
        );
        // The payload of both messages is `on`, but the filter ignores it.
        assert_eq!(topics("on/"), Vec::<&str>::new());
    }

    #[test]
    fn a_long_payload_is_cut() {
        assert_eq!(truncate(b"on"), "on");
        let exact = "x".repeat(PAYLOAD_LIMIT);
        assert_eq!(truncate(exact.as_bytes()), exact);
        // Multi-byte characters are cut at a character boundary.
        let long = "°".repeat(PAYLOAD_LIMIT + 1);
        let short = truncate(long.as_bytes());
        assert_eq!(short.chars().count(), PAYLOAD_LIMIT + 1);
        assert!(short.ends_with("°…"));
    }

    #[test]
    fn the_log_counts_the_entries_that_it_drops() {
        let mut log = Log::new(2);
        for number in 1..=5 {
            log.push(number);
        }
        assert_eq!(log.dropped(), 3);
        log.clear();
        assert_eq!(log.dropped(), 0);
    }

    #[test]
    fn routine_effects_can_be_hidden() {
        let mut log = Log::new(10);
        log.push(effect("RadioJoin", ""));
        log.push(effect("StreamClose", "dashboard/sim/cmd"));
        log.push(effect("Render", ""));
        let names = |hide| {
            log.shown("", hide)
                .map(|entry| entry.effect)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(true), ["StreamClose", "RadioJoin"]);
        assert_eq!(names(false), ["Render", "StreamClose", "RadioJoin"]);
        assert_eq!(
            log.shown("render", false).count(),
            1,
            "the text filter still applies"
        );
    }

    #[test]
    fn a_subscribe_request_is_one_entry_that_a_topic_filter_finds() {
        let mut inspector = Inspector::default();
        let topics = vec![
            "homeassistant/statestream/light/kitchen/state".to_string(),
            "homeassistant/statestream/light/hall/state".to_string(),
        ];
        inspector.subscribe(topics.clone());
        assert_eq!(inspector.mqtt.len(), 1);
        let entry = inspector.mqtt.iter().next().unwrap();
        assert_eq!(entry.topic, "2 topics");
        assert_eq!(entry.topics, topics);
        assert_eq!(inspector.mqtt.filtered("hall").count(), 1);
        assert_eq!(inspector.mqtt.filtered("garage").count(), 0);
    }

    /// Returns the rows of the MQTT log, from the oldest, as text.
    fn rows(inspector: &Inspector) -> Vec<String> {
        let mut rows: Vec<String> = inspector
            .mqtt
            .iter()
            .map(|entry| {
                format!(
                    "{:?} {:?} {} {}{}",
                    entry.direction,
                    entry.packet,
                    entry.topic,
                    entry.payload,
                    if entry.retained { " retained" } else { "" }
                )
            })
            .collect();
        rows.reverse();
        rows
    }

    #[test]
    fn the_wire_shows_the_packets_out_with_no_credentials() {
        let mut bytes = Vec::new();
        let connect = Connect {
            host: "broker".into(),
            port: 1883,
            client_id: "dashboard-sim".into(),
            credentials: Some(Credentials {
                username: "dashboard".into(),
                password: "secret-password".into(),
            }),
            keep_alive_secs: 60,
            last_will: LastWill {
                topic: "dashboard/sim/status".into(),
                payload: b"offline".to_vec(),
                retain: true,
            },
        };
        codec::connect(&mut bytes, &connect).unwrap();
        let flags = Flags {
            qos: Qos::AtLeastOnce,
            retain: true,
        };
        codec::publish(
            &mut bytes,
            "dashboard/sim/status",
            b"online",
            flags,
            Some(1),
        )
        .unwrap();
        codec::subscribe(&mut bytes, 2, &["a/state", "b/state"]).unwrap();
        codec::pingreq(&mut bytes);
        codec::disconnect(&mut bytes);

        let mut inspector = Inspector::default();
        let mut wire = Wire::new("broker:1883".into());
        // The bytes arrive in two chunks, and the first chunk ends in the
        // middle of a packet.
        let (first, second) = bytes.split_at(5);
        inspector.sent(&mut wire, first);
        assert_eq!(inspector.mqtt.len(), 0);
        inspector.sent(&mut wire, second);
        assert_eq!(
            rows(&inspector),
            [
                "Out Connect  broker:1883 as dashboard-sim",
                "Out Publish dashboard/sim/status online retained",
                "Out Subscribe 2 topics ",
                "Out Ping  ",
                "Out Disconnect  ",
            ]
        );
        let text = format!("{:?}", inspector.mqtt);
        assert!(!text.contains("secret-password"));
        assert!(!text.contains("dashboard\""), "the user name is not shown");
    }

    #[test]
    fn the_wire_shows_the_packets_in() {
        let mut bytes = Vec::new();
        server::connack(&mut bytes, false, 0);
        server::suback(&mut bytes, 1, &[0, codec::SUBACK_FAILURE]);
        let retained = Flags {
            qos: Qos::AtMostOnce,
            retain: true,
        };
        codec::publish(&mut bytes, "light/kitchen/state", b"on", retained, None).unwrap();
        server::pingresp(&mut bytes);

        let mut inspector = Inspector::default();
        let mut wire = Wire::new("broker:1883".into());
        inspector.received(&mut wire, &bytes);
        assert_eq!(
            rows(&inspector),
            [
                "In ConnAck  accepted",
                "In SubAck  2 topics, 1 refused",
                "In Publish light/kitchen/state on retained",
                "In Ping  ",
            ]
        );
        assert_eq!(inspector.messages, 1);
    }

    #[test]
    fn the_wire_stops_a_direction_that_is_not_mqtt() {
        let mut inspector = Inspector::default();
        let mut wire = Wire::new("broker:1883".into());
        // A remaining length of more than four bytes is not valid.
        inspector.received(&mut wire, &[0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
        let mut bytes = Vec::new();
        server::pingresp(&mut bytes);
        inspector.received(&mut wire, &bytes);
        assert_eq!(inspector.mqtt.len(), 0);

        // The other direction continues.
        let mut bytes = Vec::new();
        codec::pingreq(&mut bytes);
        inspector.sent(&mut wire, &bytes);
        assert_eq!(rows(&inspector), ["Out Ping  "]);
    }
}
