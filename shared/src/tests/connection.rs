//! Tests of the connection feature, one step at a time.

use alloc::{format, vec::Vec};
use core::time::Duration;

use crux_core::{App, Request};

use super::{ExpectFullRefreshTimer, ExpectLight, start};
use crate::{
    Cmd, Connection, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    capabilities::{mqtt::MqttEvent, network::WifiStatus},
    features::{
        connection::{Link, RECONNECT_MAX, RECONNECT_MIN},
        refresh::DRAW_DELAY,
    },
    mqtt::{Connect, Message, Qos},
    view::LightState::{On, Unavailable},
};

/// Drives the core from the start to an MQTT connection request. Returns the
/// request, which the test must keep while the connection is open.
pub fn drive_to_connecting(power: PowerSource) -> (Model, Cmd, Request<Connect>) {
    let app = Dashboard;
    let mut model = Model::default();

    let mut cmd = app.update(start(power), &mut model);
    let event = cmd
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
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

/// The time `seconds` after the Unix epoch.
fn at(seconds: u64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(seconds.cast_signed(), 0).unwrap()
}

/// Returns the state-stream topic of `key` for the entity `entity_id`.
pub fn topic(entity_id: &str, key: &str) -> alloc::string::String {
    let (domain, object_id) = entity_id.split_once('.').unwrap();
    format!("homeassistant/statestream/{domain}/{object_id}/{key}")
}

/// Sends a message on the stream of the connection, and returns the command
/// of the update.
pub fn send(
    model: &mut Model,
    cmd: &mut Cmd,
    request: &mut Request<Connect>,
    topic: &str,
    payload: &str,
) -> Cmd {
    request.resolve(message(topic, payload)).unwrap();
    let event = cmd.expect_one_event();
    Dashboard.update(event, model)
}

/// Expects the command to start only the draw timer. Fires the timer and
/// returns its event.
#[track_caller]
fn expect_draw_timer(cmd: &mut Cmd) -> Event {
    let mut timer = cmd.expect_one_effect().expect_time_notify_after();
    assert_eq!(Duration::from(timer.operation.duration), DRAW_DELAY);
    let id = timer.operation.id;
    timer.resolve(id).unwrap();
    cmd.expect_one_event()
}

/// Sends a change of `entity_id` as Home Assistant does: the state, then
/// `last_updated` with the time `time` (in seconds since the Unix epoch).
/// Returns the commands of the two updates.
fn change(
    model: &mut Model,
    cmd: &mut Cmd,
    request: &mut Request<Connect>,
    entity_id: &str,
    state: &str,
    time: u64,
) -> (Cmd, Cmd) {
    let first = send(model, cmd, request, &topic(entity_id, "state"), state);
    let updated = super::broker::iso_time(time);
    let second = send(
        model,
        cmd,
        request,
        &topic(entity_id, "last_updated"),
        &updated,
    );
    (first, second)
}

/// Sends a change of `entity_id` with [`change`]. If the page shows the
/// entity, expects one draw timer and fires it. Returns the command of the
/// draw, or `None` if the page does not show the entity.
fn change_and_draw(
    model: &mut Model,
    cmd: &mut Cmd,
    request: &mut Request<Connect>,
    entity_id: &str,
    state: &str,
    time: u64,
) -> Option<Cmd> {
    let (mut first, mut second) = change(model, cmd, request, entity_id, state, time);
    second.expect_no_effects();
    let mut effects: Vec<_> = first.effects().collect();
    let effect = effects.pop()?;
    assert!(effects.is_empty());
    let mut timer = effect.expect_time_notify_after();
    assert_eq!(Duration::from(timer.operation.duration), DRAW_DELAY);
    let id = timer.operation.id;
    timer.resolve(id).unwrap();
    let event = first.expect_one_event();
    Some(Dashboard.update(event, model))
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
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
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
fn connected_publishes_online_subscribes_and_draws() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connecting(PowerSource::Stand);

    request.resolve(MqttEvent::Connected).unwrap();
    let event = cmd.expect_one_event();

    app.update(event, &mut model)
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/status");
            assert_eq!(publish.payload, b"online");
            assert_eq!(publish.qos, Qos::AtLeastOnce);
            assert!(publish.retain);
        })
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
        .expect_only_render();

    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Online);
    assert_eq!(view.updated, "");
}

#[test]
fn a_message_for_a_visible_tile_draws_the_new_state() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    // The change does not draw at once. It starts the draw timer.
    let mut first = send(
        &mut model,
        &mut cmd,
        &mut request,
        "homeassistant/statestream/light/all_lights/state",
        "on",
    );
    // The timer waits, so the next change starts no other timer.
    send(
        &mut model,
        &mut cmd,
        &mut request,
        "homeassistant/statestream/light/all_lights/brightness",
        "64",
    )
    .expect_no_effects();

    // The timer draws both changes once.
    let due = expect_draw_timer(&mut first);
    app.update(due, &mut model).expect_only_render();

    let view = app.view(&model);
    let all_lights = view.index().unwrap().all_lights.as_ref().unwrap();
    assert_eq!(all_lights.label, "All lights");
    assert_eq!(all_lights.state, TileState::On);
    assert_eq!(all_lights.text, "On 25%");
    // A partial redraw does not ask for a full refresh.
    assert_eq!(view.full_refresh_seq, 1);
}

#[test]
fn a_message_for_another_page_changes_the_state_but_does_not_draw() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let mut navigate = app.update(Event::Navigate(Route::room("living_room")), &mut model);
    let _effects: Vec<_> = navigate.effects().collect();

    // `light.loft_1` is not in the layout (the entity is `light.loft_loft_1`),
    // so the core keeps nothing and does not draw.
    send(
        &mut model,
        &mut cmd,
        &mut request,
        "homeassistant/statestream/light/loft_1/state",
        "on",
    )
    .expect_no_effects();
    assert_eq!(model.entities.get("light.loft_1"), None);

    // The state and the time changed, but the page does not show them. The
    // core starts no timer and does not draw.
    let (mut first, mut second) = change(
        &mut model,
        &mut cmd,
        &mut request,
        "light.loft_loft_1",
        "on",
        super::NOW,
    );
    first.expect_no_effects();
    second.expect_no_effects();
    let loft = model.entities.get("light.loft_loft_1").unwrap();
    assert_eq!(loft.state.as_deref(), Some("on"));
    assert_eq!(loft.last_updated, Some(at(super::NOW)));
}

#[test]
fn on_the_index_a_message_draws_only_if_a_summary_changes() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let lights = |model: &Model| app.view(model).index().unwrap().rooms[7].lights.clone();
    assert_eq!(lights(&model), [Unavailable; 3]);

    // The summary of the Loft shows the new state. The draw timer draws once.
    change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        "light.loft_loft_1",
        "on",
        super::NOW,
    )
    .unwrap()
    .expect_only_render();
    assert_eq!(lights(&model), [On, Unavailable, Unavailable]);
    assert_eq!(app.view(&model).full_refresh_seq, 1);

    // No summary shows the group of the Loft, because the Loft has the
    // members of the group. The core does not draw.
    assert!(
        change_and_draw(
            &mut model,
            &mut cmd,
            &mut request,
            "light.loft_lights",
            "on",
            super::NOW,
        )
        .is_none()
    );
    assert_eq!(lights(&model), [On, Unavailable, Unavailable]);
}

#[test]
fn a_lost_connection_shows_the_time_of_the_last_message_and_retries_after_a_backoff() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW,
    );

    request
        .resolve(MqttEvent::Disconnected {
            reason: Some("connection lost".into()),
        })
        .unwrap();
    let event = cmd.expect_one_event();
    assert!(cmd.is_done());

    // The time is the time of the last update from Home Assistant, not the
    // time of the disconnect.
    let reconnect = app
        .update(event, &mut model)
        .expect_render()
        .resolve_time_notify_after(|notify| {
            assert_eq!(Duration::from(notify.duration), RECONNECT_MIN);
            notify.id
        })
        .expect_event();
    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.updated, "Last changed 09:41");

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
            // The view still shows `Connecting`, so the core does not draw.
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
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
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
    let index = view.index().unwrap();
    let all_lights = index.all_lights.as_ref().unwrap();
    assert_eq!(all_lights.state, TileState::Unavailable);
    assert_eq!(all_lights.text, "Unavailable");
    let weather = index.weather.as_ref().unwrap();
    assert_eq!(weather.state, TileState::Value);
    assert_eq!(weather.text, "11.5 °C");
    assert_eq!(weather.detail, "Rainy");
}

#[test]
fn an_answer_to_an_older_wifi_request_is_ignored() {
    let app = Dashboard;
    let mut model = Model::default();

    let mut first = app.update(start(PowerSource::Battery), &mut model);
    first
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer();
    let mut old = first.expect_effect().expect_wifi_on();

    // The shell starts again, for example after a deep sleep, and the core
    // asks for Wi-Fi again.
    let mut second = app.update(start(PowerSource::Battery), &mut model);
    second
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_wifi_on();

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

/// The All lights toggle on the index page.
const VISIBLE: &str = "light.all_lights";
/// An entity that the index page does not show: no summary of a room shows
/// the humidity. The living room page shows it.
const HIDDEN: &str = "sensor.living_room_thermometer_humidity";
/// The weather tile on the index page.
const WEATHER: &str = "weather.home";
/// 2026-12-01T08:41:00Z, in winter, when the UK time is UTC.
const WINTER: u64 = 1_796_114_460;

/// Goes to the page of `route`, and expects a draw.
fn navigate(model: &mut Model, route: Route) {
    let mut cmd = Dashboard.update(Event::Navigate(route), model);
    let effects: Vec<_> = cmd.effects().collect();
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Render(_)))
    );
}

#[test]
fn a_shown_change_gives_one_draw_with_the_time_from_home_assistant() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    // Home Assistant sends the state, then `last_updated`, as plain ISO
    // 8601 text with microseconds.
    let mut first = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(VISIBLE, "state"),
        "on",
    );
    let updated = "2026-07-01T08:41:21.123456+00:00";
    let mut second = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(VISIBLE, "last_updated"),
        updated,
    );
    // The state only starts the draw timer, and `last_updated` gives no
    // effects, because the timer waits.
    second.expect_no_effects();
    let due = expect_draw_timer(&mut first);

    // The timer gives exactly one render. 08:41 UTC is 09:41 in BST.
    app.update(due, &mut model).expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
    assert_eq!(app.view(&model).last_sync, Some(super::NOW));
}

#[test]
fn in_winter_the_footer_time_is_utc() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    change_and_draw(&mut model, &mut cmd, &mut request, VISIBLE, "on", WINTER)
        .unwrap()
        .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 08:41");
}

#[test]
fn a_hidden_change_gives_no_effects_and_shows_its_time_on_its_page() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW,
    );

    // On the index page, the humidity of the living room changes two minutes
    // later. The index page does not show it, so the core starts no timer
    // and does not draw.
    let (mut first, mut second) = change(
        &mut model,
        &mut cmd,
        &mut request,
        HIDDEN,
        "40",
        super::NOW + 120,
    );
    first.expect_no_effects();
    second.expect_no_effects();
    assert_eq!(app.view(&model).updated, "Last changed 09:41");

    // The living room page shows the time of that change.
    navigate(&mut model, Route::room("living_room"));
    assert_eq!(app.view(&model).updated, "Last changed 09:43");
    assert_eq!(app.view(&model).last_sync, Some(super::NOW + 120));
}

#[test]
fn a_page_keeps_its_own_older_time_after_a_page_turn() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW,
    );
    navigate(&mut model, Route::room("living_room"));

    // On the living room page, the humidity changes two minutes later.
    change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        HIDDEN,
        "40",
        super::NOW + 120,
    )
    .unwrap()
    .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:43");

    // No entity on the index page changed, so it shows its older time.
    navigate(&mut model, Route::Home);
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
}

#[test]
fn the_index_time_follows_last_updated_of_an_entity_that_a_summary_shows() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW,
    );

    // The summary of the Loft shows its first light.
    change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        "light.loft_loft_1",
        "on",
        super::NOW + 120,
    )
    .unwrap()
    .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:43");

    // Only the time of an attribute change moves, for example the
    // temperature of the weather. The time still shows.
    let mut first = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(WEATHER, "last_updated"),
        &super::broker::iso_time(super::NOW + 180),
    );
    let due = expect_draw_timer(&mut first);
    app.update(due, &mut model).expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:44");
}

#[test]
fn a_burst_of_messages_gives_one_draw_timer_and_one_render() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);

    // The seed automation sends each entity again when the dashboard
    // connects. The first change that the page shows starts the draw timer.
    let updated = super::broker::iso_time(super::NOW);
    let mut first = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(VISIBLE, "state"),
        "on",
    );
    for (entity_id, key, payload) in [
        (VISIBLE, "last_updated", updated.as_str()),
        (HIDDEN, "state", "40"),
        (HIDDEN, "last_updated", updated.as_str()),
        (WEATHER, "state", "rainy"),
        (WEATHER, "last_updated", updated.as_str()),
        (WEATHER, "temperature", "11.5"),
    ] {
        send(
            &mut model,
            &mut cmd,
            &mut request,
            &topic(entity_id, key),
            payload,
        )
        .expect_no_effects();
    }

    // The timer draws all the changes once.
    let due = expect_draw_timer(&mut first);
    app.update(due, &mut model).expect_only_render();
    let view = app.view(&model);
    assert_eq!(view.updated, "Last changed 09:41");
    assert_eq!(
        view.index().unwrap().weather.as_ref().unwrap().detail,
        "Rainy"
    );

    // The next change starts a new timer.
    let (mut first, _) = change(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "off",
        super::NOW,
    );
    let _due = expect_draw_timer(&mut first);
}

#[test]
fn a_last_updated_in_the_same_minute_as_the_page_time_draws_nothing() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW,
    );

    // The same entity in the same minute is not a change, so the core
    // starts no timer.
    send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(VISIBLE, "last_updated"),
        &super::broker::iso_time(super::NOW + 40),
    )
    .expect_no_effects();

    // Another entity on the page gets its first time, in the same minute as
    // the footer time. The core starts the timer, but the view is the same,
    // so the timer draws nothing.
    let mut first = send(
        &mut model,
        &mut cmd,
        &mut request,
        &topic(WEATHER, "last_updated"),
        &super::broker::iso_time(super::NOW + 30),
    );
    let due = expect_draw_timer(&mut first);
    app.update(due, &mut model).expect_no_effects();
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
}

#[test]
fn a_last_updated_that_is_not_valid_is_ignored() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW,
    );

    for payload in [
        "",
        "yesterday",
        "\"2026-07-01T09:00:00+00:00\"",
        "1782895260",
    ] {
        send(
            &mut model,
            &mut cmd,
            &mut request,
            &topic(VISIBLE, "last_updated"),
            payload,
        )
        .expect_no_effects();
    }
    assert_eq!(
        model.entities.get(VISIBLE).unwrap().last_updated,
        Some(at(super::NOW))
    );
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
}

#[test]
fn the_core_does_not_subscribe_to_last_changed() {
    let (model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
    let topics = crate::entities::topics(model.pages);
    assert!(topics.contains(&topic(VISIBLE, "last_updated")));
    assert!(topics.contains(&topic(HIDDEN, "last_updated")));
    assert!(!topics.iter().any(|topic| topic.ends_with("last_changed")));
}

#[test]
fn after_start_the_footer_shows_the_kept_time_until_a_change_on_the_page() {
    let app = Dashboard;
    let mut model = Model::default();
    let Event::Start(start) = start(PowerSource::Stand) else {
        unreachable!()
    };
    let start = Event::Start(crate::Start {
        last_sync: Some(super::NOW),
        ..start
    });
    let _effects: Vec<_> = app.update(start, &mut model).effects().collect();
    // No entity has a `last_updated` time, so the footer shows the kept time.
    assert_eq!(model.entities.last_updated().count(), 0);
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
    assert_eq!(app.view(&model).last_sync, Some(super::NOW));
}

#[test]
fn a_time_from_home_assistant_replaces_the_kept_time() {
    let app = Dashboard;
    let (mut model, mut cmd, mut request) = drive_to_connected(PowerSource::Stand);
    model.last_sync = Some(at(super::NOW));
    assert_eq!(app.view(&model).updated, "Last changed 09:41");

    // A change on the page has an earlier time than the kept time. The
    // footer shows the time from Home Assistant.
    change_and_draw(
        &mut model,
        &mut cmd,
        &mut request,
        VISIBLE,
        "on",
        super::NOW - 600,
    )
    .unwrap()
    .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:31");
}
