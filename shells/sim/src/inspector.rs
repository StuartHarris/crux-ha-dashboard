//! The records that the shell keeps for the window: a log of the effects and
//! a log of the MQTT traffic.
//!
//! The shell writes to an [`Inspector`]. The window only reads it, so the
//! tests can examine the logs with no window.
//!
//! Do not put a `Debug` text of an effect, a request or a `Start` event in a
//! log. These values contain the MQTT password.

use std::collections::VecDeque;

use chrono::{DateTime, Local};

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
    pub time: DateTime<Local>,
    /// The name of the effect, for example `MqttPublish`.
    pub effect: &'static str,
    /// The facts of the request, for example the topic. It never contains
    /// a password.
    pub request: String,
    /// What the shell did, for example `sent` or `resolved: Connected`.
    pub outcome: String,
}

/// The effects that the core asks for many times. The effect log can hide
/// them, so that the other effects stay visible.
pub const ROUTINE_EFFECTS: [&str; 2] = ["Render", "TimeNow"];

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
    /// The broker accepted the connection.
    ConnAck,
    /// A subscription to one or more topics.
    Subscribe,
    /// A message.
    Publish,
    /// The end of the connection.
    Disconnect,
}

/// What happened to an MQTT packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The shell received the packet.
    Received,
    /// The shell gave the packet to the client.
    Sent,
    /// The shell is read-only, so it did not send the packet.
    NotSent,
    /// The shell has no connection, so it did not send the packet.
    NoConnection,
    /// The client refused the packet, with this message.
    Failed(String),
}

/// One MQTT packet, in or out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttEntry {
    /// The time of the entry.
    pub time: DateTime<Local>,
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
    /// connect packet, the broker and the client id.
    pub payload: String,
    /// The retained flag of a message.
    pub retained: bool,
    /// What happened to the packet.
    pub delivery: Delivery,
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
}

impl Default for Inspector {
    fn default() -> Self {
        Self {
            effects: Log::new(CAPACITY),
            mqtt: Log::new(CAPACITY),
        }
    }
}

impl Inspector {
    /// Adds an entry to the effect log, with the time now.
    pub fn effect(&mut self, effect: &'static str, request: String, outcome: impl Into<String>) {
        self.effects.push(EffectEntry {
            time: Local::now(),
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
        delivery: Delivery,
    ) {
        self.mqtt.push(MqttEntry {
            time: Local::now(),
            direction,
            packet,
            topic: topic.into(),
            topics: Vec::new(),
            payload,
            retained,
            delivery,
        });
    }

    /// Adds one entry to the MQTT log for a subscribe request to `topics`,
    /// with the time now.
    pub fn subscribe(&mut self, topics: &[String], delivery: Delivery) {
        self.mqtt.push(MqttEntry {
            time: Local::now(),
            direction: Direction::Out,
            packet: Packet::Subscribe,
            topic: format!("{} topics", topics.len()),
            topics: topics.to_vec(),
            payload: String::new(),
            retained: false,
            delivery,
        });
    }
}

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::{
        Delivery, Direction, EffectEntry, Inspector, Log, MqttEntry, PAYLOAD_LIMIT, Packet,
        truncate,
    };

    fn effect(name: &'static str, request: &str) -> EffectEntry {
        EffectEntry {
            time: Local::now(),
            effect: name,
            request: request.into(),
            outcome: "sent".into(),
        }
    }

    fn message(topic: &str) -> MqttEntry {
        MqttEntry {
            time: Local::now(),
            direction: Direction::In,
            packet: Packet::Publish,
            topic: topic.into(),
            topics: Vec::new(),
            payload: "on".into(),
            retained: true,
            delivery: Delivery::Received,
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
        log.push(effect("MqttPublish", "dashboard/sim/cmd"));
        log.push(effect("Render", ""));
        log.push(effect("TimeNow", ""));
        let names = |filter| {
            log.filtered(filter)
                .map(|entry| entry.effect)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(""), ["TimeNow", "Render", "MqttPublish"]);
        assert_eq!(names("  RENDER "), ["Render"]);
        assert_eq!(names("cmd"), ["MqttPublish"]);
        assert_eq!(names("SENT"), ["TimeNow", "Render", "MqttPublish"]);
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
        log.push(effect("TimeNow", ""));
        log.push(effect("MqttPublish", "dashboard/sim/cmd"));
        log.push(effect("Render", ""));
        let names = |hide| {
            log.shown("", hide)
                .map(|entry| entry.effect)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(true), ["MqttPublish"]);
        assert_eq!(names(false), ["Render", "MqttPublish", "TimeNow"]);
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
        inspector.subscribe(&topics, Delivery::Sent);
        assert_eq!(inspector.mqtt.len(), 1);
        let entry = inspector.mqtt.iter().next().unwrap();
        assert_eq!(entry.topic, "2 topics");
        assert_eq!(entry.topics, topics);
        assert_eq!(inspector.mqtt.filtered("hall").count(), 1);
        assert_eq!(inspector.mqtt.filtered("garage").count(), 0);
    }
}
