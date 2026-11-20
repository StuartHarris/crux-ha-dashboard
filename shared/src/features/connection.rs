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

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use chrono::{DateTime, Utc};
use crux_time::clock::Time;
use futures::StreamExt;

use crate::{
    Cmd, Event, Model,
    capabilities::{
        log::{self, Area},
        stream::{Stream, StreamItem, Written},
    },
    entities,
    features::{
        pending,
        refresh::{redraw, redraw_soon},
        wifi,
    },
    mqtt::{
        Action, Connect, End, Input, LastWill, Limits, Message, Publish, Qos, Refused, Session,
        SubscribeId, SubscribeOutcome, Timer,
    },
    summary,
};

/// The first wait before the core connects again after a failure.
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
/// The longest wait before the core connects again after a failure.
pub const RECONNECT_MAX: Duration = Duration::from_secs(60);
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

/// The state of the MQTT connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
    /// The link is down.
    #[default]
    Down,
    /// The core asked for the link and waits for the answer.
    Starting,
    /// The link is up.
    Up,
}

/// The state of the MQTT connection.
#[derive(Debug)]
pub struct State {
    /// The state of the MQTT connection, as the view shows it.
    pub mqtt: Link,
    /// The id of the newest MQTT connection. Each stream operation carries
    /// it, and the core ignores the answers for an older connection.
    pub id: u32,
    /// The session of the connection, until the session ends.
    pub session: Option<Live>,
    /// The close of a stream for a sleep: the id of the connection and the
    /// id of the sleep. The answer to the `Close` request starts the next
    /// step of the sleep. Refer to [`go_offline`].
    pub closing: Option<(u32, u32)>,
    /// The id of the connection of a session that ended, while the `Close`
    /// request for its stream waits for its answer. Until the answer
    /// arrives, the core does not open a new stream. Refer to [`connect`].
    pub draining: Option<u32>,
    /// The core wanted to connect while the `Close` request of
    /// [`State::draining`] waited. The core connects when the answer
    /// arrives.
    pub connect_waits: bool,
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

/// The session of an open connection.
#[derive(Debug)]
pub struct Live {
    /// The MQTT session.
    pub session: Session,
    /// The subscribe job of the connection, after the CONNACK.
    pub subscribe: Option<SubscribeId>,
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
            mqtt: Link::Down,
            id: 0,
            session: None,
            closing: None,
            draining: None,
            connect_waits: false,
            skipped: 0,
            reconnect: 0,
            backoff: RECONNECT_MIN,
            outbox: Vec::new(),
        }
    }
}

/// Forgets the connection and the radio after the shell starts. The core
/// ignores the answers to earlier requests.
pub fn reset(model: &mut Model) {
    wifi::reset(model);
    let state = &mut model.connection;
    state.session = None;
    state.closing = None;
    state.draining = None;
    state.connect_waits = false;
    state.mqtt = Link::Down;
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
/// session. Refer to [`wifi::link`].
///
/// While the `Close` request of an old stream waits for its answer, the
/// core does not ask for the link yet. A radio request, for example a
/// `Leave` for a change of access point, can stop the old stream before the
/// DISCONNECT packet goes. The answer to the `Close` request continues.
/// Refer to [`stream_closed`].
pub fn go_online(model: &mut Model) -> Cmd {
    let wants_online = model.wants_online();
    let state = &mut model.connection;
    if !wants_online || state.mqtt != Link::Down || state.session.is_some() {
        return Cmd::done();
    }
    if state.draining.is_some() {
        state.connect_waits = true;
        return Cmd::done();
    }
    wifi::link(model)
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
/// again, if the core wants a connection.
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
/// last will. The core connects again after the answer to the `Close`
/// request. Refer to [`ended`]. With no session that is up, the next
/// connect goes to the better access point, so the core does nothing now.
pub fn close_for_roam(model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    if state.mqtt != Link::Up {
        return Cmd::done();
    }
    let Some(live) = &mut state.session else {
        return Cmd::done();
    };
    live.session.disconnect();
    pump(model)
}

/// Closes the connection for the sleep `sleep`. The answer to the `Close`
/// request starts the next step of the sleep ([`Event::Offline`]).
///
/// If the connection is up, the core publishes `asleep` on the status topic
/// and then sends a DISCONNECT packet, so the broker does not publish the
/// last will. A session that is not up ends at once. With no session, the
/// core sends `Close` for the current id, and the shell answers at once.
///
/// The core drops the service calls in the outbox and removes their marks.
/// The device sleeps, so the core does not draw.
pub fn go_offline(sleep: u32, model: &mut Model) -> Cmd {
    model.connection.outbox.clear();
    pending::forget_dropped(model);
    let status = model.settings.as_ref().map(|settings| Publish {
        topic: settings.status_topic(),
        payload: STATUS_ASLEEP.into(),
        qos: Qos::AtLeastOnce,
        retain: true,
    });
    let state = &mut model.connection;
    // The core ignores the answers to earlier requests.
    state.reconnect = state.reconnect.wrapping_add(1);
    let up = state.mqtt == Link::Up;
    state.mqtt = Link::Down;
    let id = state.id;
    state.closing = Some((id, sleep));
    let Some(live) = &mut state.session else {
        return close_stream(id, true, STATUS_ASLEEP.to_string());
    };
    if up && let Some(status) = status {
        // A publish that the session refuses cannot stop the sleep, so the
        // core drops it.
        let _ = live.session.publish(status);
    }
    live.session.disconnect();
    pump(model)
}

/// Publishes a service call now, or keeps it until the core connects.
///
/// When the core issues the call, the timeout of the mark of its entity
/// starts. While the call waits in the outbox, the timeout of the outbox
/// runs. If the outbox is full, the core drops the oldest call and removes
/// its mark. Refer to [`pending`].
///
/// If the session cannot take the call now, the call goes first in the
/// outbox. The outbox drains at the end of each [`pump`].
pub fn send(call: Call, model: &mut Model) -> Cmd {
    if model.connection.mqtt == Link::Up {
        return match issue(call, model) {
            Ok(mark) => pump(model).and(mark),
            Err(call) => {
                let entity_id = call.entity_id;
                model.connection.outbox.insert(0, call);
                pending::queued(entity_id, model)
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
    pending::queued(entity_id, model).and(go_online(model))
}

/// Gives a service call to the session and starts the timer of its mark.
/// Returns the command of the timer.
///
/// A call that is too long for a packet can never go. The core drops it,
/// removes its mark, and writes a line to the log. The command that the
/// function returns holds that line.
///
/// # Errors
///
/// Returns the call if the session cannot take it now.
fn issue(call: Call, model: &mut Model) -> Result<Cmd, Call> {
    let Some(live) = &mut model.connection.session else {
        return Err(call);
    };
    match live.session.publish(call.publish.clone()) {
        Ok(()) => Ok(pending::issued(call.entity_id, model)),
        Err(Refused::TooLong) => {
            let reason = log::warn(
                Area::Mqtt,
                format!(
                    "the service call for {} is too long for a packet, so the core dropped it",
                    call.entity_id
                ),
            );
            pending::forget_dropped(model);
            Ok(reason)
        }
        Err(Refused::Full | Refused::NotConnected) => Err(call),
    }
}

/// Issues the service calls of the outbox, in order, until the session
/// cannot take more. Returns the commands of the timers of their marks.
fn drain_outbox(model: &mut Model) -> Cmd {
    let mut commands = Vec::new();
    while !model.connection.outbox.is_empty() {
        let call = model.connection.outbox.remove(0);
        match issue(call, model) {
            Ok(command) => commands.push(command),
            Err(call) => {
                model.connection.outbox.insert(0, call);
                break;
            }
        }
    }
    Cmd::all(commands)
}

/// Tries to connect again after the wait for the reconnect `id`.
pub fn reconnect(id: u32, model: &mut Model) -> Cmd {
    if id == model.connection.reconnect {
        go_online(model)
    } else {
        Cmd::done()
    }
}

/// Opens a stream and starts a session, if there is none.
///
/// A session that closes for a sleep is still there after a wake during the
/// close. The core connects again when that session ends.
///
/// After a session ends, the core does not open a stream until the shell
/// answers the `Close` request for the old stream. A new `Open` makes the
/// shell abort the old stream. An abort can send a TCP reset, and then the
/// broker can lose bytes that it did not read, for example the DISCONNECT.
/// Without the DISCONNECT, the broker publishes the last will. Refer to
/// [`stream_closed`].
fn connect(model: &mut Model) -> Cmd {
    let Some(settings) = &model.settings else {
        return Cmd::done();
    };
    let state = &mut model.connection;
    if state.mqtt != Link::Down || state.session.is_some() {
        return Cmd::done();
    }
    if state.draining.is_some() {
        state.connect_waits = true;
        return Cmd::done();
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
    state.mqtt = Link::Starting;
    state.id = state.id.wrapping_add(1);
    // The core ignores a reconnect timer that runs now.
    state.reconnect = state.reconnect.wrapping_add(1);
    state.session = Some(Live {
        session: Session::new(&request, Limits::default()),
        subscribe: None,
    });
    let connection = state.id;

    let open = Cmd::new(move |ctx| async move {
        let mut items = Stream::open(connection, host, port).into_stream(ctx.clone());
        while let Some(item) = items.next().await {
            let last = matches!(item, StreamItem::Closed { .. });
            ctx.send_event(Event::Stream { connection, item });
            if last {
                return;
            }
        }
        // The shell ended the stream without `Closed`.
        let item = StreamItem::closed("the stream ended");
        ctx.send_event(Event::Stream { connection, item });
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
            if id == model.connection.id && model.connection.session.is_some() {
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

/// Handles the answer to the `Close` request on the connection `id`.
///
/// If the core closed the stream for a sleep, it sends [`Event::Offline`]
/// for the sleep. If the core wanted to connect while the `Close` request
/// waited, it connects now. Refer to [`connect`].
pub fn stream_closed(id: u32, model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    let offline = match state.closing {
        Some((connection, sleep)) if connection == id => {
            state.closing = None;
            Cmd::event(Event::Offline(sleep))
        }
        _ => Cmd::done(),
    };
    if state.draining != Some(id) {
        return offline;
    }
    state.draining = None;
    if !core::mem::take(&mut state.connect_waits) {
        return offline;
    }
    offline.and(go_online(model))
}

/// Handles the end of the wait for the answer to the `Close` request on the
/// connection `id`.
///
/// If the answer did not arrive, the core acts as if it
/// arrived now. After the answer, the core ignores the timer, and after the
/// timer, the core ignores the answer. Refer to [`CLOSE_TIMEOUT`].
pub fn close_timeout(id: u32, model: &mut Model) -> Cmd {
    stream_closed(id, model)
}

/// Sends the `Close` request for the stream of the connection `id`, and
/// starts the timer of [`CLOSE_TIMEOUT`].
fn close_stream(id: u32, clean: bool, reason: String) -> Cmd {
    let close = Stream::close(id, clean, reason)
        .then_send(move |()| Event::StreamClosed { connection: id });
    let (timer, _handle) = Time::notify_after(CLOSE_TIMEOUT);
    close.and(timer.then_send(move |_| Event::CloseTimeout { connection: id }))
}

/// Gives `input` to the session of the connection `id`, and does the
/// actions of the session.
fn handle(id: u32, input: Input, model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    if id != state.id {
        return Cmd::done();
    }
    let Some(live) = &mut state.session else {
        return Cmd::done();
    };
    live.session.handle(input);
    pump(model)
}

/// Does the actions of the session until there are none.
///
/// - `Send` becomes a `Write` request. Its answer is [`Event::Written`].
/// - `StartTimer` starts a timer. The expiry is [`Event::SessionTimer`].
///   The core does not clear a timer that the session no longer uses. The
///   session ignores an old timer, and the core ignores a timer of an older
///   connection. Thus the shell gets no `TimeClear` for the session.
/// - `Connected` publishes the outbox and `online`, and subscribes, so all
///   three go in one `Write`.
/// - `Message` changes the state of an entity.
/// - `Subscribed` with `Done` or `Refused` for the subscribe of the
///   connection makes the link quiet ([`wifi::quiet`]). After `Refused`,
///   the link is up and quiet, and the core writes a line to the log.
/// - `Skipped` counts the message and writes a line to the log.
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
            .session
            .as_mut()
            .and_then(|live| live.session.poll())
        {
            let id = model.connection.id;
            match action {
                Action::Send(bytes) => writes.push(Stream::write(id, bytes).then_send(
                    move |outcome| Event::Written {
                        connection: id,
                        outcome,
                    },
                )),
                Action::StartTimer { timer, after } => timers.push(start_timer(id, timer, after)),
                // The core closes the stream when the session ends.
                Action::Close => {}
                Action::Connected => rest.push(connected(model)),
                Action::Message(message) => rest.push(on_message(&message, model)),
                Action::Skipped { length } => {
                    model.connection.skipped = model.connection.skipped.wrapping_add(1);
                    rest.push(log::warn(
                        Area::Mqtt,
                        format!("a message of {length} bytes is too long, so the core skipped it"),
                    ));
                }
                Action::Subscribed { id, outcome } => rest.push(subscribed(id, outcome, model)),
                Action::Ended(end) => rest.push(ended(&end, model)),
            }
        }
        if model.connection.mqtt != Link::Up || model.connection.outbox.is_empty() {
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
fn start_timer(id: u32, timer: Timer, after: Duration) -> Cmd {
    let (builder, _handle) = Time::notify_after(after);
    builder.then_send(move |_| Event::SessionTimer {
        connection: id,
        timer,
    })
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
    let Some(settings) = &model.settings else {
        return Cmd::done();
    };
    let status = Publish {
        topic: settings.status_topic(),
        payload: STATUS_ONLINE.into(),
        qos: Qos::AtLeastOnce,
        retain: true,
    };
    let state = &mut model.connection;
    state.mqtt = Link::Up;
    state.backoff = RECONNECT_MIN;
    model.unsent = 0;
    let marks = drain_outbox(model);
    let topics = entities::topics(model.pages);
    if let Some(live) = &mut model.connection.session {
        // Few bytes wait after a CONNACK, so the session takes the status.
        let _ = live.session.publish(status);
        live.subscribe = Some(live.session.subscribe(&topics));
    }
    marks.and(redraw(model, false))
}

/// Handles a message from the broker on a subscribed topic.
fn on_message(message: &Message, model: &mut Model) -> Cmd {
    if model.connection.mqtt != Link::Up {
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
    let state = &model.connection;
    let current = state.session.as_ref().and_then(|live| live.subscribe);
    if current != Some(id) || state.mqtt != Link::Up || outcome == SubscribeOutcome::Failed {
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
/// a connection.
///
/// After a clean end, the core connects again without a wait. This happens
/// after a close for a change of access point ([`close_for_roam`]), and if
/// the device woke during the close for a sleep. The core gets the link
/// only after the answer to the `Close` request, so a `Leave` request
/// cannot stop the DISCONNECT packet. After a failure, the core waits
/// first. Refer to [`schedule_reconnect`]. In both cases, the new stream
/// opens only after the answer to the `Close` request. Refer to
/// [`connect`]. During a sleep, the core does not draw.
///
/// The Wi-Fi feature records the end. Refer to [`wifi::session_ended`].
fn ended(end: &End, model: &mut Model) -> Cmd {
    let health = wifi::session_ended(end, model);
    let state = &mut model.connection;
    state.session = None;
    let id = state.id;
    state.draining = Some(id);
    state.connect_waits = false;
    let close = close_stream(id, *end == End::Clean, end.to_string());
    state.id = state.id.wrapping_add(1);
    state.mqtt = Link::Down;
    if model.power.asleep {
        return health.and(close);
    }
    let retry = match end {
        _ if !model.wants_online() => Cmd::done(),
        End::Clean => {
            // The answer to the `Close` request connects. Refer to
            // [`stream_closed`].
            model.connection.connect_waits = true;
            Cmd::done()
        }
        _ => schedule_reconnect(model),
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
        .last_updated()
        .filter(|(entity_id, _)| shows(model, entity_id))
        .map(|(_, time)| time)
        .max()
        .or(model.last_sync)
}

/// Waits, then tries to connect again. Each wait is double the wait before
/// it, up to [`RECONNECT_MAX`]. The command writes the wait to the log, in
/// whole seconds, before the timer starts.
fn schedule_reconnect(model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    state.reconnect = state.reconnect.wrapping_add(1);
    let id = state.reconnect;
    let wait = state.backoff;
    state.backoff = (wait * 2).min(RECONNECT_MAX);
    let (timer, _handle) = Time::notify_after(wait);
    let reason = log::info(Area::Mqtt, format!("connect again in {} s", wait.as_secs()));
    reason.and(timer.then_send(move |_| Event::Reconnect(id)))
}
