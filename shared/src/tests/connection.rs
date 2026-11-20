//! Tests of the connection feature, one step at a time.

use alloc::{format, vec::Vec};
use core::time::Duration;

use crux_core::App;

use super::{
    ExpectFullRefreshTimer, ExpectLight, ExpectRadioLimit,
    broker::publish_of,
    link_up, start,
    wire::{ExpectWrite, Wire},
};
use crate::{
    Cmd, Connection, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    capabilities::{
        radio::{Joined, Target},
        stream::{StreamItem, Written},
    },
    features::{
        connection::{CLOSE_TIMEOUT, Link, RECONNECT_MAX, RECONNECT_MIN},
        refresh::DRAW_DELAY,
        wifi::{JOIN_TIMEOUT, Phase},
    },
    mqtt::{
        Qos,
        codec::{self, Flags, server::Packet},
    },
    view::LightState::{On, Unavailable},
};

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
    assert_eq!(model.connection.mqtt, Link::Up);
    (model, wire)
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

/// Sends a message from Home Assistant through the broker, and returns the
/// command of the update.
pub fn send(model: &mut Model, wire: &mut Wire, topic: &str, payload: &str) -> Cmd {
    wire.send(model, topic, payload)
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
    wire: &mut Wire,
    entity_id: &str,
    state: &str,
    time: u64,
) -> (Cmd, Cmd) {
    let first = send(model, wire, &topic(entity_id, "state"), state);
    let updated = super::broker::iso_time(time);
    let second = send(model, wire, &topic(entity_id, "last_updated"), &updated);
    (first, second)
}

/// Sends a change of `entity_id` with [`change`]. If the page shows the
/// entity, expects one draw timer and fires it. Returns the command of the
/// draw, or `None` if the page does not show the entity.
fn change_and_draw(
    model: &mut Model,
    wire: &mut Wire,
    entity_id: &str,
    state: &str,
    time: u64,
) -> Option<Cmd> {
    let (mut first, mut second) = change(model, wire, entity_id, state, time);
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

#[test]
fn start_on_the_stand_draws_and_joins_the_strongest_access_point() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(start(PowerSource::Stand), &mut model)
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_radio_join_with(|join| {
            assert_eq!((join.target, join.keep), (Target::Strongest, false));
        })
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), JOIN_TIMEOUT);
        });

    let view = app.view(&model);
    assert_eq!(view.full_refresh_seq, 1);
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.title, "Home");
}

/// Returns the topic filters of the SUBSCRIBE packets in `packets`.
fn filters(packets: &[Packet]) -> Vec<alloc::string::String> {
    packets
        .iter()
        .filter_map(|packet| match packet {
            Packet::Subscribe { filters, .. } => Some(filters),
            _ => None,
        })
        .flatten()
        .map(|(filter, _qos)| filter.clone())
        .collect()
}

#[test]
fn a_link_opens_a_connection_with_a_last_will() {
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);
    assert_eq!(wire.host(), ("broker.local", 1883));

    let _connected = wire.connect(&mut model);
    let Packet::Connect {
        flags,
        client_id,
        will_topic,
        will_payload,
        ..
    } = &wire.packets[0]
    else {
        panic!("a CONNECT: {:?}", wire.packets);
    };
    assert_eq!(client_id, "dashboard-hall");
    assert_eq!(will_topic, "dashboard/hall/status");
    assert_eq!(will_payload, b"offline");
    // The will is retained.
    assert_ne!(flags & 0x20, 0);
}

#[test]
fn connected_publishes_online_subscribes_and_draws() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);

    // The status and the subscribe go in one write.
    wire.connect(&mut model)
        .expect_write_with(&mut wire, &mut model, |packets| {
            let publishes: Vec<_> = packets.iter().filter_map(publish_of).collect();
            assert_eq!(publishes.len(), 1);
            let publish = &publishes[0];
            assert_eq!(publish.topic, "dashboard/hall/status");
            assert_eq!(publish.payload, b"online");
            assert_eq!(publish.qos, Qos::AtLeastOnce);
            assert!(publish.retain);
            let topics = filters(packets);
            let has = |topic: &str| topics.iter().any(|t| t == topic);
            assert!(has(
                "homeassistant/statestream/light/living_room_lights/state"
            ));
            assert!(has(
                "homeassistant/statestream/light/living_room_lights/brightness"
            ));
            assert!(has("homeassistant/statestream/weather/home/temperature"));
            assert!(!topics.iter().any(|topic| topic.contains("scene")));
            assert!(!topics.iter().any(|topic| topic.contains('#')));
        })
        .expect_only_render();

    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Online);
    assert_eq!(view.updated, "");
}

#[test]
fn a_message_for_a_visible_tile_draws_the_new_state() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    // The change does not draw at once. It starts the draw timer.
    let mut first = send(
        &mut model,
        &mut wire,
        "homeassistant/statestream/light/all_lights/state",
        "on",
    );
    // The timer waits, so the next change starts no other timer.
    send(
        &mut model,
        &mut wire,
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let mut navigate = app.update(Event::Navigate(Route::room("living_room")), &mut model);
    let _effects: Vec<_> = navigate.effects().collect();

    // `light.loft_1` is not in the layout (the entity is `light.loft_loft_1`),
    // so the core keeps nothing and does not draw.
    wire.send_unsubscribed(
        &mut model,
        "homeassistant/statestream/light/loft_1/state",
        "on",
    )
    .expect_no_effects();
    assert_eq!(model.entities.get("light.loft_1"), None);

    // The state and the time changed, but the page does not show them. The
    // core starts no timer and does not draw.
    let (mut first, mut second) =
        change(&mut model, &mut wire, "light.loft_loft_1", "on", super::NOW);
    first.expect_no_effects();
    second.expect_no_effects();
    let loft = model.entities.get("light.loft_loft_1").unwrap();
    assert_eq!(loft.state.as_deref(), Some("on"));
    assert_eq!(loft.last_updated, Some(at(super::NOW)));
}

#[test]
fn on_the_index_a_message_draws_only_if_a_summary_changes() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let lights = |model: &Model| app.view(model).index().unwrap().rooms[7].lights.clone();
    assert_eq!(lights(&model), [Unavailable; 3]);

    // The summary of the Loft shows the new state. The draw timer draws once.
    change_and_draw(&mut model, &mut wire, "light.loft_loft_1", "on", super::NOW)
        .unwrap()
        .expect_only_render();
    assert_eq!(lights(&model), [On, Unavailable, Unavailable]);
    assert_eq!(app.view(&model).full_refresh_seq, 1);

    // No summary shows the group of the Loft, because the Loft has the
    // members of the group. The core does not draw.
    assert!(
        change_and_draw(&mut model, &mut wire, "light.loft_lights", "on", super::NOW,).is_none()
    );
    assert_eq!(lights(&model), [On, Unavailable, Unavailable]);
}

#[test]
fn a_failed_write_does_not_hide_an_end_by_the_host_in_either_order() {
    let app = Dashboard;
    for failed_first in [true, false] {
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        model.wifi.health.failed();
        let connection = model.connection.id;
        let write = Event::Written {
            connection,
            outcome: Written::Failed("write failed".into()),
        };
        let end = Event::Stream {
            connection,
            item: StreamItem::by_host("the broker closed the stream"),
        };
        let events = if failed_first {
            [write, end]
        } else {
            [end, write]
        };
        let mut closes = Vec::new();
        for event in events {
            let mut cmd = app.update(event, &mut model);
            closes.extend(cmd.effects().filter_map(|effect| match effect {
                Effect::StreamClose(request) => Some(request.operation),
                _ => None,
            }));
        }
        // The `Closed` item ends the session, and the end by the host
        // proves traffic.
        assert_eq!(closes.len(), 1, "failed first: {failed_first}");
        assert!(
            closes[0]
                .reason
                .contains("the broker host ended the stream"),
            "{closes:?}"
        );
        assert_eq!(model.connection.mqtt, Link::Down);
        assert!(!model.wifi.health.suspect(), "failed first: {failed_first}");
    }
}

#[test]
fn a_lost_connection_shows_the_time_of_the_last_message_and_retries_after_a_backoff() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW);
    let connection = model.connection.id;

    // The time is the time of the last update from Home Assistant, not the
    // time of the disconnect. The core closes the stream, which failed.
    let reconnect = wire
        .drop_connection(&mut model, "connection lost")
        .expect_stream_close_with(|close| assert!(!close.clean))
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), CLOSE_TIMEOUT);
        })
        .expect_render()
        .resolve_time_notify_after(|notify| {
            assert_eq!(Duration::from(notify.duration), RECONNECT_MIN);
            notify.id
        })
        .expect_event();
    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.updated, "Last changed 09:41");

    // The shell answers the `Close` request. The core waits for the backoff.
    app.update(Event::StreamClosed { connection }, &mut model)
        .expect_no_effects();

    // The lost connection makes the link suspect, so the reconnect leaves
    // and joins again first.
    let mut cmd = app.update(reconnect, &mut model);
    let _wire = Wire::new(link_up(&mut cmd, &mut model));
}

#[test]
fn the_backoff_doubles_up_to_a_limit_and_resets_on_connect() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);

    let mut expected = RECONNECT_MIN;
    for _ in 0..8 {
        let connection = model.connection.id;
        let refused = StreamItem::by_host("connection refused");
        let event = app
            .update(
                Event::Stream {
                    connection,
                    item: refused,
                },
                &mut model,
            )
            .expect_stream_close_with(|close| assert!(!close.clean))
            .expect_time_notify_after_with(|notify| {
                assert_eq!(Duration::from(notify.duration), CLOSE_TIMEOUT);
            })
            // The view still shows `Connecting`, so the core does not draw.
            .resolve_time_notify_after(|notify| {
                assert_eq!(Duration::from(notify.duration), expected);
                notify.id
            })
            .expect_event();
        app.update(Event::StreamClosed { connection }, &mut model)
            .expect_no_effects();
        expected = (expected * 2).min(RECONNECT_MAX);
        let mut cmd = app.update(event, &mut model);
        wire = Wire::new(link_up(&mut cmd, &mut model));
    }
    assert_eq!(model.connection.backoff, RECONNECT_MAX);

    let _connected = wire.connect(&mut model);
    assert_eq!(model.connection.backoff, RECONNECT_MIN);
}

#[test]
fn an_item_from_an_old_connection_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let old = model.connection.id.wrapping_sub(1);

    app.update(
        Event::Stream {
            connection: old,
            item: StreamItem::closed("connection lost"),
        },
        &mut model,
    )
    .expect_no_effects();
    assert_eq!(model.connection.mqtt, Link::Up);
}

#[test]
fn a_stale_reconnect_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let stale = model.connection.reconnect.wrapping_sub(1);

    app.update(Event::Reconnect(stale), &mut model)
        .expect_no_effects();
}

#[test]
fn a_join_that_fails_is_tried_again_after_a_backoff() {
    let app = Dashboard;
    let mut model = Model::default();

    let event = app
        .update(start(PowerSource::Stand), &mut model)
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
        .resolve_radio_join(|_| Joined::Failed("authentication failed".into()))
        .expect_radio_limit(JOIN_TIMEOUT)
        .expect_one_event();
    let event = app
        .update(event, &mut model)
        .resolve_time_notify_after(|notify| {
            assert_eq!(Duration::from(notify.duration), RECONNECT_MIN);
            notify.id
        })
        .expect_event();
    app.update(event, &mut model)
        .expect_radio_join()
        .expect_only_time_notify_after();
    assert_eq!(model.wifi.phase, Phase::Joining(Target::Strongest));
}

#[test]
fn the_view_shows_the_state_of_a_tile_from_the_statestream() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

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
        let _ = wire.send(&mut model, &topic, payload);
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
fn an_answer_to_an_older_join_is_ignored() {
    let app = Dashboard;
    let mut model = Model::default();

    let mut first = app.update(start(PowerSource::Battery), &mut model);
    first
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer();
    let mut old = first.expect_effect().expect_radio_join();

    // The shell starts again, for example after a deep sleep, and the core
    // joins again.
    let mut second = app.update(start(PowerSource::Battery), &mut model);
    second
        .expect_light(0)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_radio_join();

    let joined = crate::fake::FakeRadio::default().join(&old.operation);
    old.resolve(joined).unwrap();
    let stale = first.expect_event();
    app.update(stale, &mut model).expect_no_effects();
    assert_eq!(model.wifi.phase, Phase::Joining(Target::Strongest));
}

#[test]
fn bytes_before_the_stream_opens_are_ignored() {
    let app = Dashboard;
    let (mut model, wire) = drive_to_connecting(PowerSource::Stand);

    let mut bytes = Vec::new();
    let flags = Flags {
        qos: Qos::AtMostOnce,
        retain: false,
    };
    codec::publish(
        &mut bytes,
        "homeassistant/statestream/light/all_lights/state",
        b"on",
        flags,
        None,
    )
    .unwrap();
    let event = Event::Stream {
        connection: wire.connection(),
        item: StreamItem::Received(bytes),
    };
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    // Home Assistant sends the state, then `last_updated`, as plain ISO
    // 8601 text with microseconds.
    let mut first = send(&mut model, &mut wire, &topic(VISIBLE, "state"), "on");
    let updated = "2026-07-01T08:41:21.123456+00:00";
    let mut second = send(
        &mut model,
        &mut wire,
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    change_and_draw(&mut model, &mut wire, VISIBLE, "on", WINTER)
        .unwrap()
        .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 08:41");
}

#[test]
fn a_hidden_change_gives_no_effects_and_shows_its_time_on_its_page() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW);

    // On the index page, the humidity of the living room changes two minutes
    // later. The index page does not show it, so the core starts no timer
    // and does not draw.
    let (mut first, mut second) = change(&mut model, &mut wire, HIDDEN, "40", super::NOW + 120);
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW);
    navigate(&mut model, Route::room("living_room"));

    // On the living room page, the humidity changes two minutes later.
    change_and_draw(&mut model, &mut wire, HIDDEN, "40", super::NOW + 120)
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW);

    // The summary of the Loft shows its first light.
    change_and_draw(
        &mut model,
        &mut wire,
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
        &mut wire,
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    // The seed automation sends each entity again when the dashboard
    // connects. The first change that the page shows starts the draw timer.
    let updated = super::broker::iso_time(super::NOW);
    let mut first = send(&mut model, &mut wire, &topic(VISIBLE, "state"), "on");
    for (entity_id, key, payload) in [
        (VISIBLE, "last_updated", updated.as_str()),
        (HIDDEN, "state", "40"),
        (HIDDEN, "last_updated", updated.as_str()),
        (WEATHER, "state", "rainy"),
        (WEATHER, "last_updated", updated.as_str()),
        (WEATHER, "temperature", "11.5"),
    ] {
        send(&mut model, &mut wire, &topic(entity_id, key), payload).expect_no_effects();
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
    let (mut first, _) = change(&mut model, &mut wire, VISIBLE, "off", super::NOW);
    let _due = expect_draw_timer(&mut first);
}

#[test]
fn a_last_updated_in_the_same_minute_as_the_page_time_draws_nothing() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW);

    // The same entity in the same minute is not a change, so the core
    // starts no timer.
    send(
        &mut model,
        &mut wire,
        &topic(VISIBLE, "last_updated"),
        &super::broker::iso_time(super::NOW + 40),
    )
    .expect_no_effects();

    // Another entity on the page gets its first time, in the same minute as
    // the footer time. The core starts the timer, but the view is the same,
    // so the timer draws nothing.
    let mut first = send(
        &mut model,
        &mut wire,
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW);

    for payload in [
        "",
        "yesterday",
        "\"2026-07-01T09:00:00+00:00\"",
        "1782895260",
    ] {
        send(
            &mut model,
            &mut wire,
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
    let (model, _wire) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.last_sync = Some(at(super::NOW));
    assert_eq!(app.view(&model).updated, "Last changed 09:41");

    // A change on the page has an earlier time than the kept time. The
    // footer shows the time from Home Assistant.
    change_and_draw(&mut model, &mut wire, VISIBLE, "on", super::NOW - 600)
        .unwrap()
        .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:31");
}
