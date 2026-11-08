//! A shell with fixed answers, for tests and pictures.
//!
//! [`FixedShell`] drives the real core with no clock and no network. It
//! answers each effect at once: Wi-Fi and the connection always work, the
//! broker sends [`states`], and the clock is always [`NOW`]. Timers expire
//! only when you call [`FixedShell::fire_timers`]. Each run gives the same
//! views, so the golden-image tests of the `ui` crate and the PNG mode of
//! the simulator use it.
//!
//! Turn on the `fixtures` feature to use this module.

use std::{
    collections::VecDeque,
    format,
    string::{String, ToString},
    vec::Vec,
};

use crux_core::{Request, capability::Operation};
use crux_time::{Instant, operation::NotifyAfter};

use crate::{
    Core, Dashboard, Effect, Event, PowerSource, Settings, Start, TileKind, ViewModel,
    capabilities::{
        mqtt::{Connect, Message, MqttEvent},
        network::WifiStatus,
    },
    pages::PAGES,
};

/// The time of the fixed clock in seconds since the Unix epoch:
/// 2026-07-01 09:41:00 UTC, which is 10:41 in British Summer Time.
pub const NOW: u64 = 1_782_898_860;

/// A shell that answers the effects of the core at once, with fixed answers.
pub struct FixedShell {
    core: Core<Dashboard>,
    connection: Option<Request<Connect>>,
    timers: Vec<Request<NotifyAfter>>,
    /// The entity ids for which the broker sends no message.
    without: Vec<String>,
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
        let mut shell = Self {
            core: Core::new(),
            connection: None,
            timers: Vec::new(),
            without: without.iter().map(ToString::to_string).collect(),
        };
        let effects = shell.core.process_event(Event::Start(Start {
            settings: Settings {
                device_id: "fixture".into(),
                host: "localhost".into(),
                port: 1883,
                credentials: None,
                read_only: false,
            },
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
                Effect::WifiOn(mut request) => self.resolve(&mut request, WifiStatus::Connected),
                Effect::MqttConnect(mut request) => {
                    let more = self.resolve(&mut request, MqttEvent::Connected);
                    self.connection = Some(request);
                    more
                }
                Effect::MqttSubscribe(_) => {
                    let mut more = Vec::new();
                    if let Some(mut request) = self.connection.take() {
                        for (topic, payload) in states() {
                            if self
                                .without
                                .iter()
                                .any(|entity| is_topic_of(&topic, entity))
                            {
                                continue;
                            }
                            let message = MqttEvent::Message(Message {
                                topic,
                                payload: payload.into_bytes(),
                            });
                            more.extend(self.resolve(&mut request, message));
                        }
                        self.connection = Some(request);
                    }
                    more
                }
                Effect::MqttDisconnect(_) => self.end_stream(),
                Effect::TimeNow(mut request) => self.resolve(&mut request, Instant::new(NOW, 0)),
                Effect::TimeNotifyAfter(request) => {
                    self.timers.push(request);
                    Vec::new()
                }
                Effect::TimeClear(mut request) => {
                    let id = request.operation.id;
                    self.timers.retain(|timer| timer.operation.id != id);
                    self.resolve(&mut request, id)
                }
                Effect::Render(_)
                | Effect::MqttPublish(_)
                | Effect::WifiOff(_)
                | Effect::Sleep(_) => Vec::new(),
            };
            queue.extend(more);
        }
    }

    /// Sends `event` to the core, and answers the effects until the core
    /// waits.
    pub fn send(&mut self, event: Event) {
        let effects = self.core.process_event(event);
        self.run(effects);
    }

    /// Sends the state of an entity and its attributes, as Home Assistant
    /// does, and answers the effects until the core waits. Each attribute
    /// value is JSON, for example `128`. If the connection is not open, the
    /// core gets nothing.
    pub fn set_state(&mut self, entity_id: &str, state: &str, attributes: &[(&str, &str)]) {
        let Some((domain, object)) = entity_id.split_once('.') else {
            return;
        };
        let Some(mut request) = self.connection.take() else {
            return;
        };
        let base = format!("homeassistant/statestream/{domain}/{object}");
        let messages = core::iter::once((format!("{base}/state"), state)).chain(
            attributes
                .iter()
                .map(|(name, value)| (format!("{base}/{name}"), *value)),
        );
        let mut effects = Vec::new();
        for (topic, payload) in messages {
            let message = MqttEvent::Message(Message {
                topic,
                payload: payload.as_bytes().to_vec(),
            });
            effects.extend(self.resolve(&mut request, message));
        }
        self.connection = Some(request);
        self.run(effects);
    }

    /// Ends the MQTT connection, as if the broker went away, and answers
    /// the effects until the core waits.
    pub fn drop_connection(&mut self) {
        let effects = self.end_stream();
        self.run(effects);
    }

    /// Ends the stream of the MQTT connection, and returns the new effects.
    fn end_stream(&mut self) -> Vec<Effect> {
        let Some(mut request) = self.connection.take() else {
            return Vec::new();
        };
        let reason = Some("test".into());
        self.resolve(&mut request, MqttEvent::Disconnected { reason })
    }

    /// Fires all the timers that wait, and answers the effects until the
    /// core waits.
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

/// Returns a fixed state message for each entity on each page, and a JSON
/// attribute where the tile shows one. Some entities are off, one is
/// unavailable, and one has no state.
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
