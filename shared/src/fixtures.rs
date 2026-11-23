//! A shell with fixed answers, for tests and pictures.
//!
//! [`FixedShell`] drives the real core with no clock and no network. It
//! answers each effect at once: the fake radio ([`fake::FakeRadio`]) and
//! the connection always work, and the fake broker sends [`states`]. Home
//! Assistant updated each entity at [`NOW`]. Timers expire only when you
//! call [`FixedShell::fire_timers`]. Each run gives the same views, so the
//! golden-image tests of the `ui` crate and the PNG mode of the simulator
//! use it.
//!
//! Turn on the `fixtures` feature to use this module.

use std::{
    collections::VecDeque,
    format,
    string::{String, ToString},
    vec::Vec,
};

use crux_core::{Request, capability::Operation};
use crux_time::operation::NotifyAfter;

use crate::{
    Core, Dashboard, Effect, Event, Firmware, PowerSource, Settings, Start, TileKind, ViewModel,
    capabilities::{
        log::Uptime,
        stream::{Open, StreamItem, Written},
    },
    fake,
    pages::PAGES,
};

/// The time of the last update of each entity, in seconds since the Unix
/// epoch: 2026-07-01 09:41:00 UTC, which is 10:41 in British Summer Time.
pub const NOW: u64 = 1_782_898_860;

/// [`NOW`] as Home Assistant publishes it on a `last_updated` topic.
pub const NOW_TEXT: &str = "2026-07-01T09:41:00+00:00";

/// A shell that answers the effects of the core at once, with fixed answers.
///
/// The MQTT session of the core talks to a fake broker ([`fake::Broker`]),
/// which keeps the messages of [`states`] as retained messages. Home
/// Assistant keeps the service calls, but does not do them, so a tap does
/// not change a state.
pub struct FixedShell {
    core: Core<Dashboard>,
    broker: fake::Broker,
    radio: fake::FakeRadio,
    stream: Option<Request<Open>>,
    timers: Vec<Request<NotifyAfter>>,
}

impl FixedShell {
    /// Starts the core with `power` on the page `path`, and answers the
    /// effects until the core waits.
    #[must_use]
    pub fn start(power: PowerSource, path: &str) -> Self {
        Self::start_without(power, path, &[])
    }

    /// Starts the core as [`FixedShell::start`] does, but the broker sends
    /// no message for the entities in `without`. Each of these entities has
    /// no state, so its tile shows `-` and its toggle shows a cross.
    #[must_use]
    pub fn start_without(power: PowerSource, path: &str, without: &[&str]) -> Self {
        let mut broker = fake::Broker::default();
        broker.ignore_calls = true;
        for (topic, payload) in states() {
            if !without.iter().any(|entity| is_topic_of(&topic, entity)) {
                broker.retain(&topic, payload.as_bytes());
            }
        }
        let mut shell = Self {
            core: Core::new(),
            broker,
            radio: fake::FakeRadio::default(),
            stream: None,
            timers: Vec::new(),
        };
        let effects = shell.core.process_event(Event::Start(Start {
            settings: Settings {
                device_id: "fixture".into(),
                host: "localhost".into(),
                port: 1883,
                credentials: None,
                read_only: false,
            },
            firmware: Firmware {
                version: "0.0.0+fixture".into(),
                mac: [0x02, 0, 0, 0, 0, 0x01],
            },
            boot: crate::Boot {
                reset: crate::ResetReason::PowerOn,
                restarts: 0,
            },
            before_restart: Vec::new(),
            power,
            path: Some(path.into()),
            last_sync: None,
        }));
        shell.run(effects);
        shell
    }

    /// Returns the view of the core now.
    #[must_use]
    pub fn view(&self) -> ViewModel {
        self.core.view()
    }

    fn resolve<Op: Operation>(&self, request: &mut Request<Op>, output: Op::Output) -> Vec<Effect> {
        self.core.resolve(request, output).unwrap_or_default()
    }

    fn run(&mut self, effects: Vec<Effect>) {
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            let more = match effect {
                Effect::RadioJoin(mut request) => {
                    let joined = self.radio.join(&request.operation);
                    self.resolve(&mut request, joined)
                }
                Effect::RadioAddress(mut request) => {
                    let lease = self.radio.address(&request.operation);
                    self.resolve(&mut request, lease)
                }
                Effect::RadioLeave(mut request) => {
                    self.radio.leave(&request.operation);
                    self.resolve(&mut request, ())
                }
                Effect::RadioScan(mut request) => {
                    let scanned = self.radio.scan(&request.operation);
                    self.resolve(&mut request, scanned)
                }
                Effect::RadioStopScan(mut request) => {
                    self.radio.stop_scan(&request.operation);
                    self.resolve(&mut request, ())
                }
                Effect::RadioReadSignal(mut request) => {
                    let read = self.radio.read_signal(&request.operation);
                    self.resolve(&mut request, read)
                }
                Effect::RadioOff(mut request) => {
                    self.radio.off(&request.operation);
                    self.resolve(&mut request, ())
                }
                Effect::StreamOpen(mut request) => {
                    let mut more = self.end_stream();
                    self.broker.open().expect("the fake broker is up");
                    more.extend(self.resolve(&mut request, StreamItem::Opened));
                    self.stream = Some(request);
                    more
                }
                Effect::StreamWrite(mut request) => {
                    let bytes = std::mem::take(&mut request.operation.bytes);
                    let answer = self.broker.receive(bytes);
                    let mut more = self.resolve(&mut request, Written::Sent);
                    more.extend(self.deliver(answer));
                    more
                }
                Effect::StreamClose(mut request) => {
                    let open = self
                        .stream
                        .as_ref()
                        .map(|stream| stream.operation.connection);
                    let mut more = if open == Some(request.operation.connection) {
                        self.end_stream()
                    } else {
                        Vec::new()
                    };
                    more.extend(self.resolve(&mut request, ()));
                    more
                }
                Effect::FrontLight(mut request) => self.resolve(&mut request, ()),
                Effect::TimeNotifyAfter(request) => {
                    self.timers.push(request);
                    Vec::new()
                }
                Effect::TimeClear(mut request) => {
                    let id = request.operation.id;
                    self.timers.retain(|timer| timer.operation.id != id);
                    self.resolve(&mut request, id)
                }
                // The shell has no clock, so each line has the time 0.
                Effect::Log(mut request) => self.resolve(&mut request, Uptime(0)),
                Effect::LogClock(mut request) => self.resolve(&mut request, Uptime(0)),
                Effect::Render(_) | Effect::Sleep(_) | Effect::RadioRestart(_) => Vec::new(),
            };
            queue.extend(more);
        }
    }

    /// Gives `bytes` from the broker to the core on the open stream, and
    /// returns the new effects.
    fn deliver(&mut self, bytes: Vec<u8>) -> Vec<Effect> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let Some(mut request) = self.stream.take() else {
            return Vec::new();
        };
        let effects = self.resolve(&mut request, StreamItem::Received(bytes));
        self.stream = Some(request);
        effects
    }

    /// Sends `event` to the core, and answers the effects until the core
    /// waits.
    pub fn send(&mut self, event: Event) {
        let effects = self.core.process_event(event);
        self.run(effects);
    }

    /// Sends the state of an entity, the time of the update ([`NOW`]) and
    /// the attributes, as Home Assistant does, and answers the effects until
    /// the core waits. Each attribute value is JSON, for example `128`. If
    /// the connection is not open, the core gets nothing.
    ///
    /// The core draws the change when its draw timer expires. Call
    /// [`FixedShell::fire_timers`] to draw it.
    pub fn set_state(&mut self, entity_id: &str, state: &str, attributes: &[(&str, &str)]) {
        let Some((domain, object)) = entity_id.split_once('.') else {
            return;
        };
        let base = format!("homeassistant/statestream/{domain}/{object}");
        let messages = [
            (format!("{base}/state"), state),
            (format!("{base}/last_updated"), NOW_TEXT),
        ]
        .into_iter()
        .chain(
            attributes
                .iter()
                .map(|(name, value)| (format!("{base}/{name}"), *value)),
        );
        for (topic, payload) in messages {
            self.broker.publish(&topic, payload.as_bytes(), false);
        }
        let bytes = self.broker.take();
        let effects = self.deliver(bytes);
        self.run(effects);
    }

    /// Ends the MQTT connection, as if the broker went away, and answers
    /// the effects until the core waits.
    pub fn drop_connection(&mut self) {
        let effects = self.end_stream();
        self.run(effects);
    }

    /// Ends the open stream, and returns the new effects. If the broker did
    /// not get a DISCONNECT packet, it publishes the last will.
    fn end_stream(&mut self) -> Vec<Effect> {
        self.broker.drop_connection();
        let Some(mut request) = self.stream.take() else {
            return Vec::new();
        };
        self.resolve(&mut request, StreamItem::closed("test"))
    }

    /// Fires all the timers that wait, also the timers of the MQTT session,
    /// and answers the effects until the core waits.
    pub fn fire_timers(&mut self) {
        for mut timer in std::mem::take(&mut self.timers) {
            let id = timer.operation.id;
            let effects = self.resolve(&mut timer, id);
            self.run(effects);
        }
    }
}

/// Returns `true` if `topic` is a state topic of the entity `entity_id`.
fn is_topic_of(topic: &str, entity_id: &str) -> bool {
    let Some((domain, object)) = entity_id.split_once('.') else {
        return false;
    };
    topic
        .strip_prefix(&format!("homeassistant/statestream/{domain}/{object}/"))
        .is_some()
}

/// Returns the fixed messages of the broker for each entity on each page.
///
/// The messages are a state, a `last_updated` time of [`NOW`], and a JSON
/// attribute where the tile shows one. Some entities are off, one is
/// unavailable, and one has no messages.
#[must_use]
pub fn states() -> Vec<(String, String)> {
    let mut messages = Vec::new();
    let tiles = PAGES.iter().flat_map(|page| page.tiles.iter());
    for (index, tile) in tiles.enumerate() {
        let Some((domain, object)) = tile.entity_id.split_once('.') else {
            continue;
        };
        let base = format!("homeassistant/statestream/{domain}/{object}");
        let mut add = |name: &str, payload: &str| {
            messages.push((format!("{base}/{name}"), payload.to_string()));
        };
        if index % 11 == 10 {
            // The entity has no state, so its tile shows `-`.
            continue;
        }
        add("last_updated", NOW_TEXT);
        if index % 13 == 7 {
            add("state", "unavailable");
            continue;
        }
        match tile.kind {
            TileKind::Light if index % 3 == 0 => {
                add("state", "on");
                add("brightness", &((index * 37) % 255 + 1).to_string());
            }
            TileKind::Light | TileKind::Switch if index % 2 == 0 => add("state", "on"),
            TileKind::Light | TileKind::Switch => add("state", "off"),
            TileKind::Sensor => {
                let id = tile.entity_id;
                let (state, unit) = if id.ends_with("temperature") {
                    ("21.4", "\"°C\"")
                } else if id.ends_with("humidity") || id.ends_with("progress") {
                    ("48", "\"%\"")
                } else if id.ends_with("power") {
                    ("1.2", "\"W\"")
                } else if id.ends_with("finish_time") {
                    ("2026-07-01T10:30:00+00:00", "null")
                } else {
                    ("run", "null")
                };
                add("state", state);
                add("unit_of_measurement", unit);
            }
            TileKind::Climate => {
                add("state", "heat");
                add("current_temperature", "19.5");
                add("temperature", "21");
            }
            TileKind::Weather => {
                add("state", "partlycloudy");
                add("temperature", "12.3");
                add("temperature_unit", "\"°C\"");
            }
        }
    }
    messages
}
