//! The connection feature: the MQTT session, reconnects, and the messages
//! from the Home Assistant state stream.
//!
//! The core runs the MQTT session ([`Session`]), and the shell gives it only
//! a byte stream. Refer to [`crate::capabilities::stream`]. Each wait of the
//! protocol is a state of the session with a timer, so a link that stops
//! cannot make the core wait without a limit. [`pump`] does the actions of
//! the session: it writes the bytes, starts the timers and handles the
//! results.
//!
//! Before it connects, the core gets a link from the Wi-Fi feature
//! ([`wifi::link`]). After each connect, the core subscribes to all the
//! topics. When the broker acknowledged each topic filter, the link is
//! quiet ([`wifi::quiet`]). The Wi-Fi feature can then scan for a better
//! access point. To change to it, the core closes the session with a
//! DISCONNECT packet ([`close_for_roam`]), and connects again.
//!
//! The connection is in one phase at a time. Each phase holds only the
//! data that is valid in it, so the state cannot contradict itself:
//!
//! 1. `Offline`: no stream is open. A reconnect timer can run.
//! 2. `Starting`: the stream opens, and the session waits for its CONNACK.
//! 3. `Up`: the broker accepted the connection.
//! 4. `Closing`: a DISCONNECT packet waits or is in flight, for a sleep or
//!    for a change of access point.
//! 5. `Draining`: the session ended, and the `Close` request for its stream
//!    waits for its answer. Then the phase is `Offline` again.
//!
//! A sleep waits for the close of the stream, and then gets
//! [`power::Event::Offline`]. With no stream, the sleep gets it at once.
//! Refer to [`go_offline`]. The view shows that the core is online only in
//! the `Up` phase. Refer to [`State::link`].
//!
//! The core writes a line to the log of the shell
//! ([`crate::capabilities::log`]) for each decision that is not usual, with
//! the area `mqtt`. A line comes when the broker refuses a topic filter,
//! when the core waits to connect again, when the core skips a message that
//! is too long, and when the core drops a service call that is too long.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{mem, time::Duration};

use chrono::{DateTime, Utc};
use crux_time::TimerHandle;
use futures::StreamExt;

use crate::{
    Cmd, Model,
    capabilities::{
        log::{self, Area},
        stream::{Stream, StreamItem, Written},
    },
    entities,
    features::{
        pending::{self, Tap},
        power,
        refresh::{redraw, redraw_soon},
        timer::{self, Running, Slot},
        wifi,
    },
    mqtt::{
        Action, Connect, End, Input, LastWill, Limits, Message, Publish, Qos, Refused, Session,
        SubscribeId, SubscribeOutcome, Timer, TimerKind,
    },
    summary,
};

/// The internal events of the MQTT connection. The core makes them from the
/// answers to its stream requests and from its timers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An item from the byte stream of the MQTT connection with this id.
    Stream {
        /// The id of the connection. The core ignores an item from an older
        /// connection.
        connection: u32,
        /// The item from the stream.
        item: StreamItem,
    },
    /// The answer to a `Write` request on the MQTT connection with this id.
    Written {
        /// The id of the connection. The core ignores the answer for an
        /// older connection.
        connection: u32,
        /// The answer.
        outcome: Written,
    },
    /// The answer to the `Close` request for the MQTT connection with this
    /// id.
    StreamClosed {
        /// The id of the connection.
        connection: u32,
    },
    /// The wait for the answer to the `Close` request for the MQTT
    /// connection with this id is over. Refer to [`CLOSE_TIMEOUT`].
    CloseTimeout {
        /// The id of the connection.
        connection: u32,
    },
    /// A timer of the MQTT session of the connection with this id expired.
    SessionTimer {
        /// The id of the connection. The core ignores the timer of an older
        /// connection.
        connection: u32,
        /// The timer.
        timer: Timer,
    },
    /// The wait before the reconnect with this id is over.
    Reconnect(u32),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Connection(event)
    }
}

/// Handles an internal event of the MQTT connection.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::Stream { connection, item } => stream(connection, item, model),
        Event::Written {
            connection,
            outcome,
        } => written(connection, &outcome, model),
        Event::StreamClosed { connection } => stream_closed(connection, model),
        Event::CloseTimeout { connection } => close_timeout(connection, model),
        Event::SessionTimer { connection, timer } => session_timer(connection, timer, model),
        Event::Reconnect(id) => reconnect(id, model),
    }
}

/// The first wait before the core connects again after a failure.
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
/// The longest wait before the core connects again after a failure.
pub const RECONNECT_MAX: Duration = Duration::from_secs(60);
// The log line of a reconnect shows whole seconds.
const _: () = assert!(RECONNECT_MIN.subsec_nanos() == 0 && RECONNECT_MAX.subsec_nanos() == 0);
/// The longest time that the core waits for the answer to a `Close`
/// request.
///
/// After this time, the core acts as if the answer arrived, so a
/// shell that does not answer cannot stop a reconnect or a sleep. No other
/// timer of the core has this duration.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(6);
/// The largest number of service calls that the core keeps while it is
/// offline. If there are more, the core removes the oldest call.
pub const OUTBOX_LIMIT: usize = 8;
/// The MQTT keep-alive interval in seconds.
pub const KEEP_ALIVE_SECS: u16 = 60;

/// The status payload while the dashboard is connected.
pub const STATUS_ONLINE: &str = "online";
/// The status payload after the dashboard disconnects to sleep.
pub const STATUS_ASLEEP: &str = "asleep";
/// The status payload that the broker publishes if the connection stops.
pub const STATUS_OFFLINE: &str = "offline";

/// The state of the MQTT connection, as the view shows it. Refer to
/// [`State::link`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
    /// The link is down.
    #[default]
    Down,
    /// The stream opens, or the session waits for its CONNACK.
    Starting,
    /// The link is up.
    Up,
}

/// The state of the MQTT connection.
#[derive(Debug)]
pub struct State {
    /// The phase of the connection.
    phase: Phase,
    /// The id of the newest MQTT connection. Each stream operation carries
    /// it, and the core ignores the answers for an older connection.
    pub id: u32,
    /// The number of messages that the session skipped, because they were
    /// too long.
    pub skipped: usize,
    /// The id of the newest reconnect timer.
    pub reconnect: u32,
    /// The wait before the next reconnect.
    pub backoff: Duration,
    /// The service calls that wait for a connection.
    pub outbox: Vec<Call>,
}

/// The phase of the MQTT connection. Each phase holds only the data that
/// is valid in it.
#[derive(Debug)]
enum Phase {
    /// No stream is open. The reconnect timer can run.
    ///
    /// The core leaves this phase when it gets a link and opens a stream
    /// ([`connect`]). Then the phase is `Starting`.
    Offline {
        /// The reconnect timer that runs.
        retry: Slot,
    },
    /// The stream opens, or the CONNECT packet waits for its CONNACK.
    ///
    /// The CONNACK makes the phase `Up` ([`connected`]). A failure ends the
    /// session, and the phase is `Draining`. A sleep ends the session at
    /// once, and the phase is `Draining`.
    Starting(Live),
    /// The broker accepted the connection. Messages change the entities.
    ///
    /// A sleep or a change of access point sends a DISCONNECT packet, and
    /// the phase is `Closing`. A failure ends the session, and the phase is
    /// `Draining`.
    Up {
        /// The session of the connection.
        live: Live,
        /// The subscribe job of the connection.
        subscribe: SubscribeId,
    },
    /// A DISCONNECT packet waits or is in flight, for a sleep or for a
    /// change of access point. The core drops the messages.
    ///
    /// When the session ends, the phase is `Draining` with the same
    /// [`After`].
    Closing {
        /// The session of the connection.
        live: Live,
        /// What the core does after the close.
        after: After,
    },
    /// The session ended, and the `Close` request for its stream waits for
    /// its answer. The core opens no stream and asks for no link in this
    /// phase. Refer to [`connect`].
    ///
    /// The answer, or the end of [`CLOSE_TIMEOUT`], makes the phase
    /// `Offline` and does the steps of `after`. Refer to [`stream_closed`].
    Draining {
        /// The timer of [`CLOSE_TIMEOUT`]. Its id is the id of the
        /// connection of the `Close` request.
        close: Running,
        /// The reconnect timer that runs.
        retry: Slot,
        /// What the core does after the answer.
        after: After,
    },
}

impl Default for Phase {
    fn default() -> Self {
        Self::Offline {
            retry: Slot::default(),
        }
    }
}

impl Phase {
    /// Returns the session, if a stream is open.
    const fn live_mut(&mut self) -> Option<&mut Live> {
        match self {
            Self::Starting(live) | Self::Up { live, .. } | Self::Closing { live, .. } => Some(live),
            Self::Offline { .. } | Self::Draining { .. } => None,
        }
    }

    /// Returns the reconnect timer, if the phase has no stream.
    const fn retry_mut(&mut self) -> Option<&mut Slot> {
        match self {
            Self::Offline { retry } | Self::Draining { retry, .. } => Some(retry),
            Self::Starting(_) | Self::Up { .. } | Self::Closing { .. } => None,
        }
    }

    /// Clears all the timers of the phase.
    fn clear_timers(self) {
        match self {
            Self::Offline { mut retry } => retry.stop(),
            Self::Starting(mut live)
            | Self::Up { mut live, .. }
            | Self::Closing { mut live, .. } => live.clear_timers(),
            Self::Draining {
                close,
                mut retry,
                after: _,
            } => {
                close.clear();
                retry.stop();
            }
        }
    }
}

/// What the core does after the close of a stream: the waiter of a sleep,
/// and the intent to connect again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct After {
    /// The sleep that waits for the close. The answer to the `Close`
    /// request sends [`power::Event::Offline`] for this sleep.
    sleep: Option<u32>,
    /// The core connects again after the close.
    connect: bool,
}

impl State {
    /// Returns the state of the connection, as the view shows it. A stream
    /// that closes shows as down.
    #[must_use]
    pub const fn link(&self) -> Link {
        match self.phase {
            Phase::Offline { .. } | Phase::Closing { .. } | Phase::Draining { .. } => Link::Down,
            Phase::Starting(_) => Link::Starting,
            Phase::Up { .. } => Link::Up,
        }
    }

    /// Returns the sleep that waits for the close of the stream, or `None`.
    /// Refer to [`go_offline`].
    #[must_use]
    pub const fn sleep_waiter(&self) -> Option<u32> {
        match self.phase {
            Phase::Closing { after, .. } | Phase::Draining { after, .. } => after.sleep,
            Phase::Offline { .. } | Phase::Starting(_) | Phase::Up { .. } => None,
        }
    }

    /// Returns `true` if a stream is open.
    #[must_use]
    pub const fn has_stream(&self) -> bool {
        matches!(
            self.phase,
            Phase::Starting(_) | Phase::Up { .. } | Phase::Closing { .. }
        )
    }
}

/// The session of an open connection.
#[derive(Debug)]
pub struct Live {
    /// The MQTT session.
    pub session: Session,
    /// The core wrote the line for a message that is too long on this
    /// connection. Later messages on the connection only count in
    /// [`State::skipped`].
    pub skip_logged: bool,
    /// The handle of the timer of each kind that runs, at the index of its
    /// kind.
    timers: [Option<TimerHandle>; TimerKind::COUNT],
}

impl Live {
    /// Makes the state of a new session.
    fn new(session: Session) -> Self {
        Self {
            session,
            skip_logged: false,
            timers: Default::default(),
        }
    }

    /// Keeps the handle of the timer of `kind` that the session started.
    /// Clears the timer of that kind before it, because the session no
    /// longer uses that timer.
    fn timer_started(&mut self, kind: TimerKind, handle: TimerHandle) {
        if let Some(old) = self.timers[kind.index()].replace(handle) {
            old.clear();
        }
    }

    /// Clears the timer of `kind`, if it runs.
    fn cancel_timer(&mut self, kind: TimerKind) {
        if let Some(handle) = self.timers[kind.index()].take() {
            handle.clear();
        }
    }

    /// Clears all the timers of the session.
    fn clear_timers(&mut self) {
        for handle in self.timers.iter_mut().filter_map(Option::take) {
            handle.clear();
        }
    }
}

/// A service call that waits in the outbox.
#[derive(Debug)]
pub struct Call {
    /// The message to publish.
    pub publish: Publish,
    /// The entity of the service call, which has a mark. Refer to
    /// [`pending`].
    pub entity_id: &'static str,
}

impl Default for State {
    fn default() -> Self {
        Self {
            phase: Phase::default(),
            id: 0,
            skipped: 0,
            reconnect: 0,
            backoff: RECONNECT_MIN,
            outbox: Vec::new(),
        }
    }
}

/// Forgets the connection and the radio after the shell starts. The core
/// ignores the answers to earlier requests.
///
/// The phase becomes `Offline`, and the core clears every timer of the
/// connection.
pub fn reset(model: &mut Model) {
    wifi::reset(model);
    let state = &mut model.connection;
    mem::take(&mut state.phase).clear_timers();
    state.id = state.id.wrapping_add(1);
    state.reconnect = state.reconnect.wrapping_add(1);
    state.backoff = RECONNECT_MIN;
}

/// Starts a new online cycle after a wake or a move to the stand. The next
/// failure waits the shortest time again.
pub const fn restart(model: &mut Model) {
    model.connection.backoff = RECONNECT_MIN;
}

/// Gets a link and connects, if the core wants a connection and has no
/// stream. Refer to [`wifi::link`].
///
/// While the `Close` request of an old stream waits for its answer, the
/// core does not ask for the link yet. A radio request, for example a
/// `Leave` for a change of access point, can stop the old stream before the
/// DISCONNECT packet goes. The answer to the `Close` request continues.
/// Refer to [`stream_closed`].
///
/// If the device wakes during the close for a sleep, the core forgets the
/// sleep and connects again after the close.
pub fn go_online(model: &mut Model) -> Cmd {
    // `wants_online` is false while the device sleeps. Thus the core can
    // forget a sleep that waits for a close below.
    if !model.wants_online() {
        return Cmd::done();
    }
    match &mut model.connection.phase {
        Phase::Offline { .. } => wifi::link(model),
        Phase::Closing { after, .. } | Phase::Draining { after, .. } => {
            *after = After {
                sleep: None,
                connect: true,
            };
            Cmd::done()
        }
        Phase::Starting(_) | Phase::Up { .. } => Cmd::done(),
    }
}

/// The Wi-Fi feature has a link: connect, if the core wants a connection.
pub fn link_up(model: &mut Model) -> Cmd {
    if model.wants_online() {
        connect(model)
    } else {
        Cmd::done()
    }
}

/// The Wi-Fi feature did not get a link: wait for the backoff, and then try
/// again, if the core wants a connection. With a stream, the core ignores
/// the failure.
pub fn link_failed(model: &mut Model) -> Cmd {
    if model.wants_online() {
        schedule_reconnect(model)
    } else {
        Cmd::done()
    }
}

/// Closes the session with a DISCONNECT packet, so that the reconnect goes
/// to a better access point. Refer to [`wifi::quiet`].
///
/// The end is clean, so the broker keeps `online` and does not publish the
/// last will. During the close, the view shows that the core connects, and
/// the core drops the messages. The core connects again after the answer
/// to the `Close` request. Refer to [`ended`]. With no session that is up,
/// the next connect goes to the better access point, so the core does
/// nothing now.
pub fn close_for_roam(model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    match mem::take(&mut state.phase) {
        Phase::Up {
            mut live,
            subscribe: _,
        } => {
            live.session.disconnect();
            state.phase = Phase::Closing {
                live,
                after: After {
                    sleep: None,
                    connect: true,
                },
            };
            pump(model)
        }
        phase => {
            state.phase = phase;
            Cmd::done()
        }
    }
}

/// Closes the connection for the sleep `sleep`. The close then sends
/// [`power::Event::Offline`] for the sleep, which starts the next step of
/// the sleep.
///
/// - If the connection is up, the core publishes `asleep` on the status
///   topic and then sends a DISCONNECT packet, so the broker does not
///   publish the last will. The answer to the `Close` request sends
///   `Offline`.
/// - A session that is not up ends at once. The answer to its `Close`
///   request sends `Offline`.
/// - If the `Close` request of an old stream waits for its answer, that
///   answer sends `Offline`. The core sends no other `Close` request.
/// - With no stream, the core sends `Offline` at once.
///
/// A new sleep replaces the sleep that waits. The core stops the reconnect
/// timer, and does not connect again after the close.
///
/// The core drops the service calls in the outbox and removes their marks.
/// The mark of a call that went out stays, but its timeout of the outbox
/// stops. The device sleeps, so the core does not draw.
pub fn go_offline(sleep: u32, model: &mut Model) -> Cmd {
    model.connection.outbox.clear();
    pending::forget_dropped(model);
    let status = model.startup.settings().map(|settings| Publish {
        topic: settings.status_topic(),
        payload: STATUS_ASLEEP.into(),
        qos: Qos::AtLeastOnce,
        retain: true,
    });
    let state = &mut model.connection;
    // The core ignores the answers to earlier requests.
    state.reconnect = state.reconnect.wrapping_add(1);
    let after = After {
        sleep: Some(sleep),
        connect: false,
    };
    let (mut live, up) = match mem::take(&mut state.phase) {
        Phase::Offline { mut retry } => {
            retry.stop();
            state.phase = Phase::Offline { retry };
            return Cmd::event(power::Event::Offline(sleep).into());
        }
        Phase::Draining {
            close,
            mut retry,
            after: _,
        } => {
            retry.stop();
            state.phase = Phase::Draining {
                close,
                retry,
                after,
            };
            return Cmd::done();
        }
        Phase::Starting(live) | Phase::Closing { live, after: _ } => (live, false),
        Phase::Up { live, subscribe: _ } => (live, true),
    };
    if up && let Some(status) = status {
        // A publish that the session refuses cannot stop the sleep, so the
        // core drops it.
        let _ = live.session.publish(status);
    }
    live.session.disconnect();
    state.phase = Phase::Closing { live, after };
    pump(model)
}

/// Publishes a service call now, or keeps it until the core connects.
///
/// `tap` is the tap that asks for the call, or `None` for a call with no
/// mark. The tap gives the entity of the call a mark, or replaces the tap of
/// its mark. When the core issues the call, the timeout of the mark starts.
/// While the call waits in the outbox, the timeout of the outbox runs. If
/// the outbox is full, the core drops the oldest call and removes its mark.
/// Refer to [`pending`].
///
/// If the session cannot take the call now, the call goes first in the
/// outbox. The outbox drains at the end of each [`pump`].
pub fn send(call: Call, tap: Option<Tap>, model: &mut Model) -> Cmd {
    if model.connection.link() == Link::Up {
        return match issue(call, tap, model) {
            Ok(mark) => pump(model).and(mark),
            Err((call, tap)) => {
                let entity_id = call.entity_id;
                model.connection.outbox.insert(0, call);
                pending::queued(entity_id, tap, model)
            }
        };
    }
    let entity_id = call.entity_id;
    let outbox = &mut model.connection.outbox;
    let full = outbox.len() == OUTBOX_LIMIT;
    if full {
        outbox.remove(0);
    }
    outbox.push(call);
    if full {
        pending::forget_dropped(model);
    }
    pending::queued(entity_id, tap, model).and(go_online(model))
}

/// Gives a service call to the session and starts the timer of its mark.
/// Returns the command of the timer.
///
/// `tap` is the tap that asks for the call, or `None` for a call from the
/// outbox. Refer to [`send`].
///
/// A call that is too long for a packet can never go. The core drops it,
/// removes its mark, and writes a line to the log. The command that the
/// function returns holds that line. If the entity has a mark, the tap
/// replaces the tap of the mark before the core removes the mark. The mark
/// then stays if it has a timeout of an issued call or a call in the outbox.
///
/// # Errors
///
/// Returns the call and the tap if the session cannot take the call now.
fn issue(call: Call, tap: Option<Tap>, model: &mut Model) -> Result<Cmd, (Call, Option<Tap>)> {
    let Some(live) = model.connection.phase.live_mut() else {
        return Err((call, tap));
    };
    match live.session.publish(call.publish.clone()) {
        Ok(()) => Ok(pending::issued(call.entity_id, tap, model)),
        Err(Refused::TooLong) => {
            let reason = log::warn(
                Area::Mqtt,
                format!(
                    "the service call for {} is too long for a packet, so the core dropped it",
                    call.entity_id
                ),
            );
            if let Some(tap) = tap {
                pending::retap(call.entity_id, tap, model);
            }
            pending::forget_dropped(model);
            Ok(reason)
        }
        Err(Refused::Full | Refused::NotConnected) => Err((call, tap)),
    }
}

/// Issues the service calls of the outbox, in order, until the session
/// cannot take more. Returns the commands of the timers of their marks.
fn drain_outbox(model: &mut Model) -> Cmd {
    let mut commands = Vec::new();
    while !model.connection.outbox.is_empty() {
        let call = model.connection.outbox.remove(0);
        match issue(call, None, model) {
            Ok(command) => commands.push(command),
            Err((call, _)) => {
                model.connection.outbox.insert(0, call);
                break;
            }
        }
    }
    Cmd::all(commands)
}

/// Tries to connect again after the wait for the reconnect `id`.
pub fn reconnect(id: u32, model: &mut Model) -> Cmd {
    let expired = model
        .connection
        .phase
        .retry_mut()
        .is_some_and(|retry| retry.expired(id));
    if expired {
        go_online(model)
    } else {
        Cmd::done()
    }
}

/// Opens a stream and starts a session, if there is no stream.
///
/// A session that closes for a sleep is still there after a wake during the
/// close. The core connects again after that close.
///
/// After a session ends, the core does not open a stream until the shell
/// answers the `Close` request for the old stream. A new `Open` makes the
/// shell abort the old stream. An abort can send a TCP reset, and then the
/// broker can lose bytes that it did not read, for example the DISCONNECT.
/// Without the DISCONNECT, the broker publishes the last will. Refer to
/// [`stream_closed`].
fn connect(model: &mut Model) -> Cmd {
    let Some(settings) = model.startup.settings() else {
        return Cmd::done();
    };
    let state = &mut model.connection;
    match &mut state.phase {
        Phase::Offline { .. } => {}
        Phase::Draining { after, .. } => {
            after.connect = true;
            return Cmd::done();
        }
        Phase::Starting(_) | Phase::Up { .. } | Phase::Closing { .. } => return Cmd::done(),
    }
    let request = Connect {
        host: settings.host.clone(),
        port: settings.port,
        client_id: settings.client_id(),
        credentials: settings.credentials.clone(),
        keep_alive_secs: KEEP_ALIVE_SECS,
        last_will: LastWill {
            topic: settings.status_topic(),
            payload: STATUS_OFFLINE.into(),
            retain: true,
        },
    };
    let (host, port) = (request.host.clone(), request.port);
    state.id = state.id.wrapping_add(1);
    // The core clears a reconnect timer that runs now.
    state.reconnect = state.reconnect.wrapping_add(1);
    mem::replace(
        &mut state.phase,
        Phase::Starting(Live::new(Session::new(&request, Limits::default()))),
    )
    .clear_timers();
    let connection = state.id;

    let open = Cmd::new(move |ctx| async move {
        let mut items = Stream::open(connection, host, port).into_stream(ctx.clone());
        while let Some(item) = items.next().await {
            let last = matches!(item, StreamItem::Closed { .. });
            ctx.send_event(Event::Stream { connection, item }.into());
            if last {
                return;
            }
        }
        // The shell ended the stream without `Closed`.
        let item = StreamItem::closed("the stream ended");
        ctx.send_event(Event::Stream { connection, item }.into());
    });
    open.and(pump(model))
}

/// Handles an item from the stream of the connection `id`. The core ignores
/// an item for an older connection.
///
/// Bytes from the broker prove that the link moves data in both
/// directions. Refer to [`wifi::traffic`]. An end of the stream by the
/// broker host proves it too. The session then ends with [`End::ByHost`],
/// and [`wifi::session_ended`] records the proof.
pub fn stream(id: u32, item: StreamItem, model: &mut Model) -> Cmd {
    let mut proof = Cmd::done();
    let input = match item {
        StreamItem::Opened => Input::Opened,
        StreamItem::Received(bytes) => {
            if id == model.connection.id && model.connection.has_stream() {
                proof = wifi::traffic(model);
            }
            Input::Received(bytes)
        }
        StreamItem::Closed { reason, by_host } => Input::Closed { reason, by_host },
    };
    proof.and(handle(id, input, model))
}

/// Handles the answer to a `Write` request on the connection `id`.
///
/// The core ignores `Failed`. A write fails only when the stream ended or
/// has no stream, and the `Closed` item of the stream ends the session. The
/// `Closed` item also tells if the broker host ended the stream, and
/// `Failed` does not. Thus the session ends in the same way if `Failed`
/// arrives before or after `Closed`. If no `Closed` item arrives, the send
/// timer of the session ends it.
pub fn written(id: u32, outcome: &Written, model: &mut Model) -> Cmd {
    match outcome {
        Written::Sent => handle(id, Input::Sent, model),
        Written::Failed(_) => Cmd::done(),
    }
}

/// Handles the expiry of a timer of the session on the connection `id`.
pub fn session_timer(id: u32, timer: Timer, model: &mut Model) -> Cmd {
    handle(id, Input::Timer(timer), model)
}

/// Handles the answer to the `Close` request on the connection `id`, and
/// clears its timer of [`CLOSE_TIMEOUT`]. The core ignores the answer if
/// no `Close` request for `id` waits.
///
/// The phase becomes `Offline`, and a reconnect timer that runs continues.
/// If a sleep waits for the close, the core sends [`power::Event::Offline`]
/// for the sleep. If the core wanted to connect while the `Close` request
/// waited, it connects now. Refer to [`connect`].
pub fn stream_closed(id: u32, model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    let after = match mem::take(&mut state.phase) {
        Phase::Draining {
            close,
            retry,
            after,
        } if close.id() == id => {
            close.clear();
            state.phase = Phase::Offline { retry };
            after
        }
        phase => {
            state.phase = phase;
            return Cmd::done();
        }
    };
    let offline = after.sleep.map_or_else(Cmd::done, |sleep| {
        Cmd::event(power::Event::Offline(sleep).into())
    });
    if after.connect {
        offline.and(go_online(model))
    } else {
        offline
    }
}

/// Handles the end of the wait for the answer to the `Close` request on the
/// connection `id`.
///
/// If the answer did not arrive, the core acts as if it
/// arrived now. The answer clears the timer. If the timer expires first,
/// the core ignores the answer. Refer to [`CLOSE_TIMEOUT`].
pub fn close_timeout(id: u32, model: &mut Model) -> Cmd {
    stream_closed(id, model)
}

/// Sends the `Close` request for the stream of the connection `id`, and
/// starts the timer of [`CLOSE_TIMEOUT`]. Returns the command and the
/// timer.
fn close_stream(id: u32, clean: bool, reason: String) -> (Cmd, Running) {
    let close = Stream::close(id, clean, reason)
        .then_send(move |()| Event::StreamClosed { connection: id }.into());
    let (timer, running) = Running::start(
        id,
        CLOSE_TIMEOUT,
        Event::CloseTimeout { connection: id }.into(),
    );
    (close.and(timer), running)
}

/// Gives `input` to the session of the connection `id`, and does the
/// actions of the session.
fn handle(id: u32, input: Input, model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    if id != state.id {
        return Cmd::done();
    }
    let Some(live) = state.phase.live_mut() else {
        return Cmd::done();
    };
    live.session.handle(input);
    pump(model)
}

/// Does the actions of the session until there are none.
///
/// - `Send` becomes a `Write` request. Its answer is [`Event::Written`].
/// - `StartTimer` starts a timer. The expiry is [`Event::SessionTimer`].
///   The timer replaces the timer of the same kind, so the core clears the
///   old timer.
/// - `CancelTimer` clears the timer of its kind. When the session ends, the
///   core clears all the timers of the session. If the expiry of a cleared
///   timer still arrives, the session ignores an old timer, and the core
///   ignores a timer of an older connection.
/// - `Connected` publishes the outbox and `online`, and subscribes, so all
///   three go in one `Write`.
/// - `Message` changes the state of an entity.
/// - `Subscribed` with `Done` or `Refused` for the subscribe of the
///   connection makes the link quiet ([`wifi::quiet`]). After `Refused`,
///   the link is up and quiet, and the core writes a line to the log.
/// - `Skipped` counts the message. The core writes a line to the log for the
///   first skipped message of each connection only, because a flood of
///   messages that are too long would fill the log.
/// - `Ended` closes the stream, and connects again if the core wants a
///   connection. The new stream opens after the answer to the `Close`
///   request.
///
/// While the connection is up, the outbox drains after the actions. The
/// command has the timers first, then the `Write`, then the other effects.
pub fn pump(model: &mut Model) -> Cmd {
    let mut timers = Vec::new();
    let mut writes = Vec::new();
    let mut rest = Vec::new();
    loop {
        while let Some(action) = model
            .connection
            .phase
            .live_mut()
            .and_then(|live| live.session.poll())
        {
            let id = model.connection.id;
            match action {
                Action::Send(bytes) => {
                    writes.push(Stream::write(id, bytes).then_send(move |outcome| {
                        Event::Written {
                            connection: id,
                            outcome,
                        }
                        .into()
                    }));
                }
                Action::StartTimer { timer, after } => {
                    let (command, handle) = start_timer(id, timer, after);
                    if let Some(live) = model.connection.phase.live_mut() {
                        live.timer_started(timer.kind, handle);
                    }
                    timers.push(command);
                }
                Action::CancelTimer(kind) => {
                    if let Some(live) = model.connection.phase.live_mut() {
                        live.cancel_timer(kind);
                    }
                }
                // The core closes the stream when the session ends.
                Action::Close => {}
                Action::Connected => rest.push(connected(model)),
                Action::Message(message) => rest.push(on_message(&message, model)),
                Action::Skipped { length } => {
                    model.connection.skipped = model.connection.skipped.wrapping_add(1);
                    // A retained flood can hold many messages that are too
                    // long, so only the first one on a connection has a line.
                    let first = model
                        .connection
                        .phase
                        .live_mut()
                        .is_some_and(|live| !mem::replace(&mut live.skip_logged, true));
                    if first {
                        rest.push(log::warn(
                            Area::Mqtt,
                            format!(
                                "a message of {length} bytes is too long, so the core skipped it"
                            ),
                        ));
                    }
                }
                Action::Subscribed { id, outcome } => rest.push(subscribed(id, outcome, model)),
                Action::Ended(end) => rest.push(ended(&end, model)),
            }
        }
        if model.connection.link() != Link::Up || model.connection.outbox.is_empty() {
            break;
        }
        let before = model.connection.outbox.len();
        rest.push(drain_outbox(model));
        if model.connection.outbox.len() == before {
            break;
        }
    }
    Cmd::all(timers).and(Cmd::all(writes)).and(Cmd::all(rest))
}

/// Starts the timer `timer` of the session on the connection `id`.
/// Returns its command and its handle.
fn start_timer(id: u32, timer: Timer, after: Duration) -> (Cmd, TimerHandle) {
    timer::notify_after(
        after,
        Event::SessionTimer {
            connection: id,
            timer,
        }
        .into(),
    )
}

/// The broker accepted the connection: publish the outbox and `online`,
/// subscribe to all the topics, and draw.
///
/// The service calls of the outbox go first, so a tap that waits for the
/// connection does not wait behind the subscribe. The subscribe after the
/// status is safe: the state messages are retained, so the broker sends the
/// newest one of each topic with the subscribe, also if the seed automation
/// publishes before it. The timer of each mark starts now. Refer to
/// [`pending`].
fn connected(model: &mut Model) -> Cmd {
    let Some(settings) = model.startup.settings() else {
        return Cmd::done();
    };
    let status = Publish {
        topic: settings.status_topic(),
        payload: STATUS_ONLINE.into(),
        qos: Qos::AtLeastOnce,
        retain: true,
    };
    if !matches!(model.connection.phase, Phase::Starting(_)) {
        return Cmd::done();
    }
    model.connection.backoff = RECONNECT_MIN;
    model.unsent = 0;
    let marks = drain_outbox(model);
    let topics = entities::topics(model.pages);
    let state = &mut model.connection;
    match mem::take(&mut state.phase) {
        Phase::Starting(mut live) => {
            // Few bytes wait after a CONNACK, so the session takes the
            // status.
            let _ = live.session.publish(status);
            let subscribe = live.session.subscribe(&topics);
            state.phase = Phase::Up { live, subscribe };
        }
        phase => state.phase = phase,
    }
    marks.and(redraw(model, false))
}

/// Handles a message from the broker on a subscribed topic.
fn on_message(message: &Message, model: &mut Model) -> Cmd {
    if model.connection.link() != Link::Up {
        return Cmd::done();
    }
    let Some(topic_entity) = entities::entity_of(&message.topic) else {
        return Cmd::done();
    };
    // A unit can change, so the check is before and after the change.
    let shown_before = shows(model, &topic_entity);
    let changed = model
        .entities
        .apply(model.pages, &message.topic, &message.payload);
    // A new exact time can confirm a tap also in the same minute.
    let confirmed = pending::confirm(&topic_entity, model);
    let Some(entity_id) = changed else {
        return confirmed;
    };
    // The draw waits for the other messages of the change. Refer to
    // [`redraw_soon`].
    if shown_before || shows(model, &entity_id) {
        confirmed.and(redraw_soon(model))
    } else {
        confirmed
    }
}

/// Handles the result of a subscribe job. After `Done` or `Refused` for the
/// subscribe of the connection, the link is quiet ([`wifi::quiet`]). After
/// `Refused`, the core also writes a line to the log. After `Failed`, the
/// connection ended or never got the acknowledgements, so the link is not
/// quiet.
fn subscribed(id: SubscribeId, outcome: SubscribeOutcome, model: &mut Model) -> Cmd {
    let current = matches!(
        model.connection.phase,
        Phase::Up { subscribe, .. } if subscribe == id
    );
    if !current || outcome == SubscribeOutcome::Failed {
        return Cmd::done();
    }
    if outcome == SubscribeOutcome::Refused {
        let reason = log::warn(
            Area::Mqtt,
            "the broker refused one or more topic filters, so some entities get no state",
        );
        return reason.and(wifi::quiet(model));
    }
    wifi::quiet(model)
}

/// The session ended: close the stream, and connect again if the core wants
/// a connection. The phase becomes `Draining`.
///
/// After a clean end, the core connects again without a wait. This happens
/// after a close for a change of access point ([`close_for_roam`]), and if
/// the device woke during the close for a sleep. The core gets the link
/// only after the answer to the `Close` request, so a `Leave` request
/// cannot stop the DISCONNECT packet. After a failure, the core waits
/// first. Refer to [`schedule_reconnect`]. In both cases, the new stream
/// opens only after the answer to the `Close` request. Refer to
/// [`connect`]. A sleep that waits for the close of the session waits for
/// the answer too. During a sleep, the core does not draw.
///
/// The Wi-Fi feature records the end. Refer to [`wifi::session_ended`].
fn ended(end: &End, model: &mut Model) -> Cmd {
    let health = wifi::session_ended(end, model);
    let wants_online = model.wants_online();
    let state = &mut model.connection;
    let (mut live, after) = match mem::take(&mut state.phase) {
        Phase::Starting(live) | Phase::Up { live, subscribe: _ } => (
            live,
            After {
                sleep: None,
                connect: true,
            },
        ),
        Phase::Closing { live, after } => (live, after),
        phase @ (Phase::Offline { .. } | Phase::Draining { .. }) => {
            state.phase = phase;
            return health;
        }
    };
    live.clear_timers();
    let clean = *end == End::Clean;
    let id = state.id;
    let (close, timer) = close_stream(id, clean, end.to_string());
    state.id = state.id.wrapping_add(1);
    state.phase = Phase::Draining {
        close: timer,
        retry: Slot::default(),
        after: After {
            sleep: after.sleep,
            // After a failure, the core waits for the backoff.
            connect: after.connect && clean && wants_online,
        },
    };
    if model.power.is_asleep() {
        return health.and(close);
    }
    let retry = if !wants_online || clean {
        Cmd::done()
    } else {
        schedule_reconnect(model)
    };
    health.and(close).and(redraw(model, false)).and(retry)
}

/// Returns `true` if the current page shows the entity `entity_id`. On a
/// room page, the tiles of the room show entities. On the index page, the
/// header and the summaries show entities. Refer to
/// [`index_shows`](crate::summary::index_shows).
pub(crate) fn shows(model: &Model, entity_id: &str) -> bool {
    model.current_page().is_some_and(|page| {
        if page.is_home() {
            summary::index_shows(model.pages, &model.entities, entity_id)
        } else {
            page.tiles.iter().any(|tile| tile.entity_id == entity_id)
        }
    })
}

/// Returns the footer time of the current page: the latest `last_updated`
/// of the entities that the page shows.
///
/// The times come from Home Assistant, not from the clock of the device.
/// Refer to [`entities`] for the reason that the core uses `last_updated`.
/// If no entity on the page has a time, the footer time is
/// [`Model::last_sync`], the time that the shell kept through a deep sleep.
/// Each page has its own time, so a change on another page does not change
/// the time of this page.
#[must_use]
pub fn page_time(model: &Model) -> Option<DateTime<Utc>> {
    model
        .entities
        .last_updated_minutes()
        .filter(|(entity_id, _)| shows(model, entity_id))
        .map(|(_, time)| time)
        .max()
        .or(model.last_sync)
}

/// Waits, then tries to connect again. Each wait is double the wait before
/// it, up to [`RECONNECT_MAX`]. The command writes the wait to the log, in
/// whole seconds, before the timer starts. Both limits are whole seconds,
/// so each wait is. With a stream, the core does nothing.
fn schedule_reconnect(model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    let Some(retry) = state.phase.retry_mut() else {
        return Cmd::done();
    };
    state.reconnect = state.reconnect.wrapping_add(1);
    let id = state.reconnect;
    let wait = state.backoff;
    state.backoff = (wait * 2).min(RECONNECT_MAX);
    let timer = retry.start(id, wait, Event::Reconnect(id).into());
    let reason = log::info(Area::Mqtt, format!("connect again in {} s", wait.as_secs()));
    reason.and(timer)
}

#[cfg(test)]
mod tests;
