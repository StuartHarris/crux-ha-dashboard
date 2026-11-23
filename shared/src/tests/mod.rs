//! Tests of the app.
//!
//! The feature tests drive one step at a time with the `EffectTestExt`
//! helpers. The flow tests run the core in [`shell::Shell`], which answers the
//! effects with a fake MQTT broker, a fake Home Assistant with a clock, a
//! fake radio and timers.

pub mod broker;
mod buttons;
mod diagnostics;
pub mod flows;
mod light;
mod session;
mod subscribe;

pub mod shell;
pub mod wire;

use crate::features::wifi;
use alloc::string::String;

use wire::{ExpectWrite, Wire};

use core::time::Duration;

use crux_core::App;

use crate::{
    Boot, Cmd, Control, Dashboard, Effect, EffectTestExt, Firmware, Model, PowerSource,
    ResetReason, Settings, Start,
    app::Event,
    entities::STATESTREAM,
    fake::FakeRadio,
    features::{
        connection::Link,
        light::LIGHT_TIMEOUT,
        pending::{PENDING_TIMEOUT, SCENE_TIMEOUT, TARGET_TIMEOUT},
        refresh::FULL_REFRESH_INTERVAL,
        wifi::{ADDRESS_TIMEOUT, JOIN_TIMEOUT, LEAVE_TIMEOUT},
    },
    mqtt::Publish,
};

/// The time in the tests: 2026-07-01T08:41:00Z, which is 09:41 in BST.
pub const NOW: u64 = 1_782_895_260;

pub fn settings() -> Settings {
    Settings {
        device_id: "hall".into(),
        host: "broker.local".into(),
        port: 1883,
        credentials: None,
        read_only: false,
    }
}

/// Returns the state message with the battery `battery` (a number or
/// `null`), the stand `stand` (`ON` or `OFF`) and no power diagnostics.
/// The Wi-Fi facts are those of a join of the default fake radio with no
/// reconnect, and the boot facts are those of [`POWER_ON`] with no wake.
pub fn state(battery: &str, stand: &str) -> String {
    alloc::format!(
        concat!(
            r#"{{"battery":{},"stand":"{}","charging":null,"charge":null,"#,
            r#""voltage":null,"current":null,"temperature":null,"#,
            r#""rssi":-55,"bssid":"02:00:00:00:00:01","ip":"192.168.1.50","#,
            r#""reconnects":0,"restarts":0,"reset":"power_on","wake":"boot"}}"#
        ),
        battery,
        stand
    )
}

/// Returns the state message of the light at `level`, for example
/// `{"state":"ON","brightness":60}`. The level 0 gives `{"state":"OFF"}`.
pub fn light_state(level: u8) -> String {
    if level == 0 {
        String::from(r#"{"state":"OFF"}"#)
    } else {
        alloc::format!(r#"{{"state":"ON","brightness":{level}}}"#)
    }
}

/// The facts about the firmware in the tests.
pub fn firmware() -> Firmware {
    Firmware {
        version: "0.1.0+test".into(),
        mac: [0x28, 0x84, 0x85, 0x37, 0xb8, 0xa0],
    }
}

pub fn start(power: PowerSource) -> Event {
    start_at(power, None)
}

/// The start line of the log after [`POWER_ON`], as [`shell::Shell::logs`]
/// keeps it.
pub const STARTED: &str = "info power: started after a power-on";

/// The facts of a start after a power-on, with no restart.
pub const POWER_ON: Boot = Boot {
    reset: ResetReason::PowerOn,
    restarts: 0,
};

pub fn start_at(power: PowerSource, path: Option<&str>) -> Event {
    Event::Start(Start {
        settings: settings(),
        firmware: firmware(),
        boot: POWER_ON,
        before_restart: alloc::vec::Vec::new(),
        power,
        path: path.map(String::from),
        last_sync: None,
    })
}

/// A tap on the tile with `index`.
pub const fn tap_tile(index: usize) -> Event {
    Event::Tapped(Some(Control::Tile(index)))
}

/// Gives the entity `entity_id` the state `state` in `model`, as a message
/// on the state stream does.
pub fn set_state(model: &mut crate::Model, entity_id: &str, state: &str) {
    let (domain, object_id) = entity_id.split_once('.').expect("an entity id");
    let topic = alloc::format!("{STATESTREAM}/{domain}/{object_id}/state");
    let pages = model.pages;
    model.entities.apply(pages, &topic, state.as_bytes());
}

/// Gives the entity `entity_id` the attribute `name` with the JSON value
/// `value` in `model`, as a message on the state stream does.
pub fn set_attribute(model: &mut crate::Model, entity_id: &str, name: &str, value: &str) {
    let (domain, object_id) = entity_id.split_once('.').expect("an entity id");
    let topic = alloc::format!("{STATESTREAM}/{domain}/{object_id}/{name}");
    let pages = model.pages;
    model.entities.apply(pages, &topic, value.as_bytes());
}

/// A tap on the + button of the tile with `index`.
pub const fn tap_plus(index: usize) -> Event {
    Event::Tapped(Some(Control::Plus(index)))
}

/// A tap on the − button of the tile with `index`.
pub const fn tap_minus(index: usize) -> Event {
    Event::Tapped(Some(Control::Minus(index)))
}

/// A tap on the scene button with `index`.
pub const fn tap_scene(index: usize) -> Event {
    Event::Tapped(Some(Control::Scene(index)))
}

/// A tap on the All lights toggle of the index page.
pub const fn tap_all_lights() -> Event {
    Event::Tapped(Some(Control::AllLights))
}

/// A tap on the button of the room with `index` on the index page.
pub const fn tap_room(index: usize) -> Event {
    Event::Tapped(Some(Control::Room(index)))
}

/// Expects the effect that starts the full-refresh timer.
pub trait ExpectFullRefreshTimer {
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
pub trait ExpectServiceCall {
    /// Expects the publish of the service call, the timer of its mark, the
    /// render of the mark and the clear of the send timer of the session,
    /// and no other effect. The publish goes through `wire`.
    fn expect_only_service_call_with(
        &mut self,
        wire: &mut Wire,
        model: &mut Model,
        check: impl FnOnce(&Publish),
    );

    /// Expects the effects of a service call, with any payload.
    fn expect_only_service_call(&mut self, wire: &mut Wire, model: &mut Model) {
        self.expect_only_service_call_with(wire, model, |_| {});
    }

    /// Expects the effect that starts the timer of a mark: [`PENDING_TIMEOUT`],
    /// [`TARGET_TIMEOUT`] or [`SCENE_TIMEOUT`].
    fn expect_pending_timer(&mut self) -> &mut Self;
}

impl ExpectServiceCall for Cmd {
    #[track_caller]
    fn expect_only_service_call_with(
        &mut self,
        wire: &mut Wire,
        model: &mut Model,
        check: impl FnOnce(&Publish),
    ) {
        self.expect_publish_with(wire, model, check)
            .expect_pending_timer()
            .expect_render()
            .expect_session_clear(wire)
            .expect_no_effect_or_events();
    }

    #[track_caller]
    fn expect_pending_timer(&mut self) -> &mut Self {
        self.expect_time_notify_after_with(|notify| {
            let duration = Duration::from(notify.duration);
            assert!(
                [PENDING_TIMEOUT, TARGET_TIMEOUT, SCENE_TIMEOUT].contains(&duration),
                "{duration:?}"
            );
        })
    }
}

/// Expects the effects of the front light.
pub trait ExpectLight {
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

/// Expects the start of the time limit of a radio request.
pub trait ExpectRadioLimit {
    /// Expects the next effect to start a timer of `limit`.
    fn expect_radio_limit(&mut self, limit: Duration) -> &mut Self;
}

impl ExpectRadioLimit for Cmd {
    #[track_caller]
    fn expect_radio_limit(&mut self, limit: Duration) -> &mut Self {
        self.expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), limit);
        })
    }
}

/// Expects the line of the log for the start of a step of a sleep.
pub trait ExpectStep {
    /// Expects the next effect to write `message` with the area `power` at
    /// the level `Debug`. Refer to [`crate::features::power::sleep_step`].
    ///
    /// The helper drops the `Log` request with no answer. The core then
    /// keeps no line, and the command ends as if the line was not there.
    fn expect_step(&mut self, message: &str) -> &mut Self;
}

impl ExpectStep for Cmd {
    #[track_caller]
    fn expect_step(&mut self, message: &str) -> &mut Self {
        self.expect_log_with(|log| {
            assert_eq!(log.level, crate::capabilities::log::Level::Debug);
            assert_eq!(log.area, crate::capabilities::log::Area::Power);
            assert_eq!(log.message, message);
        })
    }
}

/// Answers the radio requests of a link with a fake radio, one at a time:
/// a `Leave` if the core leaves first, the `Join` and the `Address`. Each
/// request must be the next effect, with the start of its time limit after
/// it. Returns the command of the lease, which connects.
#[track_caller]
pub fn link_up(cmd: &mut Cmd, model: &mut Model) -> Cmd {
    let mut radio = FakeRadio::default();
    let mut event = answer_radio(cmd, &mut radio);
    loop {
        let leased = matches!(event, Event::Wifi(wifi::Event::Leased { .. }));
        let mut next = Dashboard.update(event, model);
        if leased {
            return next;
        }
        event = answer_radio(&mut next, &mut radio);
    }
}

/// Answers the radio request that is the next effect of `cmd` with `radio`,
/// and expects the start of its time limit. Returns the event of the
/// answer.
///
/// The function skips the lines of the log before the request, for example
/// the line of a leave for a suspect link. It drops each `Log` request with
/// no answer, so the core keeps no line. A test that checks a line uses
/// `expect_log_with` before it calls this function.
#[track_caller]
pub fn answer_radio(cmd: &mut Cmd, radio: &mut FakeRadio) -> Event {
    let mut effect = cmd.expect_effect();
    while matches!(effect, Effect::Log(_)) {
        effect = cmd.expect_effect();
    }
    let limit = match effect {
        Effect::RadioJoin(mut request) => {
            let joined = radio.join(&request.operation);
            request
                .resolve(joined)
                .expect("the core waits for the join");
            JOIN_TIMEOUT
        }
        Effect::RadioAddress(mut request) => {
            let lease = radio.address(&request.operation);
            request
                .resolve(lease)
                .expect("the core waits for the address");
            ADDRESS_TIMEOUT
        }
        Effect::RadioLeave(mut request) => {
            radio.leave(&request.operation);
            request.resolve(()).expect("the core waits for the leave");
            LEAVE_TIMEOUT
        }
        effect => panic!("expected a radio request, got {effect:?}"),
    };
    cmd.expect_radio_limit(limit).expect_one_event()
}

/// Drives the core from the start to the `Open` request of the stream.
/// Returns the wire of the stream, which the test must keep while the
/// connection is open.
pub fn drive_to_connecting(power: PowerSource) -> (Model, Wire) {
    let app = Dashboard;
    let mut model = Model::default();

    let mut cmd = app.update(start(power), &mut model);
    let start = cmd
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer();
    let cmd = link_up(start, &mut model);
    (model, Wire::new(cmd))
}

/// Drives the core to an open connection, and ignores the effects of the
/// CONNACK and the subscribe. The wire then has no effects, events or
/// packets.
pub fn drive_to_connected(power: PowerSource) -> (Model, Wire) {
    let (mut model, mut wire) = drive_to_connecting(power);
    let connected = wire.connect(&mut model);
    wire.settle(connected, &mut model);
    wire.effects.clear();
    wire.events.clear();
    wire.packets.clear();
    assert_eq!(model.connection.link(), Link::Up);
    (model, wire)
}

/// Returns the state-stream topic of `key` for the entity `entity_id`.
pub fn topic(entity_id: &str, key: &str) -> alloc::string::String {
    let (domain, object_id) = entity_id.split_once('.').unwrap();
    format!("homeassistant/statestream/{domain}/{object_id}/{key}")
}

/// Sends a message from Home Assistant through the broker, and returns the
/// command of the update.
pub fn send(model: &mut Model, wire: &mut Wire, topic: &str, payload: &str) -> Cmd {
    wire.send(model, topic, payload)
}

/// Returns the lines of the log, as [`shell::Shell::logs`] keeps them, of
/// the sleep `id` with the light off and no scan: disconnect, turn the radio
/// off, then sleep.
pub fn sleep_lines(id: u32) -> [alloc::string::String; 3] {
    [
        format!("debug power: sleep {id} closes the connection"),
        format!("debug power: sleep {id} turns the radio off"),
        format!("debug power: sleep {id} draws the last image and sleeps"),
    ]
}

/// Returns `true` if the view shows that the device is asleep. A test that
/// reads the sleep state through this function does not depend on the
/// fields of the power state.
pub fn asleep(model: &Model) -> bool {
    Dashboard.view(model).asleep
}
