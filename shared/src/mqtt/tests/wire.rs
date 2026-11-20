//! A harness that runs a [`Session`] against the fake broker of
//! [`crate::fake`].

use alloc::vec::Vec;
use core::time::Duration;

use crate::{
    fake::Broker,
    mqtt::{
        Action, Input, Limits, Session, Timer, TimerKind,
        codec::server::{self, Packet},
        framer::{Frame, Framer},
    },
};

/// How the harness gives the bytes from the broker to the session.
#[derive(Clone, Copy)]
pub enum Chunks {
    /// All the bytes of one answer in one chunk.
    Whole,
    /// Chunks of random size, from 1 to the given number of bytes.
    Random(u64, usize),
}

/// Runs a session against a fake [`Broker`].
pub struct Wire {
    pub session: Session,
    pub broker: Broker,
    pub chunks: Chunks,
    /// The actions that are not `Send` or `StartTimer`, in order.
    pub log: Vec<Action>,
    /// The timers that the session started, in order.
    pub timers: Vec<(Timer, Duration)>,
    /// The kinds of the timers that the session cancelled, in order.
    pub cancels: Vec<TimerKind>,
    /// The bytes of each `Send`, in order.
    pub sends: Vec<Vec<u8>>,
}

impl Wire {
    /// Makes a session for [`super::connect`] with the default limits.
    pub fn new() -> Self {
        Self::with(Limits::default())
    }

    /// Makes a session for [`super::connect`] with `limits`, and opens a
    /// connection to the broker for it.
    pub fn with(limits: Limits) -> Self {
        let mut broker = Broker::default();
        broker.open().expect("the broker is up");
        Self {
            session: Session::new(&super::connect(), limits),
            broker,
            chunks: Chunks::Whole,
            log: Vec::new(),
            timers: Vec::new(),
            cancels: Vec::new(),
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
    /// with each action that is not `Send`, `StartTimer` or `CancelTimer`,
    /// before the next poll. A `Close` stops the connection to the broker.
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
                Action::CancelTimer(kind) => self.cancels.push(kind),
                action => {
                    if action == Action::Close {
                        self.broker.drop_connection();
                    }
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
