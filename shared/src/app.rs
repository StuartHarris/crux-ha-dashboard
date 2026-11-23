//! The root of the app: the events, the effects, the model and the dispatch
//! to the features.

use alloc::{format, string::String, vec::Vec};

use chrono::{DateTime, Utc};
use crux_core::{App, Command, macros::effect, render::RenderOperation};
use crux_time::operation as time;

use crate::{
    capabilities::{
        light::FrontLight,
        log::{Area, Log, LogClock, Uptime},
        power::{PowerSource, Reading, Sleep},
        radio::{Address, Join, Leave, Off, Restart, Scan, StopScan},
        stream::{Close, Open, Write},
    },
    entities::Entities,
    features::{
        connection, device, light, logs, pending, power,
        refresh::{self, redraw},
        rooms, wifi,
    },
    layout::Page,
    mqtt::Credentials,
    pages::PAGES,
    route::Route,
    view::ViewModel,
};

/// The broker and device settings, which the shell supplies.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// Returns the topic on which the dashboard publishes the lines of its
    /// log. Refer to [`logs`].
    #[must_use]
    pub fn log_topic(&self) -> String {
        format!("dashboard/{}/log", self.device_id)
    }

    /// Returns the retained topic on which the dashboard publishes the
    /// state of its sensors as one JSON object. Refer to [`device`].
    #[must_use]
    pub fn state_topic(&self) -> String {
        format!("dashboard/{}/state", self.device_id)
    }

    /// Returns the topic `dashboard/<id>/set/<name>`, on which Home
    /// Assistant sends a command to the device, for example
    /// `dashboard/hall/set/button`. With the name `+`, it is the topic
    /// filter of all these topics. Refer to [`device::route`].
    #[must_use]
    pub fn set_topic(&self, name: &str) -> String {
        format!("dashboard/{}/set/{name}", self.device_id)
    }

    /// Returns the id of the dashboard as a device of Home Assistant, for
    /// example `dashboard_hall`. Refer to [`device`].
    #[must_use]
    pub fn unique_id(&self) -> String {
        format!("dashboard_{}", self.device_id)
    }

    /// Returns the MQTT client id.
    #[must_use]
    pub fn client_id(&self) -> String {
        format!("dashboard-{}", self.device_id)
    }
}

/// The facts about the firmware of the shell, which the shell gives in
/// [`Start`]. The core tells them to Home Assistant in the discovery
/// message. Refer to [`device`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firmware {
    /// The version of the firmware, for example `0.1.0+1b073946` or
    /// `0.1.0+1b073946.dirty`.
    pub version: String,
    /// The MAC address of the Wi-Fi station interface.
    pub mac: [u8; 6],
}

/// Why the chip started: the kind of its last reset. The shell gives it in
/// [`Boot`].
///
/// The core writes it in the line `power: started after <reset>` and
/// publishes it in the state message. Refer to [`device`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetReason {
    /// The power came on, for example after the battery was empty, or a
    /// reset that also clears the RTC memory. The simulator always gives
    /// this reason.
    PowerOn,
    /// The software restarted the chip, for example at a `RadioRestart`
    /// request.
    Software,
    /// The software stopped with a panic.
    Panic,
    /// A watchdog timer reset the chip.
    Watchdog,
    /// The supply voltage was too low.
    Brownout,
    /// A reset of another kind, for example from the USB port.
    Other,
}

impl ResetReason {
    /// Returns the words for the start line of the log, for example
    /// `a watchdog reset`.
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::PowerOn => "a power-on",
            Self::Software => "a software reset",
            Self::Panic => "a panic",
            Self::Watchdog => "a watchdog reset",
            Self::Brownout => "a brownout",
            Self::Other => "a reset of another kind",
        }
    }

    /// Returns `true` if the reset tells of a fault: a panic, a watchdog
    /// reset or a brownout. The start line is then a warning.
    #[must_use]
    pub const fn is_fault(self) -> bool {
        matches!(self, Self::Panic | Self::Watchdog | Self::Brownout)
    }
}

/// The facts about the start of the chip, which the shell gives in
/// [`Start`]. The core publishes them in the state message. Refer to
/// [`device`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boot {
    /// The kind of the last reset.
    pub reset: ResetReason,
    /// The number of resets that were not a power-on, since the last
    /// power-on. This start counts. After a power-on, the value is 0. The
    /// simulator always gives 0.
    pub restarts: u32,
}

/// A warn or error line that the shell kept through a restart, in memory
/// that a reset does not clear. The shell gives these lines in
/// [`Start::before_restart`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootLine {
    /// How important the line is.
    pub level: logs::Kept,
    /// The part of the shell or of the core that wrote the line, for
    /// example `wifi`.
    pub source: String,
    /// The text of the line.
    pub message: String,
}

/// The start of the app: before the first `Start`, or after it with the
/// settings and the facts about the firmware and the boot.
#[derive(Debug)]
pub enum Startup {
    /// The core waits for the first `Start`. It has no settings, and it
    /// does not connect.
    ///
    /// `read_only` is the dry run that the user asked for before `Start`
    /// ([`rooms::set_read_only`]). At `Start`, the core does a dry run if
    /// the settings or this request ask for one. `Start` changes the value
    /// to `Started`.
    Waiting {
        /// The user asked for a dry run.
        read_only: bool,
    },
    /// The core has the settings from `Start`, with the dry run of the
    /// user, and the facts about the firmware. A later `Start` replaces both
    /// and uses only the dry run of the shell.
    Started {
        /// The broker and device settings.
        settings: Settings,
        /// The facts about the firmware.
        firmware: Firmware,
        /// The facts about the start of the chip.
        boot: Boot,
    },
}

impl Default for Startup {
    fn default() -> Self {
        Self::Waiting { read_only: false }
    }
}

impl Startup {
    /// Returns the settings, or `None` before `Start`.
    #[must_use]
    pub const fn settings(&self) -> Option<&Settings> {
        match self {
            Self::Waiting { .. } => None,
            Self::Started { settings, .. } => Some(settings),
        }
    }

    /// Returns the facts about the firmware, or `None` before `Start`.
    #[must_use]
    pub const fn firmware(&self) -> Option<&Firmware> {
        match self {
            Self::Waiting { .. } => None,
            Self::Started { firmware, .. } => Some(firmware),
        }
    }

    /// Returns the facts about the start of the chip, or `None` before
    /// `Start`.
    #[must_use]
    pub const fn boot(&self) -> Option<Boot> {
        match self {
            Self::Waiting { .. } => None,
            Self::Started { boot, .. } => Some(*boot),
        }
    }

    /// Returns `true` after `Start`.
    #[must_use]
    pub const fn is_started(&self) -> bool {
        matches!(self, Self::Started { .. })
    }

    /// Returns `true` if the dashboard does a dry run. Before `Start`, this
    /// is the request of the user.
    #[must_use]
    pub const fn read_only(&self) -> bool {
        match self {
            Self::Waiting { read_only } => *read_only,
            Self::Started { settings, .. } => settings.read_only,
        }
    }
}

/// The facts that the shell sends when it starts the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    /// The broker and device settings.
    pub settings: Settings,
    /// The facts about the firmware.
    pub firmware: Firmware,
    /// The facts about the start of the chip.
    pub boot: Boot,
    /// The warn and error lines that the shell kept from before a reset
    /// that was not a power-on, from the oldest. After a power-on, the
    /// list is empty. The core puts them in its log before any new line.
    /// Only the first `Start` uses them. Refer to [`logs::before_restart`].
    pub before_restart: Vec<BootLine>,
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
/// The wrapper variants at the end hold the internal events of the features.
/// The core makes them from the answers to its effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The shell started. On battery, the device also starts after a deep
    /// sleep.
    Start(Start),
    /// The device moved to the stand or off the stand.
    PowerSourceChanged(PowerSource),
    /// The charge of the battery changed, in percent. `None` means that the
    /// shell cannot read the charge. The shell sends the first level after
    /// `Start`. The event does not wake the device.
    BatteryChanged(Option<u8>),
    /// The user tapped the panel, on this control or (`None`) on no control.
    ///
    /// The shell gets the control from `ui::hit_test`. While a `Sleep`
    /// request waits, the shell answers it with `Wake::Touch` and the
    /// control, in place of a tap. The core decides what the tap does:
    ///
    /// - If the device is asleep (for example during the steps before
    ///   `Sleep`), the tap wakes it and then acts on the control, because
    ///   the panel still shows the page. A service call waits in the outbox
    ///   until the connection is open.
    /// - If the device is awake, a tap on a control acts on it. A tap on no
    ///   control does not act, and it does not start the idle timer again.
    ///   Thus a touch by accident does not keep the device awake.
    ///
    /// Each tap also turns the front light on, also a tap on no control.
    /// Refer to [`light`].
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
    /// The user asked the device to sleep now. The core acts as if the idle
    /// timer expired now: on battery, the device sleeps, or first waits one
    /// more idle period if service calls wait in the outbox. On the stand,
    /// or while the device sleeps, the event does nothing.
    SleepNow,
    /// The shell read the power diagnostics: the phase of the charge, and
    /// the voltage, the current and the temperature of the battery. Refer
    /// to [`Reading`].
    ///
    /// The shell sends a reading only when a rounded value changes. The core
    /// keeps the reading and publishes it to Home Assistant in the state
    /// message. Refer to [`device`]. The event does not draw and does not
    /// wake the device. While the device sleeps, the core only keeps it.
    PowerRead(Reading),
    /// The shell wrote a line of its own log, for example the failure of a
    /// stream. The core keeps the line in its ring, and publishes it to Home
    /// Assistant as it publishes its own lines. Refer to [`logs`].
    ///
    /// The shell sends its warn and error lines only, and never a line that
    /// it wrote for a `Log` request of the core. The event does not render
    /// and does not wake the device.
    ShellLogged {
        /// The time of the line, on the clock of the `Log` answers.
        at: Uptime,
        /// How important the line is: `Warn` or `Error`.
        level: logs::Kept,
        /// The part of the shell that wrote the line, for example `stream`.
        /// The core keeps at most [`logs::MAX_SOURCE`] bytes.
        source: String,
        /// The text of the line. The core keeps at most
        /// [`logs::MAX_MESSAGE`] bytes.
        message: String,
        /// The number of lines that the shell lost since its last
        /// `ShellLogged`, for example because its own queue was full. The
        /// core adds them to its drop count.
        dropped: u32,
    },

    /// The internal events of the Wi-Fi feature. The core makes them from the
    /// answers to its effects. The shell never sends them.
    Wifi(wifi::Event),
    /// The internal events of the MQTT connection. The core makes them from
    /// the answers to its effects. The shell never sends them.
    Connection(connection::Event),
    /// The internal events of the power feature. The core makes them from the
    /// answers to its effects. The shell never sends them.
    Power(power::Event),
    /// The internal events of the refresh policy. The core makes them from the
    /// answers to its effects. The shell never sends them.
    Refresh(refresh::Event),
    /// The internal events of the pending marks. The core makes them from the
    /// answers to its effects. The shell never sends them.
    Pending(pending::Event),
    /// The internal events of the front light. The core makes them from the
    /// answers to its effects. The shell never sends them.
    Light(light::Event),
    /// The internal events of the logs feature. The core makes them from the
    /// answers to its `Log` and `LogClock` requests. The shell never sends
    /// them.
    Logs(logs::Event),
    /// The internal events of the device feature. The core makes them from
    /// its timers. The shell never sends them.
    Device(device::Event),
}

/// A control on the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Only a tile with steps has this button. Refer to
    /// [`Tile::has_steps`](crate::layout::Tile::has_steps).
    ///
    /// - On a dimmable light, a tap increases the brightness by
    ///   [`BRIGHTNESS_STEP_PCT`](crate::layout::BRIGHTNESS_STEP_PCT) percent.
    ///   If the light is off, the tap turns it on at that brightness.
    /// - On a thermostat, a tap sets the target temperature to the next mark
    ///   of [`STEP`](crate::temperature::STEP) up.
    Plus(usize),
    /// The − button of the tile with this index on the current page.
    ///
    /// Only a tile with steps has this button.
    ///
    /// - On a dimmable light, a tap decreases the brightness by
    ///   [`BRIGHTNESS_STEP_PCT`](crate::layout::BRIGHTNESS_STEP_PCT) percent.
    ///   If the brightness gets to 0, Home Assistant turns the light off. A
    ///   tap on a light that is off does nothing.
    /// - On a thermostat, a tap sets the target temperature to the next mark
    ///   of [`STEP`](crate::temperature::STEP) down.
    Minus(usize),
}

/// The effects that the core asks the shell to do.
#[effect]
#[derive(Debug)]
pub enum Effect {
    /// Draw the view model: a partial render, or a full refresh of the panel.
    Render(RenderOperation),
    /// Open the TCP byte stream to the MQTT broker for a new session.
    StreamOpen(Open),
    /// Send the bytes of an MQTT packet (connect, subscribe, publish, ping)
    /// on the stream.
    StreamWrite(Write),
    /// Close the stream, for example at a sleep, at a failed session or
    /// before a new connection.
    StreamClose(Close),
    /// Join an access point, at a start, at a roam and after a lost link.
    RadioJoin(Join),
    /// Wait for an IP address after a join.
    RadioAddress(Address),
    /// Leave the access point, before a new join or a sleep.
    RadioLeave(Leave),
    /// Scan for access points, to find the best one for a roam.
    RadioScan(Scan),
    /// Stop a scan that runs, for example at a sleep.
    RadioStopScan(StopScan),
    /// Turn the radio off before a sleep.
    RadioOff(Off),
    /// Restart the chip, if many joins in a row give no new address.
    RadioRestart(Restart),
    /// Put the device to sleep on battery, until a touch or a timer wakes it.
    Sleep(Sleep),
    /// Set the brightness of the front light: on at a touch, off at the
    /// light timer, a reset or a sleep.
    FrontLight(FrontLight),
    /// Write a line of the core to the log of the shell: a decision of the
    /// Wi-Fi policy, a failure or a step of a sleep. The answer is the time
    /// of the line, and the core keeps the line with it. Refer to [`logs`].
    Log(Log),
    /// Ask for the time on the clock of the `Log` answers, before the core
    /// sends its kept lines to Home Assistant with their ages. Refer to
    /// [`logs`].
    LogClock(LogClock),
    /// Start a timer: the idle, light, draw, refresh, mark, radio-limit,
    /// reconnect, close and session timers.
    TimeNotifyAfter(time::NotifyAfter),
    /// Clear a timer that the core does not need any more.
    TimeClear(time::ClearTimer),
}

/// The state of the dashboard.
#[derive(Debug)]
pub struct Model {
    /// The pages of the dashboard.
    pub pages: &'static [Page],
    /// The start of the app, with the broker and device settings after
    /// `Start`.
    pub startup: Startup,
    /// The current route.
    pub route: Route,
    /// The entity states from the state stream.
    pub entities: Entities,
    /// The state of the MQTT connection.
    pub connection: connection::State,
    /// The state of Wi-Fi: the radio, the cache of the access point and the
    /// health of the link. Refer to [`wifi`].
    pub wifi: wifi::State,
    /// The power source and the sleep state.
    pub power: power::State,
    /// The state of the front light. Refer to [`light`].
    pub light: light::State,
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
    /// The entities with a service call that Home Assistant did not confirm
    /// yet. Refer to [`pending`].
    pub pending: pending::State,
    /// The lines of the log of the core, with their times. Refer to
    /// [`logs`].
    pub logs: logs::State,
    /// The discovery of the dashboard as a device of Home Assistant. Refer
    /// to [`device`].
    pub device: device::State,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            pages: PAGES,
            startup: Startup::default(),
            route: Route::Home,
            entities: Entities::default(),
            connection: connection::State::default(),
            wifi: wifi::State::default(),
            power: power::State::default(),
            light: light::State::default(),
            last_sync: None,
            unsent: 0,
            full_refresh_seq: 0,
            refresh: refresh::State::default(),
            pending: pending::State::default(),
            logs: logs::State::default(),
            device: device::State::default(),
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

    /// Returns `true` if the core wants a connection: the app started and
    /// the device is awake.
    #[must_use]
    pub const fn wants_online(&self) -> bool {
        self.startup.is_started() && !self.power.is_asleep()
    }

    /// Handles `event`, and returns the command with its effects.
    pub fn update(&mut self, event: Event) -> Cmd {
        match event {
            Event::Start(start) => on_start(start, self),
            Event::PowerSourceChanged(source) => power::source_changed(source, self),
            Event::BatteryChanged(level) => power::battery_changed(level, self),
            Event::Tapped(control) => rooms::tapped(control, self),
            Event::NextPage => rooms::turn_page(self, true),
            Event::PreviousPage => rooms::turn_page(self, false),
            Event::Navigate(route) => rooms::navigate(route, self),
            Event::SetReadOnly(read_only) => rooms::set_read_only(read_only, self),
            Event::SleepNow => power::sleep_now(self).1,
            Event::PowerRead(reading) => power::read(reading, self),
            Event::ShellLogged {
                at,
                level,
                source,
                message,
                dropped,
            } => logs::shell_logged(at, level, &source, &message, dropped, self),
            Event::Wifi(event) => wifi::update(event, self),
            Event::Connection(event) => connection::update(event, self),
            Event::Power(event) => power::update(event, self),
            Event::Refresh(event) => refresh::update(event, self),
            Event::Pending(event) => pending::update(event, self),
            Event::Light(event) => light::update(event, self),
            Event::Logs(event) => logs::update(event, self),
            Event::Device(event) => device::update(event, self),
        }
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
        model.update(event)
    }

    fn view(&self, model: &Model) -> ViewModel {
        model.into()
    }
}

fn on_start(start: Start, model: &mut Model) -> Cmd {
    let Start {
        settings,
        firmware,
        boot,
        before_restart,
        power: source,
        path,
        last_sync,
    } = start;
    // A second `Start` uses only the dry run of the shell. It keeps no lines
    // from before the restart, because the first `Start` kept them.
    let (requested, first) = match model.startup {
        Startup::Waiting { read_only } => (read_only, true),
        Startup::Started { .. } => (false, false),
    };
    // The lines from before the restart go first, before the start line
    // and each other new line.
    if first {
        logs::before_restart(before_restart, model);
    }
    model.startup = Startup::Started {
        settings: Settings {
            read_only: settings.read_only || requested,
            ..settings
        },
        firmware,
        boot,
    };
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
    let summary = power::reset(source, model);
    connection::reset(model);
    device::start(model);

    // The state of the light after a reset is not known, so the core turns
    // it off.
    light::reset(model)
        .and(redraw(model, true))
        .and(connection::go_online(model))
        .and(power::restart_idle_timer(model))
        .and(summary)
        .and(started(boot))
}

/// Writes the start line, for example `power: started after a watchdog
/// reset, restart 3`. A reset that tells of a fault gives a warning, and the
/// other resets give an Info line.
fn started(boot: Boot) -> Cmd {
    let Boot { reset, restarts } = boot;
    let message = if restarts > 0 {
        format!("started after {}, restart {restarts}", reset.words())
    } else {
        format!("started after {}", reset.words())
    };
    if reset.is_fault() {
        logs::warn(Area::Power, message)
    } else {
        logs::info(Area::Power, message)
    }
}
