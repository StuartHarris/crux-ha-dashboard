//! A fake MQTT 3.1.1 broker that reads and writes bytes, and a harness that
//! runs a [`Session`] against it.
//!
//! The broker keeps retained messages and the topic filters of the one
//! client. It matches filters with [`matches`] from the fake broker of the
//! app tests. Its `publish` has the same form as the one of that fake
//! broker, so a fake Home Assistant can publish through it.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use crate::{
    capabilities::mqtt::Qos,
    mqtt::{
        Action, Input, Limits, Session, Timer, TimerKind,
        codec::{
            self, Flags,
            server::{self, Packet},
        },
        framer::{Frame, Framer},
    },
    tests::broker::matches,
};

/// The fake broker.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate rule of the fake broker"
)]
#[derive(Default)]
pub struct WireBroker {
    framer: Option<Framer>,
    retained: BTreeMap<String, Vec<u8>>,
    /// The topic filters of the client.
    pub filters: Vec<String>,
    /// The session-present flag of the CONNACK packet.
    pub session_present: bool,
    /// The return code of the CONNACK packet.
    pub code: u8,
    /// The broker refuses this filter with the code 0x80.
    pub refuse: Option<String>,
    /// If `true`, the broker does not answer a SUBSCRIBE.
    pub hold_suback: bool,
    /// If `true`, the broker keeps the PUBACK of each publish in
    /// [`WireBroker::held_pubacks`].
    pub hold_puback: bool,
    /// If `true`, the broker does not answer a PINGREQ.
    pub hold_pingresp: bool,
    /// The packet ids of the PUBACK packets that the broker held back.
    pub held_pubacks: Vec<u16>,
    /// The packets from the client, in order.
    pub packets: Vec<Packet>,
}

impl WireBroker {
    /// Keeps a retained message, as if another client published it.
    pub fn retain(&mut self, topic: &str, payload: &[u8]) {
        self.retained.insert(topic.to_string(), payload.to_vec());
    }

    /// Reads bytes from the client, and returns the bytes of the answers.
    pub fn receive(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let framer = self
            .framer
            .get_or_insert_with(|| Framer::new(super::super::MAX_PACKET));
        framer.push(bytes);
        let mut packets = Vec::new();
        while let Some(frame) = framer.next_packet().expect("valid framing from the client") {
            let Frame::Packet { first, body } = frame else {
                panic!("an oversize packet from the client");
            };
            packets.push(server::decode(first, body).expect("a valid packet from the client"));
        }
        let mut out = Vec::new();
        for packet in packets {
            self.answer(&packet, &mut out);
            self.packets.push(packet);
        }
        out
    }

    /// Publishes a message from another client, and returns the bytes for
    /// the client if one of its filters matches the topic.
    pub fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) -> Vec<u8> {
        if retain {
            self.retain(topic, payload);
        }
        let mut out = Vec::new();
        if self.filters.iter().any(|filter| matches(filter, topic)) {
            let flags = Flags {
                qos: Qos::AtMostOnce,
                retain: false,
            };
            codec::publish(&mut out, topic, payload, flags, None).expect("a short publish");
        }
        out
    }

    fn answer(&mut self, packet: &Packet, out: &mut Vec<u8>) {
        match packet {
            Packet::Connect { .. } => server::connack(out, self.session_present, self.code),
            Packet::Publish {
                flags,
                topic,
                id,
                payload,
            } => {
                if flags & 0x01 != 0 {
                    self.retain(topic, payload);
                }
                if let Some(id) = *id {
                    if self.hold_puback {
                        self.held_pubacks.push(id);
                    } else {
                        server::puback(out, id);
                    }
                }
            }
            Packet::Subscribe { id, filters } => self.subscribe(*id, filters, out),
            Packet::PingReq if !self.hold_pingresp => server::pingresp(out),
            Packet::PubAck(_) | Packet::PingReq | Packet::Disconnect => {}
        }
    }

    /// Adds the filters, answers with a SUBACK and then sends the retained
    /// messages that match, as a broker does.
    fn subscribe(&mut self, id: u16, filters: &[(String, u8)], out: &mut Vec<u8>) {
        let mut codes = Vec::new();
        for (filter, _qos) in filters {
            if self.refuse.as_ref() == Some(filter) {
                codes.push(codec::SUBACK_FAILURE);
            } else {
                codes.push(0);
                self.filters.push(filter.clone());
            }
        }
        if !self.hold_suback {
            server::suback(out, id, &codes);
        }
        let flags = Flags {
            qos: Qos::AtMostOnce,
            retain: true,
        };
        for (filter, _qos) in filters {
            for (topic, payload) in &self.retained {
                if matches(filter, topic) {
                    codec::publish(out, topic, payload, flags, None).expect("a short publish");
                }
            }
        }
    }
}

/// How the harness gives the bytes from the broker to the session.
#[derive(Clone, Copy)]
pub enum Chunks {
    /// All the bytes of one answer in one chunk.
    Whole,
    /// Chunks of random size, from 1 to the given number of bytes.
    Random(u64, usize),
}

/// Runs a session against a [`WireBroker`].
pub struct Wire {
    pub session: Session,
    pub broker: WireBroker,
    pub chunks: Chunks,
    /// The actions that are not `Send` or `StartTimer`, in order.
    pub log: Vec<Action>,
    /// The timers that the session started, in order.
    pub timers: Vec<(Timer, Duration)>,
    /// The bytes of each `Send`, in order.
    pub sends: Vec<Vec<u8>>,
}

impl Wire {
    /// Makes a session for [`super::connect`] with the default limits.
    pub fn new() -> Self {
        Self::with(Limits::default())
    }

    /// Makes a session for [`super::connect`] with `limits`.
    pub fn with(limits: Limits) -> Self {
        Self {
            session: Session::new(&super::connect(), limits),
            broker: WireBroker::default(),
            chunks: Chunks::Whole,
            log: Vec::new(),
            timers: Vec::new(),
            sends: Vec::new(),
        }
    }

    /// Opens the stream and runs the session to the end of the CONNACK.
    pub fn open(&mut self) {
        self.session.handle(Input::Opened);
        self.run();
    }

    /// Opens the stream and runs the session to an accepted connection.
    /// Then clears the log, the sends and the packets of the broker, but
    /// keeps the timers.
    pub fn connected() -> Self {
        let mut wire = Self::new();
        wire.open();
        assert_eq!(wire.log, [Action::Connected]);
        wire.log.clear();
        wire.sends.clear();
        wire.broker.packets.clear();
        wire
    }

    /// Does the actions of the session until there are none.
    pub fn run(&mut self) {
        self.run_with(|_, _| {});
    }

    /// Does the actions of the session until there are none. Calls `react`
    /// with each action that is not `Send` or `StartTimer`, before the next
    /// poll.
    pub fn run_with(&mut self, mut react: impl FnMut(&mut Session, &Action)) {
        while let Some(action) = self.session.poll() {
            match action {
                Action::Send(bytes) => {
                    self.sends.push(bytes.clone());
                    let answer = self.broker.receive(bytes);
                    self.session.handle(Input::Sent);
                    self.deliver(&answer);
                }
                Action::StartTimer { timer, after } => self.timers.push((timer, after)),
                action => {
                    react(&mut self.session, &action);
                    self.log.push(action);
                }
            }
        }
    }

    /// Gives `bytes` from the broker to the session, in chunks.
    pub fn deliver(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match self.chunks {
            Chunks::Whole => self.session.handle(Input::Received(bytes.to_vec())),
            Chunks::Random(seed, max) => {
                for chunk in super::random_chunks(bytes, seed, max) {
                    self.session.handle(Input::Received(chunk));
                }
            }
        }
    }

    /// Returns the last timer of `kind` that the session started.
    pub fn timer(&self, kind: TimerKind) -> Timer {
        self.timers
            .iter()
            .rev()
            .find(|(timer, _)| timer.kind == kind)
            .map_or_else(|| panic!("no {kind:?} timer"), |(timer, _)| *timer)
    }

    /// Gives the timer back to the session, and runs it.
    pub fn fire(&mut self, timer: Timer) {
        self.session.handle(Input::Timer(timer));
        self.run();
    }

    /// Returns the kinds of the timers that the session started, in order.
    pub fn timer_kinds(&self) -> Vec<(TimerKind, Duration)> {
        self.timers
            .iter()
            .map(|(timer, after)| (timer.kind, *after))
            .collect()
    }
}

/// Reads all the packets in `bytes` from the client.
pub fn client_packets(bytes: &[u8]) -> Vec<Packet> {
    let mut framer = Framer::new(super::super::MAX_PACKET);
    framer.push(bytes.to_vec());
    let mut packets = Vec::new();
    while let Some(Frame::Packet { first, body }) = framer.next_packet().expect("valid framing") {
        packets.push(server::decode(first, body).expect("a valid packet"));
    }
    packets
}

/// Returns the remaining length of each packet in `bytes`.
pub fn remaining_lengths(bytes: &[u8]) -> Vec<usize> {
    let mut framer = Framer::new(super::super::MAX_PACKET);
    framer.push(bytes.to_vec());
    let mut lengths = Vec::new();
    while let Some(Frame::Packet { body, .. }) = framer.next_packet().expect("valid framing") {
        lengths.push(body.len());
    }
    lengths
}
