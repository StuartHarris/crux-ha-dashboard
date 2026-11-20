//! The state machine of one MQTT connection, with no I/O.
//!
//! The caller owns the byte stream and the timers. It gives each event of
//! the stream to [`Session::handle`] as an [`Input`]. Then it calls
//! [`Session::poll`] until it returns `None`, and does each [`Action`].
//! The caller opens the stream before it makes the session, or at the same
//! time.
//!
//! The caller is the code that owns the session, for example a feature of
//! the core. The shell does the I/O for the caller: it opens the stream,
//! sends the bytes and runs the timers.
//!
//! Each wait of the session has a timer, so a link that stops cannot make
//! the session wait without a limit. The session gives the timers as
//! [`Action::StartTimer`] with a relative time, and gets them back as
//! [`Input::Timer`]. A timer that the session replaced or cancelled has an
//! old generation, and the session ignores it.
//!
//! The session has at most one timer of each [`TimerKind`]. A new
//! `StartTimer` of a kind replaces the timer of that kind, so the caller can
//! clear the old timer. When the session cancels a timer, it gives
//! [`Action::CancelTimer`]. After [`Action::Ended`], the session uses no
//! timer, so the caller can clear all of them.

use alloc::{
    collections::{BTreeSet, VecDeque},
    string::String,
    vec::Vec,
};
use core::{fmt, mem, time::Duration};

use facet::Facet;

use super::types::{Connect, Message, Publish, Qos, SubscribeOutcome};
use super::{
    codec::{self, Flags, Inbound, TooLong},
    framer::{Frame, Framer},
    ping_interval, silence_limit,
};

/// The limits of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The longest packet body that the session reads. The session skips
    /// the body of a longer packet and stays connected.
    pub max_packet: usize,
    /// The largest number of publishes at `QoS` 1 that wait for a PUBACK.
    pub max_in_flight: usize,
    /// The largest remaining length of a SUBSCRIBE packet, unless one topic
    /// filter is longer.
    pub subscribe_packet: usize,
    /// The time for the stream to open.
    pub open: Duration,
    /// The time for the CONNACK packet after the stream opens.
    pub connack: Duration,
    /// The time for the PINGRESP packet after a PINGREQ packet.
    pub ping_response: Duration,
    /// The time for the shell to send the bytes of an [`Action::Send`].
    pub send: Duration,
    /// The time for the broker to acknowledge a publish at `QoS` 1 or a
    /// SUBSCRIBE packet. Refer to [`TimerKind::Ack`].
    pub ack: Duration,
    /// The largest number of bytes that wait for the next `Send`. A publish
    /// or a subscribe that goes above it is refused.
    pub max_outgoing: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_packet: super::MAX_PACKET,
            max_in_flight: super::MAX_IN_FLIGHT,
            subscribe_packet: super::SUBSCRIBE_PACKET,
            open: super::OPEN_TIMEOUT,
            connack: super::CONNACK_TIMEOUT,
            ping_response: super::PING_RESPONSE_TIMEOUT,
            send: super::SEND_TIMEOUT,
            ack: super::ACK_TIMEOUT,
            max_outgoing: super::MAX_OUTGOING,
        }
    }
}

/// The kinds of timer of a session.
#[derive(Facet, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum TimerKind {
    /// The stream must open before this timer expires.
    Open,
    /// The CONNACK packet must arrive before this timer expires.
    ConnAck,
    /// The session sends a PINGREQ packet when this timer expires.
    Ping,
    /// The PINGRESP packet must arrive before this timer expires.
    PingResp,
    /// The broker must send some bytes in each period of this timer.
    ///
    /// The session does not start this timer again for each packet. When
    /// the timer expires, the session ends if no bytes arrived in the
    /// period. Thus a silent broker is found one to two periods after its
    /// last bytes (120 to 240 s for a keep-alive of 60 s). This timer is a
    /// second line of defence: the `Ping` and `PingResp` timers find a dead
    /// link in 55 s or less.
    Silence,
    /// The shell must send the bytes before this timer expires.
    Send,
    /// The broker must acknowledge a publish at `QoS` 1 or a SUBSCRIBE packet
    /// in each period of this timer, while acknowledgements wait.
    ///
    /// The session starts this timer when the shell sent a packet that waits
    /// for an acknowledgement and the timer does not run. The session does
    /// not start it again for each packet. When the timer
    /// expires, the session ends if acknowledgements wait and none arrived
    /// in the period.
    Ack,
}

impl TimerKind {
    /// The number of kinds.
    pub(crate) const COUNT: usize = 7;

    /// Returns a different index below [`TimerKind::COUNT`] for each kind.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Open => 0,
            Self::ConnAck => 1,
            Self::Ping => 2,
            Self::PingResp => 3,
            Self::Silence => 4,
            Self::Send => 5,
            Self::Ack => 6,
        }
    }
}

/// A timer that the session started. The caller gives it back as
/// [`Input::Timer`] when the time is over.
///
/// The core keeps it in an event, so it derives `Facet`.
#[derive(Facet, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    /// The kind of the timer.
    pub kind: TimerKind,
    generation: u32,
}

/// An event of the byte stream or of a timer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// The stream opened.
    Opened,
    /// Bytes from the broker arrived. A chunk can hold any part of the
    /// stream.
    Received(Vec<u8>),
    /// The shell sent the bytes of the last [`Action::Send`].
    Sent,
    /// The stream closed.
    Closed {
        /// Why the stream closed.
        reason: String,
        /// `true` if the broker host ended the stream. Refer to
        /// [`End::ByHost`].
        by_host: bool,
    },
    /// A timer from [`Action::StartTimer`] expired.
    Timer(Timer),
}

/// The id of a subscribe job. Refer to [`Session::subscribe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SubscribeId(u32);

/// A thing for the caller to do.
#[derive(Clone, PartialEq, Eq)]
pub enum Action {
    /// Send the bytes on the stream, then give [`Input::Sent`]. The session
    /// gives the next `Send` only after `Sent`.
    Send(Vec<u8>),
    /// Close the stream.
    Close,
    /// Start a timer. After the time, give it back as [`Input::Timer`].
    /// The timer replaces the timer of the same kind, if one runs.
    StartTimer {
        /// The timer.
        timer: Timer,
        /// The time from now.
        after: Duration,
    },
    /// The session no longer uses the timer of this kind. The caller can
    /// clear it. If the timer expires first, the session ignores it.
    CancelTimer(TimerKind),
    /// The broker accepted the connection.
    Connected,
    /// A message arrived on a subscribed topic.
    Message(Message),
    /// A PUBLISH packet arrived with a body longer than
    /// [`Limits::max_packet`]. The session skipped it and stays connected.
    Skipped {
        /// The length of the body.
        length: usize,
    },
    /// The answer to a subscribe job.
    Subscribed {
        /// The id that [`Session::subscribe`] returned.
        id: SubscribeId,
        /// The outcome of the job.
        outcome: SubscribeOutcome,
    },
    /// The session ended. This is the last action of the session.
    Ended(End),
}

impl fmt::Debug for Action {
    // The CONNECT packet can hold the password, so the bytes of a `Send`
    // show only as their length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send(bytes) => f
                .debug_tuple("Send")
                .field(&format_args!("{} bytes", bytes.len()))
                .finish(),
            Self::Close => f.write_str("Close"),
            Self::StartTimer { timer, after } => f
                .debug_struct("StartTimer")
                .field("timer", timer)
                .field("after", after)
                .finish(),
            Self::CancelTimer(kind) => f.debug_tuple("CancelTimer").field(kind).finish(),
            Self::Connected => f.write_str("Connected"),
            Self::Message(message) => f.debug_tuple("Message").field(message).finish(),
            Self::Skipped { length } => f.debug_struct("Skipped").field("length", length).finish(),
            Self::Subscribed { id, outcome } => f
                .debug_struct("Subscribed")
                .field("id", id)
                .field("outcome", outcome)
                .finish(),
            Self::Ended(end) => f.debug_tuple("Ended").field(end).finish(),
        }
    }
}

/// Why the session did not take a publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The broker did not accept the connection yet, or the caller called
    /// [`Session::disconnect`], or the session ended.
    NotConnected,
    /// The largest number of publishes at `QoS` 1 wait for a PUBACK, or the
    /// bytes that wait for the next `Send` would go above
    /// [`Limits::max_outgoing`].
    Full,
    /// The topic or the payload is too long for an MQTT packet.
    TooLong,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotConnected => "the session is not connected",
            Self::Full => "too many publishes or bytes wait",
            Self::TooLong => "the topic or the payload is too long",
        })
    }
}

/// Why the session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// The caller asked for a disconnect, and the session closed the stream.
    Clean,
    /// The broker refused the connection with this CONNACK return code.
    Refused(u8),
    /// The broker sent bytes that are not a valid packet.
    Malformed,
    /// A timer of this kind expired.
    Timeout(TimerKind),
    /// The stream closed, for this reason.
    Transport(String),
    /// The broker host ended the stream, for this reason. It refused the
    /// connect with a TCP reset, or it reset or closed the open stream. The
    /// host got bytes from the client, and its answer arrived.
    ByHost(String),
    /// The broker kept a session, but the client asked for a clean session.
    SessionPresent,
    /// A field of the CONNECT packet is too long.
    TooLong,
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clean => f.write_str("the client closed the connection"),
            Self::Refused(code) => {
                let reason = match code {
                    1 => "unacceptable protocol version",
                    2 => "identifier rejected",
                    3 => "server unavailable",
                    4 => "bad user name or password",
                    5 => "not authorized",
                    _ => "unknown code",
                };
                write!(f, "the broker refused the connection ({code}: {reason})")
            }
            Self::Malformed => f.write_str("the broker sent a packet that is not valid"),
            Self::Timeout(kind) => f.write_str(match kind {
                TimerKind::Open => "the stream did not open in time",
                TimerKind::ConnAck => "the broker did not answer the CONNECT packet in time",
                TimerKind::Ping => "the ping timer expired",
                TimerKind::PingResp => "the broker did not answer the ping in time",
                TimerKind::Silence => "the broker sent nothing for too long",
                TimerKind::Send => "the shell did not send the bytes in time",
                TimerKind::Ack => "the broker did not acknowledge a packet in time",
            }),
            Self::Transport(reason) => write!(f, "the stream closed: {reason}"),
            Self::ByHost(reason) => write!(f, "the broker host ended the stream: {reason}"),
            Self::SessionPresent => {
                f.write_str("the broker kept a session, but the client asked for a clean session")
            }
            Self::TooLong => f.write_str("a field of the CONNECT packet is too long"),
        }
    }
}

/// The state of the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// The session waits for the stream to open.
    Opening,
    /// The session sent CONNECT and waits for CONNACK.
    Connecting,
    /// The broker accepted the connection.
    Connected,
    /// The DISCONNECT packet waits to be sent.
    Closing,
    /// The session gave [`Action::Ended`].
    Ended,
}

/// The `Send` that the shell did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flight {
    /// No `Send` is in flight.
    Idle,
    /// A `Send` is in flight. `acks` is `true` if it holds a packet that
    /// waits for an acknowledgement.
    Data { acks: bool },
    /// The `Send` in flight holds the DISCONNECT packet.
    Disconnect,
}

/// A subscribe job that waits for its SUBACK packets.
#[derive(Debug)]
struct Job {
    id: SubscribeId,
    /// The packet id and the number of filters of each SUBSCRIBE packet that
    /// waits for a SUBACK.
    packets: Vec<(u16, usize)>,
    /// `true` if the broker refused a filter.
    failed: bool,
}

/// The watch on the acknowledgements. Refer to [`TimerKind::Ack`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckWatch {
    /// The `Ack` timer does not run.
    Off,
    /// The `Ack` timer runs. `progress` is `true` if an acknowledgement
    /// arrived after the timer started.
    Armed { progress: bool },
}

/// A packet from the broker that owns its data, so the framer is free
/// again.
enum Packet {
    ConnAck { session_present: bool, code: u8 },
    Publish { message: Message, id: Option<u16> },
    PubAck(u16),
    SubAck { id: u16, count: usize, failed: bool },
    PingResp,
}

impl From<Inbound<'_>> for Packet {
    fn from(inbound: Inbound<'_>) -> Self {
        match inbound {
            Inbound::ConnAck {
                session_present,
                code,
            } => Self::ConnAck {
                session_present,
                code,
            },
            Inbound::Publish { topic, payload, id } => Self::Publish {
                message: Message {
                    topic: topic.into(),
                    payload: payload.to_vec(),
                },
                id,
            },
            Inbound::PubAck { id } => Self::PubAck(id),
            Inbound::SubAck { id, codes } => Self::SubAck {
                id,
                count: codes.len(),
                failed: codes.contains(&codec::SUBACK_FAILURE),
            },
            Inbound::PingResp => Self::PingResp,
        }
    }
}

/// One MQTT 3.1.1 connection with a clean session.
///
/// The session subscribes at `QoS` 0, and publishes at the `QoS` of each
/// [`Publish`]. The last will has `QoS` 1.
pub struct Session {
    state: State,
    limits: Limits,
    ping_interval: Option<Duration>,
    silence_limit: Option<Duration>,
    /// The CONNECT packet. It can hold the password.
    connect: Result<Vec<u8>, TooLong>,
    framer: Framer,
    actions: VecDeque<Action>,
    /// The bytes for the next `Send`, in the order of the requests. They can
    /// hold the password.
    outgoing: Vec<u8>,
    flight: Flight,
    generations: [u32; TimerKind::COUNT],
    /// `true` if bytes arrived after the last start of the silence timer.
    heard: bool,
    /// `true` if a PINGREQ packet waits for its PINGRESP.
    pinging: bool,
    /// `true` if the bytes for the next `Send` hold a packet that waits for
    /// an acknowledgement.
    unsent_acks: bool,
    ack_watch: AckWatch,
    next_id: u16,
    /// The packet ids of the publishes that wait for a PUBACK.
    publishes: BTreeSet<u16>,
    jobs: Vec<Job>,
    next_job: u32,
}

impl fmt::Debug for Session {
    // The CONNECT packet and the outgoing bytes can hold the password, so
    // they show only as their length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let connect = self.connect.as_ref().map(Vec::len);
        f.debug_struct("Session")
            .field("state", &self.state)
            .field("limits", &self.limits)
            .field("connect_bytes", &connect)
            .field("outgoing_bytes", &self.outgoing.len())
            .field("flight", &self.flight)
            .field("actions", &self.actions)
            .field("publishes", &self.publishes)
            .field("jobs", &self.jobs)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Makes a session for `connect`. The first action starts the timer for
    /// the stream to open.
    ///
    /// The session ignores the host and the port of `connect`. The caller
    /// opens the stream.
    #[must_use]
    pub fn new(connect: &Connect, limits: Limits) -> Self {
        let mut packet = Vec::new();
        let encoded = codec::connect(&mut packet, connect).map(|()| packet);
        let mut session = Self {
            state: State::Opening,
            limits,
            ping_interval: ping_interval(connect.keep_alive_secs),
            silence_limit: silence_limit(connect.keep_alive_secs),
            connect: encoded,
            framer: Framer::new(limits.max_packet),
            actions: VecDeque::new(),
            outgoing: Vec::new(),
            flight: Flight::Idle,
            generations: [0; TimerKind::COUNT],
            heard: false,
            pinging: false,
            unsent_acks: false,
            ack_watch: AckWatch::Off,
            next_id: 0,
            publishes: BTreeSet::new(),
            jobs: Vec::new(),
            next_job: 0,
        };
        session.start(TimerKind::Open, limits.open);
        session
    }

    /// Takes an event of the stream or of a timer.
    pub fn handle(&mut self, input: Input) {
        if self.state == State::Ended {
            return;
        }
        match input {
            Input::Opened => self.opened(),
            Input::Received(chunk) => self.received(chunk),
            Input::Sent => self.sent(),
            // After `disconnect`, the caller asked for the close.
            Input::Closed { .. } if self.state == State::Closing => self.end(End::Clean, false),
            Input::Closed {
                reason,
                by_host: true,
            } => self.end(End::ByHost(reason), false),
            Input::Closed { reason, .. } => self.end(End::Transport(reason), false),
            Input::Timer(timer) => self.expired(timer),
        }
    }

    /// Publishes a message. The bytes go in the next [`Action::Send`].
    ///
    /// # Errors
    ///
    /// Returns [`Refused`] if the broker did not accept the connection yet,
    /// after [`Session::disconnect`], if too many publishes at `QoS` 1 wait for
    /// a PUBACK, if the bytes that wait would go above
    /// [`Limits::max_outgoing`], or if the message is too long.
    pub fn publish(&mut self, publish: Publish) -> Result<(), Refused> {
        if self.state != State::Connected {
            return Err(Refused::NotConnected);
        }
        let Publish {
            topic,
            payload,
            qos,
            retain,
        } = publish;
        let id = match qos {
            Qos::AtMostOnce => None,
            Qos::AtLeastOnce => {
                if self.publishes.len() >= self.limits.max_in_flight {
                    return Err(Refused::Full);
                }
                Some(self.allocate(&[]).ok_or(Refused::Full)?)
            }
        };
        let flags = Flags { qos, retain };
        let before = self.outgoing.len();
        codec::publish(&mut self.outgoing, &topic, &payload, flags, id)
            .map_err(|TooLong| Refused::TooLong)?;
        if self.outgoing.len() > self.limits.max_outgoing {
            self.outgoing.truncate(before);
            return Err(Refused::Full);
        }
        if let Some(id) = id {
            self.publishes.insert(id);
            self.unsent_acks = true;
        }
        Ok(())
    }

    /// Subscribes to `topics` at `QoS` 0, and returns the id of the job.
    ///
    /// The session packs the filters in SUBSCRIBE packets of at most
    /// [`Limits::subscribe_packet`] bytes, in order. All the packets go in
    /// the next [`Action::Send`]. When the broker acknowledged all the
    /// packets, the session gives [`Action::Subscribed`] with `Done`. If the
    /// broker acknowledged all the packets but refused a filter, the outcome
    /// is `Refused`.
    ///
    /// The outcome is `Failed` at once if the session is not connected, if
    /// a filter is too long, or if the bytes that wait would go above
    /// [`Limits::max_outgoing`]. If the session ended, it gives no outcome,
    /// because [`Action::Ended`] is the last action.
    pub fn subscribe(&mut self, topics: &[String]) -> SubscribeId {
        let id = SubscribeId(self.next_job);
        self.next_job = self.next_job.wrapping_add(1);
        match self.state {
            State::Ended => return id,
            State::Connected => {}
            State::Opening | State::Connecting | State::Closing => {
                self.answer(id, SubscribeOutcome::Failed);
                return id;
            }
        }
        if topics.is_empty() {
            self.answer(id, SubscribeOutcome::Done);
            return id;
        }
        let mut bytes = Vec::new();
        let mut ids = Vec::new();
        let mut packets = Vec::new();
        let mut rest = topics;
        while !rest.is_empty() {
            let count = self.pack(rest);
            let (group, after) = rest.split_at(count);
            let Some(packet) = self.allocate(&ids) else {
                self.answer(id, SubscribeOutcome::Failed);
                return id;
            };
            if codec::subscribe(&mut bytes, packet, group).is_err() {
                self.answer(id, SubscribeOutcome::Failed);
                return id;
            }
            ids.push(packet);
            packets.push((packet, count));
            rest = after;
        }
        if self.outgoing.len() + bytes.len() > self.limits.max_outgoing {
            self.answer(id, SubscribeOutcome::Failed);
            return id;
        }
        self.outgoing.extend_from_slice(&bytes);
        self.unsent_acks = true;
        self.jobs.push(Job {
            id,
            packets,
            failed: false,
        });
        id
    }

    /// Closes the connection with a DISCONNECT packet, so the broker does not
    /// publish the last will.
    ///
    /// The DISCONNECT packet goes after all the bytes that wait. After its
    /// [`Input::Sent`], the session gives [`Action::Close`] and then
    /// `Ended(End::Clean)`. If the broker did not accept the connection yet,
    /// the session gives these two actions at once.
    pub fn disconnect(&mut self) {
        match self.state {
            State::Opening | State::Connecting => self.end(End::Clean, true),
            State::Connected => {
                codec::disconnect(&mut self.outgoing);
                self.state = State::Closing;
            }
            State::Closing | State::Ended => {}
        }
    }

    /// Returns the next action, or `None` if there is none.
    ///
    /// An [`Action::Send`] comes only when no other action waits, so the bytes
    /// of all the requests that the caller made meanwhile go in one `Send`.
    pub fn poll(&mut self) -> Option<Action> {
        if let Some(action) = self.actions.pop_front() {
            return Some(action);
        }
        let open = matches!(
            self.state,
            State::Connecting | State::Connected | State::Closing
        );
        if !open || self.flight != Flight::Idle || self.outgoing.is_empty() {
            return None;
        }
        self.flight = if self.state == State::Closing {
            Flight::Disconnect
        } else {
            Flight::Data {
                acks: mem::take(&mut self.unsent_acks),
            }
        };
        self.start(TimerKind::Send, self.limits.send);
        Some(Action::Send(mem::take(&mut self.outgoing)))
    }

    /// Returns the number of topic filters from the start of `topics` that go
    /// in one SUBSCRIBE packet. This is at least one.
    fn pack(&self, topics: &[String]) -> usize {
        // The packet id comes first.
        let mut length = 2;
        let mut count = 0;
        for topic in topics {
            let entry = codec::subscribe_entry_length(topic);
            if count > 0 && length + entry > self.limits.subscribe_packet {
                break;
            }
            length += entry;
            count += 1;
        }
        count
    }

    /// Returns the next packet id that is not 0, not in flight and not in
    /// `taken`, or `None` if all are in use.
    fn allocate(&mut self, taken: &[u16]) -> Option<u16> {
        for _ in 0..=u16::MAX {
            self.next_id = self.next_id.wrapping_add(1);
            let id = self.next_id;
            let used = id == 0
                || taken.contains(&id)
                || self.publishes.contains(&id)
                || self
                    .jobs
                    .iter()
                    .any(|job| job.packets.iter().any(|(packet, _)| *packet == id));
            if !used {
                return Some(id);
            }
        }
        None
    }

    /// Starts the `Ack` timer if it does not run.
    fn arm_ack(&mut self) {
        if self.ack_watch == AckWatch::Off {
            self.ack_watch = AckWatch::Armed { progress: false };
            self.start(TimerKind::Ack, self.limits.ack);
        }
    }

    /// Records that an acknowledgement arrived.
    const fn acknowledged(&mut self) {
        if let AckWatch::Armed { .. } = self.ack_watch {
            self.ack_watch = AckWatch::Armed { progress: true };
        }
    }

    fn answer(&mut self, id: SubscribeId, outcome: SubscribeOutcome) {
        self.actions.push_back(Action::Subscribed { id, outcome });
    }

    /// Starts the timer of `kind`. A timer of the same kind that runs now
    /// gets an old generation.
    fn start(&mut self, kind: TimerKind, after: Duration) {
        let generation = &mut self.generations[kind.index()];
        *generation = generation.wrapping_add(1);
        let timer = Timer {
            kind,
            generation: *generation,
        };
        self.actions.push_back(Action::StartTimer { timer, after });
    }

    /// Gives the timer of `kind` that runs now an old generation, and tells
    /// the caller to clear it.
    fn cancel(&mut self, kind: TimerKind) {
        let generation = &mut self.generations[kind.index()];
        *generation = generation.wrapping_add(1);
        self.actions.push_back(Action::CancelTimer(kind));
    }

    /// Ends the session. If `close` is `true`, the caller must close the
    /// stream first. Each subscribe job that waits gets `Failed`.
    fn end(&mut self, end: End, close: bool) {
        if close {
            self.actions.push_back(Action::Close);
        }
        for job in mem::take(&mut self.jobs) {
            self.answer(job.id, SubscribeOutcome::Failed);
        }
        self.actions.push_back(Action::Ended(end));
        self.state = State::Ended;
        self.outgoing = Vec::new();
        self.publishes.clear();
        self.ack_watch = AckWatch::Off;
        self.unsent_acks = false;
        self.framer = Framer::new(0);
    }

    fn opened(&mut self) {
        if self.state != State::Opening {
            return;
        }
        self.cancel(TimerKind::Open);
        match mem::replace(&mut self.connect, Err(TooLong)) {
            Ok(packet) => {
                self.outgoing = packet;
                self.state = State::Connecting;
                self.start(TimerKind::ConnAck, self.limits.connack);
            }
            Err(TooLong) => self.end(End::TooLong, true),
        }
    }

    fn sent(&mut self) {
        let flight = mem::replace(&mut self.flight, Flight::Idle);
        if flight == Flight::Idle {
            return;
        }
        self.cancel(TimerKind::Send);
        match flight {
            Flight::Disconnect => self.end(End::Clean, true),
            // The acknowledgements can only arrive after the bytes left, so
            // the watch starts now.
            Flight::Data { acks: true } => self.arm_ack(),
            Flight::Data { acks: false } | Flight::Idle => {}
        }
    }

    fn expired(&mut self, timer: Timer) {
        if timer.generation != self.generations[timer.kind.index()] {
            return;
        }
        match timer.kind {
            TimerKind::Ping => {
                if self.state != State::Connected {
                    return;
                }
                // If a PINGREQ waits, its PINGRESP timer keeps its deadline.
                if !self.pinging {
                    self.pinging = true;
                    codec::pingreq(&mut self.outgoing);
                    self.start(TimerKind::PingResp, self.limits.ping_response);
                }
                if let Some(interval) = self.ping_interval {
                    self.start(TimerKind::Ping, interval);
                }
            }
            TimerKind::Ack => self.ack_expired(),
            TimerKind::Silence if self.heard => {
                self.heard = false;
                if let Some(limit) = self.silence_limit {
                    self.start(TimerKind::Silence, limit);
                }
            }
            kind => self.end(End::Timeout(kind), true),
        }
    }

    fn ack_expired(&mut self) {
        let waiting = !self.publishes.is_empty() || !self.jobs.is_empty();
        match self.ack_watch {
            _ if !waiting => self.ack_watch = AckWatch::Off,
            AckWatch::Armed { progress: true } => {
                self.ack_watch = AckWatch::Off;
                self.arm_ack();
            }
            AckWatch::Armed { progress: false } | AckWatch::Off => {
                self.end(End::Timeout(TimerKind::Ack), true);
            }
        }
    }

    fn received(&mut self, chunk: Vec<u8>) {
        if self.state == State::Opening || chunk.is_empty() {
            return;
        }
        self.heard = true;
        self.framer.push(chunk);
        while self.state != State::Ended {
            let packet = match self.framer.next_packet() {
                Ok(None) => return,
                Ok(Some(Frame::Oversize { length, .. })) => {
                    self.actions.push_back(Action::Skipped { length });
                    continue;
                }
                Ok(Some(Frame::Packet { first, body })) => codec::decode(first, body),
                Err(malformed) => Err(malformed),
            };
            match packet {
                Ok(inbound) => {
                    let packet = Packet::from(inbound);
                    self.packet(packet);
                }
                Err(codec::Malformed) => self.end(End::Malformed, true),
            }
        }
    }

    fn packet(&mut self, packet: Packet) {
        match (self.state, packet) {
            (
                State::Connecting,
                Packet::ConnAck {
                    session_present,
                    code,
                },
            ) => self.connack(session_present, code),
            (State::Connecting, _) | (_, Packet::ConnAck { .. }) => {
                self.end(End::Malformed, true);
            }
            (_, Packet::Publish { message, id }) => {
                self.actions.push_back(Action::Message(message));
                if let (Some(id), State::Connected) = (id, self.state) {
                    codec::puback(&mut self.outgoing, id);
                }
            }
            (_, Packet::PubAck(id)) => {
                if self.publishes.remove(&id) {
                    self.acknowledged();
                }
            }
            (_, Packet::SubAck { id, count, failed }) => self.suback(id, count, failed),
            (_, Packet::PingResp) => {
                self.pinging = false;
                self.cancel(TimerKind::PingResp);
            }
        }
    }

    fn connack(&mut self, session_present: bool, code: u8) {
        self.cancel(TimerKind::ConnAck);
        if code != 0 {
            self.end(End::Refused(code), true);
            return;
        }
        if session_present {
            self.end(End::SessionPresent, true);
            return;
        }
        self.state = State::Connected;
        self.actions.push_back(Action::Connected);
        if let Some(interval) = self.ping_interval {
            self.start(TimerKind::Ping, interval);
        }
        if let Some(limit) = self.silence_limit {
            self.heard = false;
            self.start(TimerKind::Silence, limit);
        }
    }

    fn suback(&mut self, packet: u16, count: usize, failed: bool) {
        let found = self.jobs.iter().enumerate().find_map(|(index, job)| {
            job.packets
                .iter()
                .find(|(id, _)| *id == packet)
                .map(|(_, filters)| (index, *filters))
        });
        let Some((index, filters)) = found else {
            return;
        };
        // The SUBACK has one return code for each filter of its SUBSCRIBE.
        if count != filters {
            self.end(End::Malformed, true);
            return;
        }
        self.acknowledged();
        let job = &mut self.jobs[index];
        job.packets.retain(|(id, _)| *id != packet);
        job.failed |= failed;
        if job.packets.is_empty() {
            let job = self.jobs.remove(index);
            let outcome = if job.failed {
                SubscribeOutcome::Refused
            } else {
                SubscribeOutcome::Done
            };
            self.answer(job.id, outcome);
        }
    }
}
