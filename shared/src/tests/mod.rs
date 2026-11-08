//! Tests of the app.
//!
//! The feature tests drive one step at a time with the `EffectTestExt`
//! helpers. The flow tests run the core in [`shell::Shell`], which answers the
//! effects with a fake MQTT broker, a fake Home Assistant, Wi-Fi, timers and a
//! clock.

mod broker;
mod connection;
mod flows;
mod power;
mod refresh;
mod rooms;

mod shell;

use alloc::string::String;

use core::time::Duration;

use crux_time::Instant;

use crate::{
    Cmd, Control, EffectTestExt, PowerSource, Settings, Start, app::Event, entities::STATESTREAM,
    features::refresh::FULL_REFRESH_INTERVAL,
};

/// The time in the tests: 2026-07-01T08:41:00Z, which is 09:41 in BST.
const NOW: u64 = 1_782_895_260;

fn settings() -> Settings {
    Settings {
        device_id: "hall".into(),
        host: "broker.local".into(),
        port: 1883,
        credentials: None,
        read_only: false,
    }
}

fn start(power: PowerSource) -> Event {
    start_at(power, None)
}

fn start_at(power: PowerSource, path: Option<&str>) -> Event {
    Event::Start(Start {
        settings: settings(),
        power,
        path: path.map(String::from),
        last_sync: None,
    })
}

fn instant(seconds: u64) -> Instant {
    Instant::new(seconds, 0)
}

/// A tap on the tile with `index`.
const fn tap_tile(index: usize) -> Event {
    Event::Tapped(Some(Control::Tile(index)))
}

/// Gives the entity `entity_id` the state `state` in `model`, as a message
/// on the state stream does.
fn set_state(model: &mut crate::Model, entity_id: &str, state: &str) {
    let (domain, object_id) = entity_id.split_once('.').expect("an entity id");
    let topic = alloc::format!("{STATESTREAM}/{domain}/{object_id}/state");
    let pages = model.pages;
    model.entities.apply(pages, &topic, state.as_bytes());
}

/// A tap on the + button of the tile with `index`.
const fn tap_brighter(index: usize) -> Event {
    Event::Tapped(Some(Control::Brighter(index)))
}

/// A tap on the − button of the tile with `index`.
const fn tap_dimmer(index: usize) -> Event {
    Event::Tapped(Some(Control::Dimmer(index)))
}

/// A tap on the scene button with `index`.
const fn tap_scene(index: usize) -> Event {
    Event::Tapped(Some(Control::Scene(index)))
}

/// A tap on the All lights toggle of the index page.
const fn tap_all_lights() -> Event {
    Event::Tapped(Some(Control::AllLights))
}

/// A tap on the button of the room with `index` on the index page.
const fn tap_room(index: usize) -> Event {
    Event::Tapped(Some(Control::Room(index)))
}

/// Expects the effect that starts the full-refresh timer.
trait ExpectFullRefreshTimer {
    /// Expects the next effect to start the timer of
    /// [`FULL_REFRESH_INTERVAL`].
    fn expect_full_refresh_timer(&mut self) -> &mut Self;
}

impl ExpectFullRefreshTimer for Cmd {
    #[track_caller]
    fn expect_full_refresh_timer(&mut self) -> &mut Self {
        self.expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), FULL_REFRESH_INTERVAL);
        })
    }
}
