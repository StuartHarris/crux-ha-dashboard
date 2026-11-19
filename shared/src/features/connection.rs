//! The connection feature: Wi-Fi, the MQTT connection, reconnects, and the
//! messages from the Home Assistant state stream.

use alloc::vec::Vec;
use core::time::Duration;

use chrono::{DateTime, Utc};
use crux_time::clock::Time;
use futures::StreamExt;

use crate::{
    Cmd, Event, Model,
    capabilities::{
        mqtt::{Connect, LastWill, Mqtt, MqttEvent, Publish, Qos},
        network::{Network, WifiStatus},
    },
    entities,
    features::refresh::{redraw, redraw_soon},
    summary,
};

/// The first wait before the core connects again after a failure.
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
/// The longest wait before the core connects again after a failure.
pub const RECONNECT_MAX: Duration = Duration::from_secs(60);
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
    /// The state of the MQTT connection.
    pub mqtt: Link,
    /// The id of the newest MQTT connection.
    pub id: u32,
    /// The id of the newest reconnect timer.
    pub reconnect: u32,
    /// The wait before the next reconnect.
    pub backoff: Duration,
    /// The service calls that wait for a connection.
    pub outbox: Vec<Publish>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            wifi: Link::Down,
            wifi_request: 0,
            mqtt: Link::Down,
            id: 0,
            reconnect: 0,
            backoff: RECONNECT_MIN,
            outbox: Vec::new(),
        }
    }
}

/// Forgets the links after the shell starts. The core ignores the answers to
/// earlier requests.
pub const fn reset(model: &mut Model) {
    let state = &mut model.connection;
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

/// Publishes `asleep` on the status topic, if the connection is up, and
/// forgets the links. The caller then sends `Disconnect` and `WifiOff`, one
/// after the other.
pub fn go_offline(model: &mut Model) -> Cmd {
    let state = &mut model.connection;
    state.outbox.clear();
    // The core ignores the answers to earlier requests.
    state.reconnect = state.reconnect.wrapping_add(1);
    state.id = state.id.wrapping_add(1);
    state.wifi_request = state.wifi_request.wrapping_add(1);

    let command = match &model.settings {
        Some(settings) if state.mqtt == Link::Up => Mqtt::publish(Publish {
            topic: settings.status_topic(),
            payload: STATUS_ASLEEP.into(),
            qos: Qos::AtLeastOnce,
            retain: true,
        }),
        _ => Cmd::done(),
    };
    state.mqtt = Link::Down;
    state.wifi = Link::Down;
    command
}

/// Publishes a message now, or keeps it until the core connects.
pub fn send(publish: Publish, model: &mut Model) -> Cmd {
    if model.connection.mqtt == Link::Up {
        return Mqtt::publish(publish);
    }
    let outbox = &mut model.connection.outbox;
    if outbox.len() == OUTBOX_LIMIT {
        outbox.remove(0);
    }
    outbox.push(publish);
    go_online(model)
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

/// Opens an MQTT connection, if there is none.
fn connect(model: &mut Model) -> Cmd {
    let Some(settings) = &model.settings else {
        return Cmd::done();
    };
    let state = &mut model.connection;
    if state.mqtt != Link::Down {
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
    state.mqtt = Link::Starting;
    state.id = state.id.wrapping_add(1);
    // The core ignores a reconnect timer that runs now.
    state.reconnect = state.reconnect.wrapping_add(1);
    let connection = state.id;

    Cmd::new(move |ctx| async move {
        let mut events = Mqtt::connect(request).into_stream(ctx.clone());
        while let Some(event) = events.next().await {
            let last = matches!(event, MqttEvent::Disconnected { .. });
            ctx.send_event(Event::Mqtt { connection, event });
            if last {
                return;
            }
        }
        // The shell ended the stream without `Disconnected`.
        let event = MqttEvent::Disconnected { reason: None };
        ctx.send_event(Event::Mqtt { connection, event });
    })
}

/// Handles an item from the stream of the connection `id`.
pub fn mqtt(id: u32, event: MqttEvent, model: &mut Model) -> Cmd {
    if id != model.connection.id {
        return Cmd::done();
    }
    match event {
        MqttEvent::Connected => connected(model),
        MqttEvent::Message(_) if model.connection.mqtt != Link::Up => Cmd::done(),
        MqttEvent::Message(message) => {
            // A unit can change, so the check is before and after the change.
            let shown_before = entities::entity_of(&message.topic)
                .is_some_and(|entity_id| shows(model, &entity_id));
            let Some(entity_id) =
                model
                    .entities
                    .apply(model.pages, &message.topic, &message.payload)
            else {
                return Cmd::done();
            };
            // The draw waits for the other messages of the change. Refer to
            // [`redraw_soon`].
            if shown_before || shows(model, &entity_id) {
                redraw_soon(model)
            } else {
                Cmd::done()
            }
        }
        MqttEvent::Disconnected { .. } => {
            model.connection.mqtt = Link::Down;
            // Wi-Fi can be the cause, so the reconnect asks for Wi-Fi again.
            // If Wi-Fi is on, the shell answers at once.
            model.connection.wifi = Link::Down;
            model.connection.wifi_request = model.connection.wifi_request.wrapping_add(1);
            let retry = if model.wants_online() {
                schedule_reconnect(model)
            } else {
                Cmd::done()
            };
            redraw(model, false).and(retry)
        }
    }
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
    model.connection.mqtt = Link::Up;
    model.connection.backoff = RECONNECT_MIN;
    model.unsent = 0;

    // The service calls of the outbox go first, so a tap that waits for the
    // connection does not wait behind the subscribe (one packet for each
    // topic). The subscribe after the status is safe: the state messages are
    // retained, so the broker sends the newest one of each topic with the
    // subscribe, also if the seed automation publishes before it.
    let mut commands: Vec<Cmd> = model
        .connection
        .outbox
        .drain(..)
        .map(Mqtt::publish)
        .collect();
    commands.push(Mqtt::publish(status));
    commands.push(Mqtt::subscribe(entities::topics(model.pages)));
    commands.push(redraw(model, false));
    Cmd::all(commands)
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
