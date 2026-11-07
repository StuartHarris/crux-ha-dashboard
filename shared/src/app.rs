//! The root of the app: the events, the effects, the model and the dispatch
//! to the features.

use alloc::{format, string::String};

use chrono::{DateTime, Utc};
use crux_core::{
    App, Command,
    command::RequestBuilder,
    macros::effect,
    render::{RenderOperation, render},
};
use crux_time::{Instant, clock::Time, operation as time};
use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::{
    capabilities::{
        mqtt::{Connect, Credentials, Disconnect, MqttEvent, Publish, Subscribe},
        network::{WifiOff, WifiOn, WifiStatus},
        power::{PowerSource, Sleep},
    },
    entities::Entities,
    features::{connection, power, rooms},
    layout::Page,
    pages::PAGES,
    route::Route,
    view::{self, ViewModel},
};

/// The broker and device settings, which the shell supplies.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The id of this dashboard in MQTT topics, for example `hall`.
    pub device_id: String,
    /// The host name or IP address of the broker.
    pub host: String,
    /// The TCP port of the broker.
    pub port: u16,
    /// The credentials, if the broker needs them.
    pub credentials: Option<Credentials>,
}

impl Settings {
    /// Returns the topic on which the dashboard publishes service calls.
    #[must_use]
    pub fn command_topic(&self) -> String {
        format!("dashboard/{}/cmd", self.device_id)
    }

    /// Returns the retained topic that tells if the dashboard is online.
    #[must_use]
    pub fn status_topic(&self) -> String {
        format!("dashboard/{}/status", self.device_id)
    }

    /// Returns the MQTT client id.
    #[must_use]
    pub fn client_id(&self) -> String {
        format!("dashboard-{}", self.device_id)
    }
}

/// The facts that the shell sends when it starts the core.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Start {
    /// The broker and device settings.
    pub settings: Settings,
    /// The power source now.
    pub power: PowerSource,
    /// The path of the page to show, from [`ViewModel::path`]. After a deep
    /// sleep, the shell gives the path that it kept in RTC memory. If the
    /// path is `None` or not valid, the core shows the first page.
    pub path: Option<String>,
    /// The local offset from UTC in minutes, for example 60 for BST.
    pub utc_offset_minutes: i16,
    /// The time of the last update, in seconds since the Unix epoch, from
    /// [`ViewModel::last_sync`]. After a deep sleep, the shell gives the
    /// value that it kept in RTC memory.
    pub last_sync: Option<u64>,
}

/// The events that the shells send to the core.
///
/// The public variants are facts from the shell and intents from the user.
/// The skipped variants are internal. The core makes them from the answers to
/// its effects.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Event {
    /// The shell started. On battery, the device also starts after a deep
    /// sleep.
    Start(Start),
    /// The device woke from a light sleep, which kept the core in memory.
    Woke,
    /// The device moved to the stand or off the stand.
    PowerSourceChanged(PowerSource),
    /// The user tapped the tile with this index on the current page.
    TileTapped(usize),
    /// The user tapped the scene with this index on the current page.
    SceneTapped(usize),
    /// The user asked for the next page.
    NextPage,
    /// The user asked for the previous page.
    PreviousPage,
    /// The user asked for a page by its route.
    Navigate(Route),
    /// The local offset from UTC changed, for example at the start of summer
    /// time. The value is in minutes.
    UtcOffsetChanged(i16),

    /// The answer to a `WifiOn` request.
    #[serde(skip)]
    #[facet(skip)]
    Wifi {
        /// The id of the request. The core ignores the answer to an older
        /// request.
        request: u32,
        /// The answer.
        status: WifiStatus,
    },
    /// An item from the stream of the MQTT connection with this id.
    #[serde(skip)]
    #[facet(skip)]
    Mqtt {
        /// The id of the connection. The core ignores an item from an older
        /// connection.
        connection: u32,
        /// The item from the stream.
        event: MqttEvent,
    },
    /// The connection stopped at this time. The time is `None` if the shell
    /// gave a time that is not valid.
    #[serde(skip)]
    #[facet(skip)]
    Synced(#[facet(opaque)] Option<DateTime<Utc>>),
    /// The wait before the reconnect with this id is over.
    #[serde(skip)]
    #[facet(skip)]
    Reconnect(u32),
    /// The idle timer with this id expired.
    #[serde(skip)]
    #[facet(skip)]
    IdleTimeout(u32),
    /// The core read the clock before it sleeps, after the idle timer with
    /// this id expired.
    #[serde(skip)]
    #[facet(skip)]
    ReadyToSleep {
        /// The id of the idle timer.
        idle: u32,
        /// The time now, or `None` if the shell gave a time that is not
        /// valid.
        #[facet(opaque)]
        now: Option<DateTime<Utc>>,
    },
}

/// The effects that the core asks the shell to do.
#[effect]
#[derive(Debug)]
pub enum Effect {
    /// Draw the view model.
    Render(RenderOperation),
    /// Open an MQTT connection.
    MqttConnect(Connect),
    /// Subscribe to MQTT topics.
    MqttSubscribe(Subscribe),
    /// Publish an MQTT message.
    MqttPublish(Publish),
    /// Close the MQTT connection.
    MqttDisconnect(Disconnect),
    /// Turn Wi-Fi on.
    WifiOn(WifiOn),
    /// Turn Wi-Fi off.
    WifiOff(WifiOff),
    /// Put the device to sleep.
    Sleep(Sleep),
    /// Read the wall clock.
    TimeNow(time::Now),
    /// Start a timer.
    TimeNotifyAfter(time::NotifyAfter),
    /// Clear a timer.
    TimeClear(time::ClearTimer),
}

/// The state of the dashboard.
#[derive(Debug)]
pub struct Model {
    /// The pages of the dashboard.
    pub pages: &'static [Page],
    /// The broker and device settings. The core has none until `Start`.
    pub settings: Option<Settings>,
    /// The current route.
    pub route: Route,
    /// The entity states from the state stream.
    pub entities: Entities,
    /// The state of Wi-Fi and of the MQTT connection.
    pub connection: connection::State,
    /// The power source and the sleep state.
    pub power: power::State,
    /// The time at which the last connection stopped.
    pub last_sync: Option<DateTime<Utc>>,
    /// The local offset from UTC in minutes.
    pub utc_offset_minutes: i16,
    /// The number of service calls that the core dropped, because it slept
    /// before it could send them.
    pub unsent: usize,
    /// The number of full refreshes that the core asked for. See
    /// [`ViewModel::full_refresh_seq`].
    pub full_refresh_seq: u32,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            pages: PAGES,
            settings: None,
            route: Route::Home,
            entities: Entities::default(),
            connection: connection::State::default(),
            power: power::State::default(),
            last_sync: None,
            utc_offset_minutes: 0,
            unsent: 0,
            full_refresh_seq: 0,
        }
    }
}

impl Model {
    /// Returns the index of the page for the current route. If no page has
    /// the route, the index is 0.
    #[must_use]
    pub fn page_index(&self) -> usize {
        self.pages
            .iter()
            .position(|page| page.is_at(&self.route))
            .unwrap_or(0)
    }

    /// Returns the page for the current route.
    #[must_use]
    pub fn current_page(&self) -> Option<&'static Page> {
        self.pages.get(self.page_index())
    }

    /// Returns `true` if the core wants a connection: it has settings and the
    /// device is awake.
    #[must_use]
    pub const fn wants_online(&self) -> bool {
        self.settings.is_some() && !self.power.asleep
    }
}

/// The Home Assistant dashboard app.
#[derive(Default)]
pub struct Dashboard;

/// The command type of the app.
pub type Cmd = Command<Effect, Event>;

impl App for Dashboard {
    type Event = Event;
    type Model = Model;
    type ViewModel = ViewModel;
    type Effect = Effect;

    fn update(&self, event: Event, model: &mut Model) -> Cmd {
        match event {
            Event::Start(start) => on_start(start, model),
            Event::Woke => power::woke(model),
            Event::PowerSourceChanged(source) => power::source_changed(source, model),
            Event::TileTapped(index) => rooms::tile_tapped(index, model),
            Event::SceneTapped(index) => rooms::scene_tapped(index, model),
            Event::NextPage => rooms::turn_page(model, true),
            Event::PreviousPage => rooms::turn_page(model, false),
            Event::Navigate(route) => rooms::navigate(route, model),
            Event::UtcOffsetChanged(minutes) => {
                model.utc_offset_minutes = minutes;
                redraw(model, false)
            }
            Event::Wifi { request, status } => connection::wifi(request, status, model),
            Event::Mqtt { connection, event } => connection::mqtt(connection, event, model),
            Event::Synced(now) => connection::synced(now, model),
            Event::Reconnect(id) => connection::reconnect(id, model),
            Event::IdleTimeout(id) => power::idle_timeout(id, model),
            Event::ReadyToSleep { idle, now } => power::sleep(idle, now, model),
        }
    }

    fn view(&self, model: &Model) -> ViewModel {
        view::view(model)
    }
}

/// The earliest time that the core accepts from the clock:
/// 2025-01-01T00:00:00Z, in seconds since the Unix epoch.
///
/// A device with no RTC and no network time counts from the Unix epoch when
/// it starts. A time before this one is such a count, not the time of day.
pub const EARLIEST_VALID_TIME: u64 = 1_735_689_600;

/// Reads the wall clock with `Time::now_instant`. The answer is `None` if the
/// shell gives a time that is not valid, or a time before
/// [`EARLIEST_VALID_TIME`].
///
/// This function is the only place that reads the clock.
pub(crate) fn now() -> RequestBuilder<Effect, Event, impl Future<Output = Option<DateTime<Utc>>>> {
    Time::now_instant().map(valid_time)
}

/// Converts an instant from the clock to a time, or `None` if the instant is
/// not a plausible time of day.
fn valid_time(instant: Instant) -> Option<DateTime<Utc>> {
    if instant.seconds() < EARLIEST_VALID_TIME {
        return None;
    }
    let seconds = i64::try_from(instant.seconds()).ok()?;
    DateTime::from_timestamp(seconds, instant.nanos())
}

/// Asks the shell to draw. If `full_refresh` is `true`, the core also asks
/// for a full refresh of the panel. A later partial redraw does not cancel it.
pub(crate) fn redraw(model: &mut Model, full_refresh: bool) -> Cmd {
    if full_refresh {
        model.full_refresh_seq = model.full_refresh_seq.wrapping_add(1);
    }
    render()
}

fn on_start(start: Start, model: &mut Model) -> Cmd {
    let Start {
        settings,
        power: source,
        path,
        utc_offset_minutes,
        last_sync,
    } = start;
    model.settings = Some(settings);
    model.utc_offset_minutes = utc_offset_minutes;
    let kept = last_sync
        .and_then(|seconds| i64::try_from(seconds).ok())
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
    model.last_sync = kept.or(model.last_sync);
    model.route = path
        .as_deref()
        .and_then(Route::from_path)
        .filter(|route| model.pages.iter().any(|page| page.is_at(route)))
        .or_else(|| model.pages.first().map(Page::route))
        .unwrap_or_default();
    model.power.source = source;
    model.power.asleep = false;
    connection::reset(model);

    redraw(model, true)
        .and(connection::go_online(model))
        .and(power::restart_idle_timer(model))
}

#[cfg(test)]
mod tests {
    use crux_time::Instant;

    use super::{EARLIEST_VALID_TIME, valid_time};

    #[test]
    fn a_time_since_boot_is_not_a_valid_time() {
        // A device with no RTC: 90 seconds after the start.
        assert_eq!(valid_time(Instant::new(90, 0)), None);
        assert_eq!(valid_time(Instant::new(EARLIEST_VALID_TIME - 1, 0)), None);
    }

    #[test]
    fn a_time_after_2025_is_valid() {
        let time = valid_time(Instant::new(EARLIEST_VALID_TIME, 5)).unwrap();
        assert_eq!(time.to_rfc3339(), "2025-01-01T00:00:00.000000005+00:00");
    }
}
