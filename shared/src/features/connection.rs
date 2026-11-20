//! The connection feature: Wi-Fi, the MQTT session, reconnects, and the
//! messages from the Home Assistant state stream.
//!
//! The core runs the MQTT session ([`Session`]), and the shell gives it only
//! a byte stream. Refer to [`crate::capabilities::stream`]. Each wait of the
//! protocol is a state of the session with a timer, so a link that stops
//! cannot make the core wait without a limit. [`pump`] does the actions of
//! the session: it writes the bytes, starts the timers and handles the
//! results.
//!
//! After each connect, the core subscribes to all the topics. When the
//! broker acknowledged each topic filter, the link is quiet, and the core
//! sends `WifiQuiet`. The shell can then look for a better access point. A
//! change of access point can drop the connection, and the core connects
//! again.

use alloc::{
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
        network::{Network, WifiStatus},
        stream::{Stream, StreamItem, Written},
    },
    entities,
    features::{
        pending,
        refresh::{redraw, redraw_soon},
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

/// The state of a link (Wi-Fi or the MQTT connection).
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

/// The state of Wi-Fi and of the MQTT connection.
#[derive(Debug)]
pub struct State {
    /// The state of Wi-Fi.
    pub wifi: Link,
    /// The id of the newest `WifiOn` request.
    pub wifi_request: u32,
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
            wifi: Link::Down,
            wifi_request: 0,
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

/// Forgets the links after the shell starts. The core ignores the answers to
/// earlier requests.
pub fn reset(model: &mut Model) {
    let state = &mut model.connection;
    state.session = None;
    state.closing = None;
    state.draining = None;
    state.connect_waits = false;
    state.wifi = Link::Down;
    state.mqtt = Link::Down;
    state.id = state.id.wrapping_add(1);
    state.wifi_request = state.wifi_request.wrapping_add(1);
    state.reconnect = state.reconnect.wrapping_add(1);
    state.backoff = RECONNECT_MIN;
}

/// Starts a new online cycle after a wake or a move to the stand. The next
/// failure waits the shortest time again.
pub const fn restart(model: &mut Model) {
    model.connection.backoff = RECONNECT_MIN;
}

/// Turns Wi-Fi on and connects, if the core wants a connection.
pub fn go_online(model: &mut Model) -> Cmd {
    if !model.wants_online() {
        return Cmd::done();
    }
    match model.connection.wifi {
        Link::Down => {
            let state = &mut model.connection;
            state.wifi = Link::Starting;
            state.wifi_request = state.wifi_request.wrapping_add(1);
            let request = state.wifi_request;
            Network::wifi_on().then_send(move |status| Event::Wifi { request, status })
        }
        Link::Starting => Cmd::done(),
        Link::Up => connect(model),
    }
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
    state.wifi_request = state.wifi_request.wrapping_add(1);
    let up = state.mqtt == Link::Up;
    state.mqtt = Link::Down;
    state.wifi = Link::Down;
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
/// A call that is too long for a packet can never go. The core drops it and
/// removes its mark.
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
            pending::forget_dropped(model);
            Ok(Cmd::done())
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

/// Handles the answer to the `WifiOn` request with the id `request`. The
/// core ignores the answer to an older request.
pub fn wifi(request: u32, status: WifiStatus, model: &mut Model) -> Cmd {
    if request != model.connection.wifi_request {
        return Cmd::done();
    }
    match status {
        WifiStatus::Connected if model.wants_online() => {
            model.connection.wifi = Link::Up;
            connect(model)
        }
        WifiStatus::Connected => {
            // The device went to sleep while Wi-Fi started. The steps of the
            // sleep turn Wi-Fi off.
            model.connection.wifi = Link::Down;
            Cmd::done()
        }
        WifiStatus::Failed => {
            model.connection.wifi = Link::Down;
            if model.wants_online() {
                schedule_reconnect(model)
            } else {
                Cmd::done()
            }
        }
    }
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
            let last = matches!(item, StreamItem::Closed(_));
            ctx.send_event(Event::Stream { connection, item });
            if last {
                return;
            }
        }
        // The shell ended the stream without `Closed`.
        let item = StreamItem::Closed("the stream ended".to_string());
        ctx.send_event(Event::Stream { connection, item });
    });
    open.and(pump(model))
}

/// Handles an item from the stream of the connection `id`. The core ignores
/// an item for an older connection.
pub fn stream(id: u32, item: StreamItem, model: &mut Model) -> Cmd {
    let input = match item {
        StreamItem::Opened => Input::Opened,
        StreamItem::Received(bytes) => Input::Received(bytes),
        StreamItem::Closed(reason) => Input::Closed(reason),
    };
    handle(id, input, model)
}

/// Handles the answer to a `Write` request on the connection `id`. A write
/// that failed ends the session, as a closed stream does.
pub fn written(id: u32, outcome: Written, model: &mut Model) -> Cmd {
    let input = match outcome {
        Written::Sent => Input::Sent,
        Written::Failed(reason) => Input::Closed(reason),
    };
    handle(id, input, model)
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
///   connection sends `WifiQuiet`. After `Refused`, the link is up and
///   quiet, but the core has no effect to report the refused filter.
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
                Action::Skipped { .. } => {
                    model.connection.skipped = model.connection.skipped.wrapping_add(1);
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
/// subscribe of the connection, the link is quiet, so the core sends
/// `WifiQuiet`. After `Failed`, the connection ended or never got the
/// acknowledgements, so the core sends nothing.
fn subscribed(id: SubscribeId, outcome: SubscribeOutcome, model: &Model) -> Cmd {
    let state = &model.connection;
    let current = state.session.as_ref().and_then(|live| live.subscribe);
    if current != Some(id) || state.mqtt != Link::Up || outcome == SubscribeOutcome::Failed {
        return Cmd::done();
    }
    Network::quiet()
}

/// The session ended: close the stream, and connect again if the core wants
/// a connection.
///
/// After a clean end, the core connects again without a wait. This happens
/// if the device woke during the close for a sleep. After a failure, the
/// core waits first. Refer to [`schedule_reconnect`]. In both cases, the new
/// stream opens only after the answer to the `Close` request. Refer to
/// [`connect`]. During a sleep, the core does not draw.
fn ended(end: &End, model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    state.session = None;
    let id = state.id;
    state.draining = Some(id);
    state.connect_waits = false;
    let close = close_stream(id, *end == End::Clean, end.to_string());
    state.id = state.id.wrapping_add(1);
    state.mqtt = Link::Down;
    // Wi-Fi can be the cause, so the reconnect asks for Wi-Fi again. If
    // Wi-Fi is on, the shell answers at once.
    state.wifi = Link::Down;
    state.wifi_request = state.wifi_request.wrapping_add(1);
    if model.power.asleep {
        return close;
    }
    let retry = match end {
        _ if !model.wants_online() => Cmd::done(),
        End::Clean => go_online(model),
        _ => schedule_reconnect(model),
    };
    close.and(redraw(model, false)).and(retry)
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
/// it, up to [`RECONNECT_MAX`].
fn schedule_reconnect(model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    state.reconnect = state.reconnect.wrapping_add(1);
    let id = state.reconnect;
    let wait = state.backoff;
    state.backoff = (wait * 2).min(RECONNECT_MAX);
    let (timer, _handle) = Time::notify_after(wait);
    timer.then_send(move |_| Event::Reconnect(id))
}
