//! The logs feature: the core keeps its own lines of the log and the warn
//! and error lines of the shell, each with the time from the shell, and
//! publishes them to Home Assistant.
//!
//! - Each line of the core goes to the shell as a
//!   [`Log`](crate::capabilities::log::Log) request. The shell writes the
//!   line and answers with its monotonic time ([`Uptime`]). The core then
//!   keeps the line with that time.
//! - The core does not keep a line at [`Level::Debug`] ([`debug`]). The
//!   shell writes it, but the line never goes into the ring, so it never
//!   goes to Home Assistant. A [`Line`] has the level type [`Kept`], which
//!   has no `Debug` variant.
//! - The shell sends its own lines as
//!   [`Event::ShellLogged`](crate::Event::ShellLogged), with the time on the
//!   same clock. The core keeps each line as it comes, with
//!   [`Source::Shell`]. The event does not render and does not wake the
//!   device. Refer to [`shell_logged`].
//! - The core keeps the lines in a ring of [`CAPACITY`] lines. If the ring
//!   is full, a new line removes the oldest line, and the core counts the
//!   removed lines ([`State::dropped`]).
//! - The ring keeps at most [`MAX_MESSAGE`] bytes of each message, and at
//!   most [`MAX_SOURCE`] bytes of the source of a line of the shell. The
//!   shell still writes the full message.
//! - The ring is in the model, so it keeps its lines through a light sleep,
//!   which keeps the RAM, and while the device is offline.
//! - The ring is only in RAM, so a chip restart loses its lines. This
//!   includes the lines that tell why the chip restarts, for example the
//!   restart of the Wi-Fi policy after 3 joins with no new address. A later
//!   PR can keep the lines through a restart.
//!
//! # Publish
//!
//! The core publishes each line on the log topic
//! ([`Settings::log_topic`](crate::Settings::log_topic)) at `QoS` 1, not
//! retained. The payload has the age of the line in seconds, not its time,
//! because the device has no wall clock. Refer to `docs/mqtt.md`.
//!
//! Each session of the connection has a [`Flight`], in its
//! [`Live`](crate::features::connection::Live). A session end drops the
//! flight with the session, so the packet ids of a session never go to the
//! next session.
//!
//! | Flight | Event | Next flight | What the core does |
//! | --- | --- | --- | --- |
//! | `Idle` | the connection is up, and a line or a drop count waits | `Clock` | sends a [`LogClock`](log::LogClock) request |
//! | `Clock` | the answer for the same connection | `Sending` | publishes from the oldest line |
//! | `Clock` | an answer for an older connection | `Clock` | nothing |
//! | `Sending` | a new line of the core or of the shell | `Sending` | publishes the line, after the older lines that wait |
//! | `Sending` | `Acked` for a line | `Sending` | removes the line from the ring, then publishes more |
//! | `Sending` | `Acked` for the drop message | `Sending` | subtracts its count from the drop count |
//! | any | the session ends | (none) | the lines with no PUBACK stay in the ring |
//!
//! - A line leaves the ring only when the broker acknowledges it. Thus a
//!   line can come to Home Assistant two times, if a PUBACK is lost (at
//!   least once).
//! - If lines were dropped, the drop message goes before the next line.
//!   Refer to [`DROP_SOURCE`].
//! - The core publishes only while the connection is up. It publishes
//!   nothing in the write of the CONNACK, because the clock request comes
//!   first. It publishes nothing from the start of a sleep, and nothing
//!   during the close for a change of access point. Refer to [`drain`].
//! - The age of a line can be a little short. A line that waits behind the
//!   limit of publishes gets its age from the time of the clock answer, or
//!   the age 0 if it is newer than that time.
//! - At most [`LOG_IN_FLIGHT`] publishes of the log wait for a PUBACK, so
//!   the log leaves [`RESERVED`] places for the status and service calls.
//! - If the log is at that limit, or the session takes no more publishes
//!   now ([`Refused::Full`]), the core continues after the next `Sent` or
//!   `Acked`. Refer to
//!   [`connection::pump`].
//! - The core writes no line about the publish of its own log, so the log
//!   cannot make more log. The shell can write a line about a failed
//!   write of the stream, also a write of the log. The session then ends,
//!   so the line waits for the next connect.

use alloc::{collections::VecDeque, format, string::String, vec::Vec};
use core::fmt;

use serde::Serialize;

use crate::{
    Cmd, Model,
    capabilities::log::{self, Area, Level, Uptime},
    features::connection::{self, Link},
    mqtt::{MAX_IN_FLIGHT, PacketId, Publish, Qos, Refused, Session},
};

/// The number of lines that the ring keeps.
pub const CAPACITY: usize = 64;

/// The maximum length of a message in the ring, in bytes.
pub const MAX_MESSAGE: usize = 200;

/// The maximum length of the source of a line of the shell, in bytes.
pub const MAX_SOURCE: usize = 32;

/// The mark at the end of a message that the ring cut.
pub const CUT_MARK: &str = "…";

/// The number of places for publishes at `QoS` 1 that the log leaves free
/// in each session.
///
/// The session takes at most [`MAX_IN_FLIGHT`] publishes that wait for a
/// PUBACK. If the log took all of them, a sleep could not publish `asleep`
/// (the core drops a refused `asleep`), and a service call would wait for
/// a PUBACK of the log. With these places free, the log never blocks the
/// status or a few taps.
pub const RESERVED: usize = 8;

/// The largest number of publishes of the log that wait for a PUBACK in a
/// session, the drop message included.
pub const LOG_IN_FLIGHT: usize = MAX_IN_FLIGHT - RESERVED;

/// The `source` of the drop message. The drop message tells Home Assistant
/// how many lines the ring removed before they went. It has the level
/// `warn` and the age 0.
pub const DROP_SOURCE: &str = "log";

/// The internal events of the logs feature. The core makes them from the
/// answers to its `Log` and `LogClock` requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The shell wrote the line, and answered with the time of the line.
    Stamped(Line),
    /// The shell answered the `LogClock` request for the connection with
    /// this id.
    Clock {
        /// The id of the connection. The core ignores the answer for an
        /// older connection.
        connection: u32,
        /// The time on the clock of the shell.
        now: Uptime,
    },
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Logs(event)
    }
}

/// Handles an internal event of the logs feature.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::Stamped(line) => stamped(line, model),
        Event::Clock { connection, now } => clock(connection, now, model),
    }
}

/// Keeps the line, and publishes it if the session sends lines.
///
/// While the flight is `Sending`, the line goes with no new clock request.
/// The line is newer than the time of the flight, so its age is 0.
fn stamped(line: Line, model: &mut Model) -> Cmd {
    model.logs.push(line);
    publish(model)
}

/// Starts to publish the lines that wait, if the connection is up.
/// [`drain`] publishes nothing from the start of a sleep.
fn publish(model: &mut Model) -> Cmd {
    if model.connection.link() == Link::Up {
        connection::pump(model)
    } else {
        Cmd::done()
    }
}

/// Keeps a line of the shell, and adds `dropped` to the drop count. Then
/// publishes the line as a line of the core.
///
/// The ring keeps at most [`MAX_SOURCE`] bytes of the source and
/// [`MAX_MESSAGE`] bytes of the message.
///
/// `dropped` is the number of lines that the shell itself lost since its
/// last [`Event::ShellLogged`](crate::Event::ShellLogged). The drop
/// message tells Home Assistant the sum of these lines and the lines that
/// the ring removed.
///
/// The event does not render, because the view does not show the log. It
/// does not wake the device. While the device sleeps, the line waits in the
/// ring until the next connect after the wake.
pub fn shell_logged(
    at: Uptime,
    level: Kept,
    source: &str,
    message: &str,
    dropped: u32,
    model: &mut Model,
) -> Cmd {
    model.logs.push(Line {
        at,
        level,
        source: Source::Shell(ShellSource::new(source)),
        message: Message::new(message),
    });
    model.logs.dropped = model.logs.dropped.saturating_add(dropped);
    publish(model)
}

/// Starts to publish with the time `now`, if the answer is for the
/// `LogClock` request of the current session. The core ignores the answer
/// for an older connection.
fn clock(connection: u32, now: Uptime, model: &mut Model) -> Cmd {
    if connection != model.connection.id {
        return Cmd::done();
    }
    let Some(live) = model.connection.live_mut() else {
        return Cmd::done();
    };
    if !matches!(live.log, Flight::Clock) {
        return Cmd::done();
    }
    live.log = Flight::Sending(Sending {
        now,
        next: 0,
        notice: None,
        lines: Vec::new(),
    });
    connection::pump(model)
}

/// Writes `message` to the log of the shell at the level `Debug`. The core
/// does not keep the line, so it does not go to Home Assistant.
///
/// The line is a `Log` request as the other lines are, so the order of the
/// lines and the effects is the same at each level. The command ignores the
/// answer, and sends no event.
pub fn debug(area: Area, message: impl Into<String>) -> Cmd {
    let request = log::write(Level::Debug, area, message.into());
    Cmd::new(|ctx| async move {
        let _: Uptime = request.into_future(ctx).await;
    })
}

/// Writes `message` to the log of the shell at the level `Info`, and keeps
/// the line with its time.
pub fn info(area: Area, message: impl Into<String>) -> Cmd {
    write(Kept::Info, area, message.into())
}

/// Writes `message` to the log of the shell at the level `Warn`, and keeps
/// the line with its time.
pub fn warn(area: Area, message: impl Into<String>) -> Cmd {
    write(Kept::Warn, area, message.into())
}

fn write(level: Kept, area: Area, message: String) -> Cmd {
    let kept = Message::new(&message);
    log::write(level.into(), area, message).then_send(move |at| {
        Event::Stamped(Line {
            at,
            level,
            source: Source::Core(area),
            message: kept,
        })
        .into()
    })
}

/// A line of the log that the core keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The time on the monotonic clock of the shell when the shell wrote
    /// the line.
    pub at: Uptime,
    /// How important the line is.
    pub level: Kept,
    /// The part of the core or the shell that wrote the line.
    pub source: Source,
    /// The text of the line, with at most [`MAX_MESSAGE`] bytes.
    pub message: Message,
}

/// How important a line in the ring is: a [`Level`] with no `Debug`.
///
/// The core does not keep a `Debug` line, so a line in the ring cannot have
/// that level. The payload has its [`name`](Kept::name) as `level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kept {
    /// A decision of the core.
    Info,
    /// A failure, or a decision because of a failure.
    Warn,
    /// A failure in the shell.
    Error,
}

impl Kept {
    /// Returns the name of the level, for example `info`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        Level::from_kept(self).name()
    }
}

impl From<Kept> for Level {
    fn from(level: Kept) -> Self {
        Self::from_kept(level)
    }
}

impl Level {
    /// Returns the level of a line in the ring.
    const fn from_kept(level: Kept) -> Self {
        match level {
            Kept::Info => Self::Info,
            Kept::Warn => Self::Warn,
            Kept::Error => Self::Error,
        }
    }
}

/// The part of the core or the shell that wrote a line. The payload has
/// its [`name`](Source::name) as `source`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A part of the core.
    Core(Area),
    /// A part of the shell, as the shell names it.
    Shell(ShellSource),
}

impl Source {
    /// Returns the name of the source: the name of the area, or the text
    /// from the shell.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Core(area) => area.name(),
            Self::Shell(source) => source.as_str(),
        }
    }
}

/// The source of a line of the shell, with at most [`MAX_SOURCE`] bytes.
///
/// If a text is longer, [`ShellSource::new`] cuts it as [`Message::new`]
/// cuts a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSource(String);

impl ShellSource {
    /// Makes a source from `text`, and cuts it if it is longer than
    /// [`MAX_SOURCE`] bytes.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self(cut(text, MAX_SOURCE))
    }

    /// Returns the text of the source.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The text of a line in the ring, with at most [`MAX_MESSAGE`] bytes.
///
/// If a text is longer, [`Message::new`] keeps its start and adds
/// [`CUT_MARK`]. The cut is on a character boundary, so the message is
/// always valid UTF-8, and the mark is in the [`MAX_MESSAGE`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message(String);

impl Message {
    /// Makes a message from `text`, and cuts it if it is longer than
    /// [`MAX_MESSAGE`] bytes.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self(cut(text, MAX_MESSAGE))
    }

    /// Returns the text of the message.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Returns `text`, or if it is longer than `max` bytes, its start and
/// [`CUT_MARK`] in `max` bytes. The cut is on a character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let end = text.floor_char_boundary(max - CUT_MARK.len());
    let mut kept = String::with_capacity(end + CUT_MARK.len());
    kept.push_str(&text[..end]);
    kept.push_str(CUT_MARK);
    kept
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The lines of the log that the core keeps: a ring of at most
/// [`CAPACITY`] lines, from the oldest, and the number of lines that the
/// ring removed.
///
/// A line stays in the ring until the broker acknowledges it. Each line has
/// a sequence number, so a [`Flight`] can refer to a line also after the
/// ring removed older lines.
#[derive(Debug, Default)]
pub struct State {
    entries: VecDeque<Entry>,
    dropped: u32,
    /// The sequence number of the next line.
    next: u64,
}

/// A line in the ring, with its sequence number.
#[derive(Debug)]
struct Entry {
    seq: u64,
    line: Line,
}

impl State {
    /// Adds `line` at the end of the ring. If the ring is full, first
    /// removes the oldest line and adds 1 to [`State::dropped`].
    ///
    /// The oldest line can be a line that waits for its PUBACK. Then the
    /// PUBACK does nothing, and the line counts as dropped.
    pub fn push(&mut self, line: Line) {
        if self.entries.len() == CAPACITY {
            self.entries.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        let seq = self.next;
        self.next += 1;
        self.entries.push_back(Entry { seq, line });
    }

    /// Returns the lines in the ring, from the oldest.
    pub fn lines(&self) -> impl ExactSizeIterator<Item = &Line> + DoubleEndedIterator {
        self.entries.iter().map(|entry| &entry.line)
    }

    /// Returns the number of lines that the ring removed before the broker
    /// acknowledged them. The count stops at `u32::MAX`.
    #[must_use]
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Returns `true` if a line or a drop count waits for the broker.
    fn waits(&self) -> bool {
        !self.entries.is_empty() || self.dropped > 0
    }

    /// Removes the line with the sequence number `seq`, if it is in the
    /// ring.
    fn remove(&mut self, seq: u64) {
        if let Some(index) = self.entries.iter().position(|entry| entry.seq == seq) {
            self.entries.remove(index);
        }
    }
}

/// The publish of the log on one session of the connection. Refer to the
/// table in the [module docs](self).
///
/// The flight is in the [`Live`](crate::features::connection::Live) of the
/// session, so the core makes a new flight for each session.
#[derive(Debug, Default)]
pub enum Flight {
    /// The core sends no lines on this session yet.
    #[default]
    Idle,
    /// The `LogClock` request waits for its answer. The answer carries the
    /// id of the connection, so an answer for an older session does nothing.
    Clock,
    /// The core publishes the lines with the ages from the time of the
    /// clock.
    Sending(Sending),
}

/// The state of a flight that publishes lines.
#[derive(Debug)]
pub struct Sending {
    /// The time on the clock of the shell. The age of a line is this time
    /// minus the time of the line.
    now: Uptime,
    /// The sequence number of the oldest line that the session did not
    /// publish. The lines from this number wait.
    next: u64,
    /// The drop message that waits for its PUBACK: its packet id and the
    /// count that it tells. At most one drop message waits.
    notice: Option<(PacketId, u32)>,
    /// The lines that wait for their PUBACK: the packet id and the sequence
    /// number of each line.
    lines: Vec<(PacketId, u64)>,
}

/// Publishes the lines that wait, or asks for the time first. Returns the
/// command of the clock request, and `true` if the session took a publish.
///
/// [`connection::pump`] calls this function while the connection is up,
/// after the outbox. The session then sends the publishes in the next
/// write.
///
/// From the start of a sleep, the core publishes no new line, and it asks
/// for no time. The connection can still be up in the first step of a
/// sleep, but its DISCONNECT soon follows, so the PUBACK of a new line is
/// often lost, and the line goes two times. The lines wait in the ring
/// until the wake. The PUBACKs of the lines that went before still count.
pub(crate) fn drain(model: &mut Model) -> (Cmd, bool) {
    if model.connection.link() != Link::Up || model.power.is_asleep() {
        return (Cmd::done(), false);
    }
    let Some(settings) = model.startup.settings() else {
        return (Cmd::done(), false);
    };
    let id = model.connection.id;
    let Some(live) = model.connection.live_mut() else {
        return (Cmd::done(), false);
    };
    match &mut live.log {
        Flight::Idle if model.logs.waits() => {
            live.log = Flight::Clock;
            let request = log::clock().then_send(move |now| {
                Event::Clock {
                    connection: id,
                    now,
                }
                .into()
            });
            (request, false)
        }
        Flight::Idle | Flight::Clock => (Cmd::done(), false),
        Flight::Sending(sending) => {
            let topic = settings.log_topic();
            let published = fill(sending, &mut live.session, &mut model.logs, &topic);
            (Cmd::done(), published)
        }
    }
}

/// Publishes the drop message, if it is due, and then the lines that wait,
/// from the oldest, until the session takes no more. Returns `true` if the
/// session took a publish.
///
/// The drop message goes first, if lines were dropped and no drop message
/// waits for its PUBACK. At most [`LOG_IN_FLIGHT`] publishes of the log
/// wait for a PUBACK, so the core stops at that number.
///
/// A line that is too long for a packet can never go, so the core removes
/// it and counts it as dropped.
fn fill(sending: &mut Sending, session: &mut Session, state: &mut State, topic: &str) -> bool {
    let mut published = false;
    loop {
        if sending.lines.len() + usize::from(sending.notice.is_some()) >= LOG_IN_FLIGHT {
            break;
        }
        let publish = |payload| Publish {
            topic: topic.into(),
            payload,
            qos: Qos::AtLeastOnce,
            retain: false,
        };
        if state.dropped > 0 && sending.notice.is_none() {
            let count = state.dropped;
            match session.publish(publish(dropped_json(count))) {
                Ok(Some(id)) => sending.notice = Some((id, count)),
                // A publish at `QoS` 1 always has a packet id, and the drop
                // message is short. The core tries again after the next
                // `Sent` or `Acked`.
                Ok(None) | Err(_) => break,
            }
        } else if let Some(entry) = state.entries.iter().find(|entry| entry.seq >= sending.next) {
            let seq = entry.seq;
            match session.publish(publish(Payload::of(&entry.line, sending.now).to_json())) {
                Ok(Some(id)) => {
                    sending.lines.push((id, seq));
                    sending.next = seq + 1;
                }
                Err(Refused::TooLong) => {
                    state.remove(seq);
                    state.dropped = state.dropped.saturating_add(1);
                    continue;
                }
                Ok(None) | Err(Refused::Full | Refused::NotConnected) => break,
            }
        } else {
            break;
        }
        published = true;
    }
    published
}

/// The broker acknowledged the publish with the packet id `id`. If it is a
/// line of the log, the line leaves the ring. If it is the drop message,
/// the core subtracts its count from the drop count. Lines that dropped
/// after the drop message went stay in the count.
///
/// The core ignores an id that is not a publish of the log, for example the
/// id of a service call.
pub(crate) fn acked(id: PacketId, model: &mut Model) {
    let Some(Flight::Sending(sending)) = model.connection.live_mut().map(|live| &mut live.log)
    else {
        return;
    };
    if let Some((_, count)) = sending.notice.take_if(|(notice, _)| *notice == id) {
        model.logs.dropped = model.logs.dropped.saturating_sub(count);
    } else if let Some(index) = sending.lines.iter().position(|(line, _)| *line == id) {
        let (_, seq) = sending.lines.remove(index);
        model.logs.remove(seq);
    }
}

/// Returns the JSON payload of the drop message for `count` lines.
fn dropped_json(count: u32) -> Vec<u8> {
    let message = if count == 1 {
        String::from("1 older line was dropped")
    } else {
        format!("{count} older lines were dropped")
    };
    Payload {
        level: Kept::Warn.name(),
        source: DROP_SOURCE,
        age: 0,
        msg: &message,
    }
    .to_json()
}

/// The JSON payload of a line on the log topic. Refer to `docs/mqtt.md`.
#[derive(Serialize)]
struct Payload<'a> {
    level: &'static str,
    source: &'a str,
    /// The age of the line in whole seconds, when the core publishes it.
    age: u64,
    msg: &'a str,
}

impl<'a> Payload<'a> {
    /// Returns the payload of `line` at the time `now`. A line that is
    /// newer than `now` has the age 0.
    fn of(line: &'a Line, now: Uptime) -> Self {
        Self {
            level: line.level.name(),
            source: line.source.name(),
            age: now.0.saturating_sub(line.at.0) / 1000,
            msg: line.message.as_str(),
        }
    }

    /// Returns the payload as JSON.
    fn to_json(&self) -> Vec<u8> {
        // The fields are text and a number, and the writer is a `Vec`, so the
        // serialization cannot fail.
        serde_json::to_vec(self).expect("a payload of text and a number serializes")
    }
}

#[cfg(test)]
mod tests;
