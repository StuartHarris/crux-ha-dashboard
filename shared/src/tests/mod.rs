//! Tests of the app.
//!
//! The feature tests drive one step at a time with the `EffectTestExt`
//! helpers. The flow tests run the core in [`shell::Shell`], which answers the
//! effects with a fake MQTT broker, a fake Home Assistant with a clock,
//! Wi-Fi and timers.

mod broker;
mod connection;
mod flows;
mod light;
mod pending;
mod power;
mod refresh;
mod rooms;
mod subscribe;

mod shell;

use alloc::string::String;

use core::time::Duration;

use crate::{
    Cmd, Control, EffectTestExt, PowerSource, Settings, Start,
    app::Event,
    capabilities::mqtt::Publish,
    entities::STATESTREAM,
    features::{
        light::LIGHT_TIMEOUT,
        pending::{PENDING_TIMEOUT, SCENE_TIMEOUT},
        refresh::FULL_REFRESH_INTERVAL,
    },
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

/// Gives the entity `entity_id` the attribute `name` with the JSON value
/// `value` in `model`, as a message on the state stream does.
fn set_attribute(model: &mut crate::Model, entity_id: &str, name: &str, value: &str) {
    let (domain, object_id) = entity_id.split_once('.').expect("an entity id");
    let topic = alloc::format!("{STATESTREAM}/{domain}/{object_id}/{name}");
    let pages = model.pages;
    model.entities.apply(pages, &topic, value.as_bytes());
}

/// A tap on the + button of the tile with `index`.
const fn tap_plus(index: usize) -> Event {
    Event::Tapped(Some(Control::Plus(index)))
}

/// A tap on the − button of the tile with `index`.
const fn tap_minus(index: usize) -> Event {
    Event::Tapped(Some(Control::Minus(index)))
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

/// Expects the effects of a service call that the core issues at once.
trait ExpectServiceCall {
    /// Expects the publish of the service call, the timer of its mark and
    /// the render of the mark, and no other effect.
    fn expect_only_service_call_with(&mut self, check: impl FnOnce(&Publish));

    /// Expects the effects of a service call, with any payload.
    fn expect_only_service_call(&mut self) {
        self.expect_only_service_call_with(|_| {});
    }

    /// Expects the effect that starts the timer of a mark: [`PENDING_TIMEOUT`]
    /// or [`SCENE_TIMEOUT`].
    fn expect_pending_timer(&mut self) -> &mut Self;
}

impl ExpectServiceCall for Cmd {
    #[track_caller]
    fn expect_only_service_call_with(&mut self, check: impl FnOnce(&Publish)) {
        self.expect_mqtt_publish_with(check)
            .expect_pending_timer()
            .expect_only_render();
    }

    #[track_caller]
    fn expect_pending_timer(&mut self) -> &mut Self {
        self.expect_time_notify_after_with(|notify| {
            let duration = Duration::from(notify.duration);
            assert!(
                duration == PENDING_TIMEOUT || duration == SCENE_TIMEOUT,
                "{duration:?}"
            );
        })
    }
}

/// Expects the effects of the front light.
trait ExpectLight {
    /// Expects the next effect to set the front light to `level`.
    fn expect_light(&mut self, level: u8) -> &mut Self;

    /// Expects the next effect to start the timer of [`LIGHT_TIMEOUT`].
    fn expect_light_timer(&mut self) -> &mut Self;

    /// Expects the effects of a touch while the light is off: the light at
    /// `level`, then the light timer.
    fn expect_touch_light(&mut self, level: u8) -> &mut Self {
        self.expect_light(level).expect_light_timer()
    }
}

impl ExpectLight for Cmd {
    #[track_caller]
    fn expect_light(&mut self, level: u8) -> &mut Self {
        self.expect_front_light_with(|light| assert_eq!(light.level, level))
    }

    #[track_caller]
    fn expect_light_timer(&mut self) -> &mut Self {
        self.expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), LIGHT_TIMEOUT);
        })
    }
}
