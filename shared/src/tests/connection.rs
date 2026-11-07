//! Tests of the connection feature, one step at a time.

use alloc::{format, vec::Vec};
use core::time::Duration;

use crux_core::{App, Request};

use super::{instant, start};
use crate::{
    Cmd, Connection, Dashboard, EffectTestExt, Event, Model, PowerSource, TileState,
    capabilities::{
        mqtt::{Connect, Message, MqttEvent, Qos},
        network::WifiStatus,
    },
    features::connection::{Link, RECONNECT_MAX, RECONNECT_MIN},
};

/// Drives the core from the start to an MQTT connection request. Returns the
/// request, which the test must keep while the connection is open.
pub fn drive_to_connecting(power: PowerSource) -> (Model, Cmd, Request<Connect>) {
    let app = Dashboard;
    let mut model = Model::default();

    let mut cmd = app.update(start(power), &mut model);
    let event = cmd
        .expect_render()
        .resolve_wifi_on(|_| WifiStatus::Connected)
        .expect_event();

    let mut cmd = app.update(event, &mut model);
    // The connection is a stream. The test keeps the request to send more
    // items, so it takes the request and does not resolve it only once.
    let request = cmd.expect_one_effect().expect_mqtt_connect();
    (model, cmd, request)
}

/// Drives the core to an open connection, and ignores the effects of
/// `Connected`.
pub fn drive_to_connected(power: PowerSource) -> (Model, Cmd, Request<Connect>) {
    let (mut model, mut cmd, mut request) = drive_to_connecting(power);
    request.resolve(MqttEvent::Connected).unwrap();
    let event = cmd.expect_one_event();
    let mut connected = Dashboard.update(event, &mut model);
    let _effects: Vec<_> = connected.effects().collect();
    assert_eq!(model.connection.mqtt, Link::Up);
    (model, cmd, request)
}

pub fn message(topic: &str, payload: &str) -> MqttEvent {
    MqttEvent::Message(Message {
        topic: topic.into(),
        payload: payload.into(),
    })
}

#[test]
fn start_on_the_stand_draws_and_turns_wifi_on() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(start(PowerSource::Stand), &mut model)
        .expect_render()
        .expect_only_wifi_on();

    let view = app.view(&model);
    assert_eq!(view.full_refresh_seq, 1);
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.title, "Home");
}

#[test]
fn wifi_opens_a_connection_with_a_last_will() {
    let (_model, _cmd, request) = drive_to_connecting(PowerSource::Stand);

    let connect = &request.operation;
    assert_eq!(connect.host, "broker.local");
    assert_eq!(connect.port, 1883);
    assert_eq!(connect.client_id, "dashboard-hall");
    assert_eq!(connect.last_will.topic, "dashboard/hall/status");
    assert_eq!(connect.last_will.payload, b"offline");
    assert!(connect.last_will.retain);
}

#[test]
fn connected_subscribes_publishes_online_and_draws() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connecting(PowerSource::Stand);

    request.resolve(MqttEvent::Connected).unwrap();
    let event = cmd.expect_one_event();

    app.update(event, &mut model)
        .expect_mqtt_subscribe_with(|subscribe| {
            let has = |topic: &str| subscribe.topics.iter().any(|t| t == topic);
            assert!(has(
                "homeassistant/statestream/light/living_room_lights/state"
            ));
            assert!(has(
                "homeassistant/statestream/light/living_room_lights/brightness"
            ));
            assert!(has("homeassistant/statestream/weather/home/temperature"));
            assert!(!subscribe.topics.iter().any(|topic| topic.contains("scene")));
            assert!(!subscribe.topics.iter().any(|topic| topic.contains('#')));
        })
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/status");
            assert_eq!(publish.payload, b"online");
            assert_eq!(publish.qos, Qos::AtLeastOnce);
            assert!(publish.retain);
        })
        .expect_only_render();

    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Online);
    assert_eq!(view.updated, "Live");
}

#[test]
fn a_message_for_a_visible_tile_draws_the_new_state() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    request
        .resolve(message(
            "homeassistant/statestream/light/all_lights/state",
            "on",
        ))
        .unwrap();
    let event = cmd.expect_one_event();
    app.update(event, &mut model).expect_only_render();

    request
        .resolve(message(
            "homeassistant/statestream/light/all_lights/brightness",
            "64",
        ))
        .unwrap();
    let event = cmd.expect_one_event();
    app.update(event, &mut model).expect_only_render();

    let view = app.view(&model);
    assert_eq!(view.tiles[0].label, "All lights");
    assert_eq!(view.tiles[0].state, TileState::On);
    assert_eq!(view.tiles[0].text, "On 25%");
    // A partial redraw does not ask for a full refresh.
    assert_eq!(view.full_refresh_seq, 1);
}

#[test]
fn a_message_for_another_page_changes_the_state_but_does_not_draw() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    request
        .resolve(message(
            "homeassistant/statestream/light/loft_1/state",
            "on",
        ))
        .unwrap();
    let event = cmd.expect_one_event();
    // `light.loft_1` is not in the layout (the entity is `light.loft_loft_1`).
    app.update(event, &mut model).expect_no_effects();
    assert_eq!(model.entities.get("light.loft_1"), None);

    request
        .resolve(message(
            "homeassistant/statestream/light/loft_loft_1/state",
            "on",
        ))
        .unwrap();
    let event = cmd.expect_one_event();
    app.update(event, &mut model).expect_no_effects();
    assert_eq!(
        model
            .entities
            .get("light.loft_loft_1")
            .unwrap()
            .state
            .as_deref(),
        Some("on")
    );
}

#[test]
fn a_lost_connection_reads_the_clock_and_retries_after_a_backoff() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    request
        .resolve(MqttEvent::Disconnected {
            reason: Some("connection lost".into()),
        })
        .unwrap();
    let event = cmd.expect_one_event();
    assert!(cmd.is_done());

    let mut lost = app.update(event, &mut model);
    let synced = lost
        .expect_render()
        .resolve_time_now(|_| instant(super::NOW))
        .expect_event();
    let reconnect = lost
        .resolve_time_notify_after(|notify| {
            assert_eq!(Duration::from(notify.duration), RECONNECT_MIN);
            notify.id
        })
        .expect_event();

    app.update(synced, &mut model).expect_only_render();
    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.updated, "Updated 09:41");

    // Wi-Fi can be the cause, so the reconnect asks for Wi-Fi first.
    let event = app
        .update(reconnect, &mut model)
        .resolve_wifi_on(|_| WifiStatus::Connected)
        .expect_event();
    let _connect = app
        .update(event, &mut model)
        .expect_one_effect()
        .expect_mqtt_connect();
}

#[test]
fn the_backoff_doubles_up_to_a_limit_and_resets_on_connect() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connecting(PowerSource::Stand);

    let mut expected = RECONNECT_MIN;
    for _ in 0..8 {
        let connection = model.connection.id;
        let refused = MqttEvent::Disconnected {
            reason: Some("connection refused".into()),
        };
        let event = app
            .update(
                Event::Mqtt {
                    connection,
                    event: refused,
                },
                &mut model,
            )
            .expect_render()
            .resolve_time_notify_after(|notify| {
                assert_eq!(Duration::from(notify.duration), expected);
                notify.id
            })
            .expect_event();
        expected = (expected * 2).min(RECONNECT_MAX);
        let event = app
            .update(event, &mut model)
            .resolve_wifi_on(|_| WifiStatus::Connected)
            .expect_event();
        let _connect = app
            .update(event, &mut model)
            .expect_one_effect()
            .expect_mqtt_connect();
    }
    assert_eq!(model.connection.backoff, RECONNECT_MAX);

    let connection = model.connection.id;
    let _ = app.update(
        Event::Mqtt {
            connection,
            event: MqttEvent::Connected,
        },
        &mut model,
    );
    assert_eq!(model.connection.backoff, RECONNECT_MIN);
}

#[test]
fn an_item_from_an_old_connection_is_ignored() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    let old = model.connection.id.wrapping_sub(1);

    app.update(
        Event::Mqtt {
            connection: old,
            event: MqttEvent::Disconnected { reason: None },
        },
        &mut model,
    )
    .expect_no_effects();
    assert_eq!(model.connection.mqtt, Link::Up);
}

#[test]
fn a_stale_reconnect_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    let stale = model.connection.reconnect.wrapping_sub(1);

    app.update(Event::Reconnect(stale), &mut model)
        .expect_no_effects();
}

#[test]
fn wifi_that_fails_is_tried_again_after_a_backoff() {
    let app = Dashboard;
    let mut model = Model::default();

    let event = app
        .update(start(PowerSource::Stand), &mut model)
        .expect_render()
        .resolve_wifi_on(|_| WifiStatus::Failed)
        .expect_event();
    let event = app
        .update(event, &mut model)
        .resolve_time_notify_after(|notify| {
            assert_eq!(Duration::from(notify.duration), RECONNECT_MIN);
            notify.id
        })
        .expect_event();
    app.update(event, &mut model).expect_only_wifi_on();
    assert_eq!(model.connection.wifi, Link::Starting);
}

#[test]
fn the_view_shows_the_state_of_a_tile_from_the_statestream() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    let base = "homeassistant/statestream/weather/home";
    for (topic, payload) in [
        (format!("{base}/state"), "rainy"),
        (format!("{base}/temperature"), "11.5"),
        (format!("{base}/temperature_unit"), "\"°C\""),
        (
            "homeassistant/statestream/light/all_lights/state".into(),
            "unavailable",
        ),
    ] {
        request.resolve(message(&topic, payload)).unwrap();
        let event = cmd.expect_one_event();
        let _ = app.update(event, &mut model);
    }

    let view = app.view(&model);
    assert_eq!(view.tiles[0].state, TileState::Unavailable);
    assert_eq!(view.tiles[0].text, "Unavailable");
    assert_eq!(view.tiles[1].state, TileState::Value);
    assert_eq!(view.tiles[1].text, "11.5 °C");
    assert_eq!(view.tiles[1].detail, "Rainy");
}

#[test]
fn an_answer_to_an_older_wifi_request_is_ignored() {
    let app = Dashboard;
    let mut model = Model::default();

    let mut first = app.update(start(PowerSource::Battery), &mut model);
    first.expect_render();
    let mut old = first.expect_effect().expect_wifi_on();

    // The shell starts again, for example after a deep sleep, and the core
    // asks for Wi-Fi again.
    let mut second = app.update(start(PowerSource::Battery), &mut model);
    second.expect_render().expect_wifi_on();

    old.resolve(WifiStatus::Connected).unwrap();
    let stale = first.expect_event();
    app.update(stale, &mut model).expect_no_effects();
    assert_eq!(model.connection.wifi, Link::Starting);
}

#[test]
fn a_message_before_connected_is_ignored() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connecting(PowerSource::Stand);

    request
        .resolve(message(
            "homeassistant/statestream/light/all_lights/state",
            "on",
        ))
        .unwrap();
    let event = cmd.expect_one_event();
    app.update(event, &mut model).expect_no_effects();
    assert_eq!(model.entities.get("light.all_lights"), None);
}
