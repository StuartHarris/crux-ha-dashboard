//! The root of the app: the events, the effects, the model and the dispatch
//! to the features.

use alloc::{format, string::String};

use chrono::{DateTime, Utc};
use crux_core::{App, Command, macros::effect, render::RenderOperation};
use crux_time::operation as time;
use facet::Facet;
use serde::{Deserialize, Serialize};

use crate::{
    capabilities::{
        mqtt::{Connect, Credentials, Disconnect, MqttEvent, Publish, Subscribe},
        network::{WifiOff, WifiOn, WifiStatus},
        power::{PowerSource, Sleep},
    },
    entities::Entities,
    features::{
        connection, power,
        refresh::{self, redraw},
        rooms,
    },
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
    /// If `true`, the dashboard does a dry run: a tap on a tile or a scene
    /// sends no service call to Home Assistant. Refer to
    /// [`ViewModel::read_only`].
    pub read_only: bool,
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
    /// The footer time of the page, in seconds since the Unix epoch, from
    /// [`ViewModel::last_sync`]. After a deep sleep, the shell gives the
    /// value that it kept in RTC memory. The core shows it until an entity
    /// on the page has a `last_updated` time from Home Assistant.
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
    /// The device woke from a light sleep, which kept the core in memory,
    /// for a reason that is not a tap. Examples are a timer and the Wake
    /// button of the simulator. A tap that wakes the device is a `Tapped`
    /// event.
    Woke,
    /// The device moved to the stand or off the stand.
    PowerSourceChanged(PowerSource),
    /// The user tapped the panel, on this control or (`None`) on no control.
    ///
    /// The shell sends each tap, also while the device is asleep, and gets
    /// the control from `ui::hit_test`. The core decides what the tap does:
    ///
    /// - If the device is asleep, the tap wakes it. The core does not act on
    ///   the control, because the user cannot see the controls on a panel
    ///   that sleeps.
    /// - If the device is awake, a tap on a control acts on it. A tap on no
    ///   control does nothing, so a touch by accident does not keep the
    ///   device awake.
    Tapped(Option<Control>),
    /// The user asked for the next page.
    NextPage,
    /// The user asked for the previous page.
    PreviousPage,
    /// The user asked for a page by its route.
    Navigate(Route),
    /// The user started a dry run (`true`) or stopped it (`false`). Refer to
    /// [`Settings::read_only`]. Before `Start`, the core keeps a request for
    /// a dry run. `Start` cannot then stop the dry run.
    SetReadOnly(bool),

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
    /// The wait before the reconnect with this id is over.
    #[serde(skip)]
    #[facet(skip)]
    Reconnect(u32),
    /// The idle timer with this id expired.
    #[serde(skip)]
    #[facet(skip)]
    IdleTimeout(u32),
    /// The full-refresh timer with this id expired.
    #[serde(skip)]
    #[facet(skip)]
    FullRefreshDue(u32),
    /// The draw timer with this id expired. Refer to
    /// [`refresh::redraw_soon`].
    #[serde(skip)]
    #[facet(skip)]
    DrawDue(u32),
}

/// A control on the panel.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Control {
    /// The button that shows the previous page.
    PreviousPage,
    /// The button that shows the home page.
    Home,
    /// The button that shows the next page.
    NextPage,
    /// The tile with this index on the current page.
    Tile(usize),
    /// The scene button with this index on the current page.
    Scene(usize),
    /// The All lights toggle on the index page.
    AllLights,
    /// The button of the room with this index on the index page. The index
    /// is the position of the room in
    /// [`IndexView::rooms`](crate::IndexView::rooms).
    Room(usize),
    /// The + button of the tile with this index on the current page.
    ///
    /// Only a dimmable light that is on or off has this button. Refer to
    /// [`Tile::has_steps`](crate::layout::Tile::has_steps). A tap increases
    /// the brightness by
    /// [`BRIGHTNESS_STEP_PCT`](crate::layout::BRIGHTNESS_STEP_PCT) percent.
    /// If the light is off, the tap turns it on at that brightness.
    Brighter(usize),
    /// The − button of the tile with this index on the current page.
    ///
    /// Only a dimmable light that is on or off has this button. A tap
    /// decreases the brightness by
    /// [`BRIGHTNESS_STEP_PCT`](crate::layout::BRIGHTNESS_STEP_PCT) percent.
    /// If the brightness gets to 0, Home Assistant turns the light off. A tap
    /// on a light that is off does nothing.
    Dimmer(usize),
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
    /// The user asked for a dry run before `Start`. Refer to
    /// [`rooms::set_read_only`].
    pub read_only_requested: bool,
    /// The current route.
    pub route: Route,
    /// The entity states from the state stream.
    pub entities: Entities,
    /// The state of Wi-Fi and of the MQTT connection.
    pub connection: connection::State,
    /// The power source and the sleep state.
    pub power: power::State,
    /// The time that the shell kept through a deep sleep and gave back in
    /// `Start`. The footer shows this time only if no entity on the page has
    /// a `last_updated` time, for example after a wake, before the first
    /// messages arrive. Refer to [`connection::page_time`].
    pub last_sync: Option<DateTime<Utc>>,
    /// The number of service calls that the core dropped, because it slept
    /// before it could send them.
    pub unsent: usize,
    /// The number of full refreshes that the core asked for. See
    /// [`ViewModel::full_refresh_seq`].
    pub full_refresh_seq: u32,
    /// The state of the refresh policy. Refer to [`refresh`].
    pub refresh: refresh::State,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            pages: PAGES,
            settings: None,
            read_only_requested: false,
            route: Route::Home,
            entities: Entities::default(),
            connection: connection::State::default(),
            power: power::State::default(),
            last_sync: None,
            unsent: 0,
            full_refresh_seq: 0,
            refresh: refresh::State::default(),
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

    /// Returns the pages of the rooms, in order: each page except the home
    /// page.
    pub fn rooms(&self) -> impl Iterator<Item = &'static Page> + use<> {
        let pages: &'static [Page] = self.pages;
        pages.iter().filter(|page| page.is_room())
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
            Event::Tapped(control) => rooms::tapped(control, model),
            Event::NextPage => rooms::turn_page(model, true),
            Event::PreviousPage => rooms::turn_page(model, false),
            Event::Navigate(route) => rooms::navigate(route, model),
            Event::SetReadOnly(read_only) => rooms::set_read_only(read_only, model),
            Event::Wifi { request, status } => connection::wifi(request, status, model),
            Event::Mqtt { connection, event } => connection::mqtt(connection, event, model),
            Event::Reconnect(id) => connection::reconnect(id, model),
            Event::IdleTimeout(id) => power::idle_timeout(id, model),
            Event::FullRefreshDue(id) => refresh::timer_expired(id, model),
            Event::DrawDue(id) => refresh::draw_due(id, model),
        }
    }

    fn view(&self, model: &Model) -> ViewModel {
        view::view(model)
    }
}

fn on_start(start: Start, model: &mut Model) -> Cmd {
    let Start {
        settings,
        power: source,
        path,
        last_sync,
    } = start;
    let read_only = settings.read_only || model.read_only_requested;
    model.read_only_requested = false;
    model.settings = Some(Settings {
        read_only,
        ..settings
    });
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
