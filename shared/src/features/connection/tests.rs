//! Tests of the connection feature, one step at a time.

use alloc::{format, vec, vec::Vec};
use core::time::Duration;

use crux_core::App;

use crate::tests::{
    ExpectFullRefreshTimer, ExpectLight, ExpectRadioLimit,
    broker::publish_of,
    drive_to_connected, drive_to_connecting,
    flows::house,
    link_up, send, start, tap_all_lights, topic,
    wire::{ExpectWrite, Wire},
};
use crate::{
    Cmd, Connection, Dashboard, Effect, EffectTestExt, Event, Model, PowerSource, Route, TileState,
    capabilities::{
        log::Area,
        radio::{Joined, Restart, Target},
        stream::{StreamItem, Written},
    },
    features::{
        connection::{self, CLOSE_TIMEOUT, Call, Link, RECONNECT_MAX, RECONNECT_MIN},
        device::{self, Button},
        light, power,
        refresh::DRAW_DELAY,
        wifi::{JOIN_TIMEOUT, Phase},
    },
    mqtt::{
        Publish, Qos,
        codec::{self, Flags, server::Packet},
    },
    view::LightState::{On, Unavailable},
};

/// The time `seconds` after the Unix epoch.
fn at(seconds: u64) -> jiff::Timestamp {
    jiff::Timestamp::from_second(seconds.cast_signed()).unwrap()
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
    let updated = crate::tests::broker::iso_time(time);
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
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), JOIN_TIMEOUT);
        })
        .expect_only_log_with(|log| {
            assert_eq!(log.to_string(), "power: started after a power-on");
        });

    let view = app.view(&model);
    assert_eq!(view.full_refresh.seq, 1);
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
        .expect_render()
        .expect_only_session_clear(&wire);

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
    assert_eq!(view.full_refresh.seq, 1);
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
    let (mut first, mut second) = change(
        &mut model,
        &mut wire,
        "light.loft_loft_1",
        "on",
        crate::tests::NOW,
    );
    first.expect_no_effects();
    second.expect_no_effects();
    let loft = model.entities.get("light.loft_loft_1").unwrap();
    assert_eq!(loft.state.as_deref(), Some("on"));
    assert_eq!(loft.last_updated_minute(), Some(at(crate::tests::NOW)));
}

#[test]
fn on_the_index_a_message_draws_only_if_a_summary_changes() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let lights = |model: &Model| app.view(model).index().unwrap().rooms[7].lights.clone();
    assert_eq!(lights(&model), [Unavailable; 3]);

    // The summary of the Loft shows the new state. The draw timer draws once.
    change_and_draw(
        &mut model,
        &mut wire,
        "light.loft_loft_1",
        "on",
        crate::tests::NOW,
    )
    .unwrap()
    .expect_only_render();
    assert_eq!(lights(&model), [On, Unavailable, Unavailable]);
    assert_eq!(app.view(&model).full_refresh.seq, 1);

    // No summary shows the group of the Loft, because the Loft has the
    // members of the group. The core does not draw.
    assert!(
        change_and_draw(
            &mut model,
            &mut wire,
            "light.loft_lights",
            "on",
            crate::tests::NOW,
        )
        .is_none()
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
        let write = Event::Connection(connection::Event::Written {
            connection,
            outcome: Written::Failed("write failed".into()),
        });
        let end = Event::Connection(connection::Event::Stream {
            connection,
            item: StreamItem::by_host("the broker closed the stream"),
        });
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
        assert_eq!(model.connection.link(), Link::Down);
        assert!(!model.wifi.health.suspect(), "failed first: {failed_first}");
    }
}

#[test]
fn a_lost_connection_shows_the_time_of_the_last_message_and_retries_after_a_backoff() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", crate::tests::NOW);
    let connection = model.connection.id;

    // The time is the time of the last update from Home Assistant, not the
    // time of the disconnect. The link is suspect, and the core closes the
    // stream, which failed.
    let reconnect = wire
        .drop_connection(&mut model, "connection lost")
        .expect_log_with(|log| {
            assert_eq!(
                log.to_string(),
                "wifi: the link is suspect, because the MQTT session failed: the stream closed: connection lost"
            );
        })
        .expect_stream_close_with(|close| assert!(!close.clean))
        .expect_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), CLOSE_TIMEOUT);
        })
        .expect_render()
        .expect_log_with(|log| {
            assert_eq!(log.to_string(), "mqtt: the stream closed: connection lost, so connect again in 1 s");
        })
        .resolve_time_notify_after(|notify| {
            assert_eq!(Duration::from(notify.duration), RECONNECT_MIN);
            notify.id
        })
        .expect_event();
    let view = app.view(&model);
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.updated, "Last changed 09:41");

    // The shell answers the `Close` request. The core waits for the backoff.
    app.update(
        Event::Connection(connection::Event::StreamClosed { connection }),
        &mut model,
    )
    .expect_no_effects();

    // The lost connection makes the link suspect, so the reconnect leaves
    // and joins again first.
    let mut cmd = app.update(reconnect, &mut model);
    cmd.expect_log_with(|log| {
        assert_eq!(
            log.to_string(),
            "wifi: the link is suspect, so leave 02:00:00:00:00:01 on channel 6 and join again"
        );
    });
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
                Event::Connection(connection::Event::Stream {
                    connection,
                    item: refused,
                }),
                &mut model,
            )
            .expect_stream_close_with(|close| assert!(!close.clean))
            .expect_time_notify_after_with(|notify| {
                assert_eq!(Duration::from(notify.duration), CLOSE_TIMEOUT);
            })
            // The view still shows `Connecting`, so the core does not draw.
            .expect_log_with(|log| {
                assert_eq!(
                    log.to_string(),
                    format!(
                        "mqtt: the broker host ended the stream: connection refused, so connect again in {} s",
                        expected.as_secs()
                    )
                );
            })
            .resolve_time_notify_after(|notify| {
                assert_eq!(Duration::from(notify.duration), expected);
                notify.id
            })
            .expect_event();
        app.update(
            Event::Connection(connection::Event::StreamClosed { connection }),
            &mut model,
        )
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
        Event::Connection(connection::Event::Stream {
            connection: old,
            item: StreamItem::closed("connection lost"),
        }),
        &mut model,
    )
    .expect_no_effects();
    assert_eq!(model.connection.link(), Link::Up);
}

#[test]
fn a_stale_reconnect_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let stale = model.connection.reconnect.wrapping_sub(1);

    app.update(
        Event::Connection(connection::Event::Reconnect(stale)),
        &mut model,
    )
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
        .expect_log_with(|log| {
            assert_eq!(
                log.to_string(),
                "wifi: the join to the strongest access point failed, so the link is down"
            );
        })
        .expect_log_with(|log| {
            assert_eq!(
                log.to_string(),
                "mqtt: the link is down, so connect again in 1 s"
            );
        })
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
    let event = Event::Connection(connection::Event::Stream {
        connection: wire.connection(),
        item: StreamItem::Received(bytes),
    });
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
    assert_eq!(app.view(&model).last_sync, Some(crate::tests::NOW));
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
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", crate::tests::NOW);

    // On the index page, the humidity of the living room changes two minutes
    // later. The index page does not show it, so the core starts no timer
    // and does not draw.
    let (mut first, mut second) =
        change(&mut model, &mut wire, HIDDEN, "40", crate::tests::NOW + 120);
    first.expect_no_effects();
    second.expect_no_effects();
    assert_eq!(app.view(&model).updated, "Last changed 09:41");

    // The living room page shows the time of that change.
    navigate(&mut model, Route::room("living_room"));
    assert_eq!(app.view(&model).updated, "Last changed 09:43");
    assert_eq!(app.view(&model).last_sync, Some(crate::tests::NOW + 120));
}

#[test]
fn a_page_keeps_its_own_older_time_after_a_page_turn() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", crate::tests::NOW);
    navigate(&mut model, Route::room("living_room"));

    // On the living room page, the humidity changes two minutes later.
    change_and_draw(&mut model, &mut wire, HIDDEN, "40", crate::tests::NOW + 120)
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
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", crate::tests::NOW);

    // The summary of the Loft shows its first light.
    change_and_draw(
        &mut model,
        &mut wire,
        "light.loft_loft_1",
        "on",
        crate::tests::NOW + 120,
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
        &crate::tests::broker::iso_time(crate::tests::NOW + 180),
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
    let updated = crate::tests::broker::iso_time(crate::tests::NOW);
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
    let (mut first, _) = change(&mut model, &mut wire, VISIBLE, "off", crate::tests::NOW);
    let _due = expect_draw_timer(&mut first);
}

#[test]
fn a_last_updated_in_the_same_minute_as_the_page_time_draws_nothing() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", crate::tests::NOW);

    // The same entity in the same minute is not a change, so the core
    // starts no timer.
    send(
        &mut model,
        &mut wire,
        &topic(VISIBLE, "last_updated"),
        &crate::tests::broker::iso_time(crate::tests::NOW + 40),
    )
    .expect_no_effects();

    // Another entity on the page gets its first time, in the same minute as
    // the footer time. The core starts the timer, but the view is the same,
    // so the timer draws nothing.
    let mut first = send(
        &mut model,
        &mut wire,
        &topic(WEATHER, "last_updated"),
        &crate::tests::broker::iso_time(crate::tests::NOW + 30),
    );
    let due = expect_draw_timer(&mut first);
    app.update(due, &mut model).expect_no_effects();
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
}

#[test]
fn a_last_updated_that_is_not_valid_is_ignored() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let _ = change_and_draw(&mut model, &mut wire, VISIBLE, "on", crate::tests::NOW);

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
        model.entities.get(VISIBLE).unwrap().last_updated_minute(),
        Some(at(crate::tests::NOW))
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
        last_sync: Some(crate::tests::NOW),
        ..start
    });
    let _effects: Vec<_> = app.update(start, &mut model).effects().collect();
    // No entity has a `last_updated` time, so the footer shows the kept time.
    assert_eq!(model.entities.last_updated_minutes().count(), 0);
    assert_eq!(app.view(&model).updated, "Last changed 09:41");
    assert_eq!(app.view(&model).last_sync, Some(crate::tests::NOW));
}

#[test]
fn a_time_from_home_assistant_replaces_the_kept_time() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    model.last_sync = Some(at(crate::tests::NOW));
    assert_eq!(app.view(&model).updated, "Last changed 09:41");

    // A change on the page has an earlier time than the kept time. The
    // footer shows the time from Home Assistant.
    change_and_draw(
        &mut model,
        &mut wire,
        VISIBLE,
        "on",
        crate::tests::NOW - 600,
    )
    .unwrap()
    .expect_only_render();
    assert_eq!(app.view(&model).updated, "Last changed 09:31");
}

#[test]
fn a_service_call_that_is_too_long_for_a_packet_is_dropped_with_a_line() {
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let call = Call {
        publish: Publish {
            // A topic must fit a 16-bit length.
            topic: "t".repeat(usize::from(u16::MAX) + 1),
            payload: vec![],
            qos: Qos::AtMostOnce,
            retain: false,
        },
        entity_id: "light.all_lights",
    };
    let mut cmd = connection::send(call, None, &mut model);
    cmd.expect_log_with(|log| {
        assert_eq!(
            log.to_string(),
            "mqtt: the service call for light.all_lights is too long for a packet, so the core dropped it"
        );
    });
    // The core did not keep the call.
    assert!(model.connection.outbox.is_empty());
}

#[test]
fn a_service_call_that_is_too_long_in_the_outbox_is_dropped_with_a_line_when_the_core_connects() {
    let (mut model, mut wire) = drive_to_connecting(PowerSource::Stand);
    let call = Call {
        publish: Publish {
            topic: "t".repeat(usize::from(u16::MAX) + 1),
            payload: vec![],
            qos: Qos::AtMostOnce,
            retain: false,
        },
        entity_id: "light.all_lights",
    };
    let _ = connection::send(call, None, &mut model);
    assert_eq!(model.connection.outbox.len(), 1);

    // The CONNACK drains the outbox. The call cannot go, so the core drops
    // it, and the `Write` has only `online` and the subscribe.
    wire.connect(&mut model)
        .expect_write_with(&mut wire, &mut model, |_| {})
        .expect_log_with(|log| {
            assert_eq!(
                log.to_string(),
                "mqtt: the service call for light.all_lights is too long for a packet, so the core dropped it"
            );
        })
        .expect_render()
        .expect_only_session_clear(&wire);
    assert!(model.connection.outbox.is_empty());
}

#[test]
fn the_answer_to_close_clears_the_close_timeout() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.take_log();

    shell.send(Event::SleepNow);
    let log = shell.take_log();
    let close = format!("TimeNotifyAfter {}ms", CLOSE_TIMEOUT.as_millis());
    let timer = log
        .iter()
        .position(|effect| *effect == close)
        .unwrap_or_else(|| panic!("no close timeout in {log:?}"));
    assert_eq!(log[timer + 1], "TimeClear", "{log:?}");

    let events = shell.advance(CLOSE_TIMEOUT);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Connection(connection::Event::CloseTimeout { .. })
        )),
        "{events:?}"
    );
}

#[test]
fn a_sleep_clears_the_reconnect_timer() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.broker.down = true;
    shell.take_log();
    shell.broker.drop_connection();
    shell.run();
    let wait = format!("TimeNotifyAfter {}ms", RECONNECT_MIN.as_millis());
    let log = shell.take_log();
    assert!(log.contains(&wait), "{log:?}");
    let reconnect = shell.model.connection.reconnect;

    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    let events = shell.advance(RECONNECT_MAX);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Connection(connection::Event::Reconnect(id)) if *id == reconnect)),
        "{events:?}"
    );
}

#[test]
fn the_core_clears_the_session_timers_that_the_session_no_longer_uses() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    // The ping, silence and ack timers wait. The core cleared the open,
    // CONNACK and send timers.
    assert_eq!(shell.session_timer_count(), 3);

    // Each write starts a send timer, and the answer to the write cancels
    // it.
    for _ in 0..3 {
        shell.send(tap_all_lights());
    }
    assert_eq!(shell.session_timer_count(), 3);

    // The session ends, so the core clears all its timers.
    shell.broker.drop_connection();
    shell.run();
    assert_eq!(shell.session_timer_count(), 0);
}

/// Returns `true` if `effect` is a radio request.
const fn is_radio(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::RadioJoin(_)
            | Effect::RadioAddress(_)
            | Effect::RadioLeave(_)
            | Effect::RadioScan(_)
            | Effect::RadioStopScan(_)
            | Effect::RadioReadSignal(_)
            | Effect::RadioOff(_)
            | Effect::RadioRestart(_)
    )
}

/// Ends the session with a failure, and keeps the `Close` request of its
/// stream without an answer, so the old stream drains. Returns the
/// command that holds the `Close` request.
fn drive_to_a_drain_after_a_failure() -> (Model, Cmd) {
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let dropped = wire.drop_connection(&mut model, "connection lost");
    assert_ne!(Dashboard.view(&model).connection, Connection::Online);
    (model, dropped)
}

/// Answers the `Close` request in `dropped`, and returns the command of the
/// update for the answer.
#[track_caller]
fn answer_the_close(dropped: &mut Cmd, model: &mut Model) -> Cmd {
    let mut close = dropped
        .effects()
        .find_map(|effect| match effect {
            Effect::StreamClose(request) => Some(request),
            _ => None,
        })
        .expect("a Close request");
    close.resolve(()).unwrap();
    let answer = dropped
        .events()
        .find(|event| {
            matches!(
                event,
                Event::Connection(connection::Event::StreamClosed { .. })
            )
        })
        .expect("the answer to the Close request");
    Dashboard.update(answer, model)
}

/// A tap while the old stream drains asks for no link. A radio
/// request can stop the old stream before its last bytes go. The core asks
/// for the link at the answer to the `Close` request.
#[test]
fn a_tap_while_the_old_stream_drains_makes_no_radio_request() {
    let (mut model, mut dropped) = drive_to_a_drain_after_a_failure();

    let mut tap = Dashboard.update(tap_all_lights(), &mut model);
    let radio = tap.effects().filter(is_radio).count();
    assert_eq!(radio, 0);
    assert_eq!(model.connection.outbox.len(), 1);

    // The link is suspect, so the core leaves and joins again.
    let mut answer = answer_the_close(&mut dropped, &mut model);
    let radio = answer.effects().filter(is_radio).count();
    assert_eq!(radio, 1);
}

/// A link while the old stream drains opens no stream. A new `Open`
/// can stop the old stream before its last bytes go.
#[test]
fn a_link_while_the_old_stream_drains_opens_no_stream() {
    let (mut model, _dropped) = drive_to_a_drain_after_a_failure();

    let mut cmd = connection::link_up(&mut model);
    let opens = cmd
        .effects()
        .filter(|effect| matches!(effect, Effect::StreamOpen(_)))
        .count();
    assert_eq!(opens, 0);
    assert_ne!(Dashboard.view(&model).connection, Connection::Online);
}

/// A link while the old stream drains after a failure makes the core
/// connect at the answer to the `Close` request. It does not wait for the
/// end of the backoff.
#[test]
fn a_link_while_the_old_stream_drains_connects_at_the_answer_to_the_close() {
    let (mut model, mut dropped) = drive_to_a_drain_after_a_failure();
    let _ = connection::link_up(&mut model);

    // The link is suspect, so the core leaves and joins again at once.
    let mut answer = answer_the_close(&mut dropped, &mut model);
    let radio = answer.effects().filter(is_radio).count();
    assert_eq!(radio, 1);
}

/// A sleep during the close for a change of access point replaces the
/// reconnect. The answer to the `Close` request continues the sleep, and
/// the core does not connect again.
#[test]
fn a_sleep_during_the_close_for_a_roam_goes_on_at_the_answer_to_the_close() {
    let (mut model, mut wire) = drive_to_connected(PowerSource::Battery);
    // The DISCONNECT of the roam waits for the answer to its `Write`.
    let roam = connection::close_for_roam(&mut model);
    assert!(!light::is_on(&model));
    let sleep = Dashboard.update(Event::SleepNow, &mut model);
    assert!(model.power.is_asleep());
    let id = model.power.sleep;

    wire.settle(roam, &mut model);
    wire.settle(sleep, &mut model);
    assert!(
        wire.events
            .contains(&Event::Power(power::Event::Offline(id))),
        "{:?}",
        wire.events
    );
    assert!(
        !wire
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::StreamOpen(_)) || is_radio(effect)),
        "{:?}",
        wire.effects
    );
}

/// Returns the `Restart` notifications in `effects`.
fn restarts<'a>(effects: impl Iterator<Item = &'a Effect>) -> Vec<Restart> {
    effects
        .filter_map(|effect| match effect {
            Effect::RadioRestart(request) => Some(request.operation.clone()),
            _ => None,
        })
        .collect()
}

/// The restart that Home Assistant asks for.
fn home_assistant_restart() -> Restart {
    Restart {
        area: Area::Device,
        reason: connection::RESTART_REASON.into(),
    }
}

/// A restart during the close for a change of access point replaces the
/// reconnect. The chip restarts at the answer to the `Close` request, and
/// the core does not connect again.
#[test]
fn a_restart_during_a_roam_close_restarts_and_does_not_connect() {
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);
    let roam = connection::close_for_roam(&mut model);
    let restart = Dashboard.update(
        Event::Device(device::Event::Pressed(Button::Restart)),
        &mut model,
    );
    assert!(model.connection.restart_waits());

    wire.settle(roam, &mut model);
    wire.settle(restart, &mut model);
    assert_eq!(restarts(wire.effects.iter()), [home_assistant_restart()]);
    assert!(
        !wire
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::StreamOpen(_))
                || (is_radio(effect) && !matches!(effect, Effect::RadioRestart(_)))),
        "{:?}",
        wire.effects
    );
    assert_eq!(model.wifi.phase, Phase::Restarting);
}

/// A restart while the old stream drains after a failure stops the
/// reconnect timer. The chip restarts at the answer to the `Close` request.
#[test]
fn a_restart_while_the_stream_drains_after_a_failure_restarts_at_the_answer() {
    let (mut model, mut dropped) = drive_to_a_drain_after_a_failure();
    let mut restart = Dashboard.update(
        Event::Device(device::Event::Pressed(Button::Restart)),
        &mut model,
    );
    // The restart waits for the answer. The reconnect timer stops through
    // its handle, so the command has no effects.
    restart.expect_no_effects();
    assert!(model.connection.restart_waits());

    let mut answer = answer_the_close(&mut dropped, &mut model);
    let effects: Vec<Effect> = answer.effects().collect();
    assert_eq!(restarts(effects.iter()), [home_assistant_restart()]);
    assert_eq!(effects.iter().filter(|effect| is_radio(effect)).count(), 1);
}

/// A state message after the stream opened and before the CONNACK
/// does not change the entities.
#[test]
fn a_message_before_the_connack_does_not_change_the_entities() {
    let app = Dashboard;
    let (mut model, wire) = drive_to_connecting(PowerSource::Stand);
    let connection = wire.connection();
    let opened = Event::Connection(connection::Event::Stream {
        connection,
        item: StreamItem::Opened,
    });
    let _effects: Vec<_> = app.update(opened, &mut model).effects().collect();

    let mut bytes = Vec::new();
    let flags = Flags {
        qos: Qos::AtMostOnce,
        retain: false,
    };
    codec::publish(&mut bytes, &topic(VISIBLE, "state"), b"on", flags, None).unwrap();
    let message = Event::Connection(connection::Event::Stream {
        connection,
        item: StreamItem::Received(bytes),
    });
    // The session gets the message, and ends with a failure.
    let closes: Vec<_> = app
        .update(message, &mut model)
        .effects()
        .filter_map(|effect| match effect {
            Effect::StreamClose(request) => Some(request.operation),
            _ => None,
        })
        .collect();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0].connection, connection);
    assert!(!closes[0].clean);
    assert_eq!(model.entities.get(VISIBLE), None);
    assert_ne!(app.view(&model).connection, Connection::Online);
}

/// During the close for a change of access point, the view shows that
/// the core connects, and a state message does not change the entities.
#[test]
fn a_close_for_a_roam_shows_connecting_and_drops_the_messages() {
    let app = Dashboard;
    let (mut model, mut wire) = drive_to_connected(PowerSource::Stand);

    // The DISCONNECT waits, so the session stays open in its close.
    let mut roam = connection::close_for_roam(&mut model);
    assert!(
        roam.effects()
            .any(|effect| matches!(effect, Effect::StreamWrite(_)))
    );
    assert_eq!(model.connection.link(), Link::Down);
    assert_eq!(app.view(&model).connection, Connection::Connecting);

    send(&mut model, &mut wire, &topic(VISIBLE, "state"), "on").expect_no_effects();
    assert_eq!(model.entities.get(VISIBLE), None);
}

#[test]
fn a_stream_failure_waits_1_2_4_s_before_each_retry() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connecting(PowerSource::Stand);

    for expected in [1, 2, 4] {
        let connection = model.connection.id;
        let mut cmd = app.update(
            Event::Connection(connection::Event::Stream {
                connection,
                item: StreamItem::by_host("connection refused"),
            }),
            &mut model,
        );
        // The retry timer is the timer that is not the time limit of the
        // close.
        let mut retry = cmd
            .effects()
            .find_map(|effect| match effect {
                Effect::TimeNotifyAfter(request)
                    if Duration::from(request.operation.duration) != CLOSE_TIMEOUT =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("a retry timer");
        assert_eq!(
            Duration::from(retry.operation.duration),
            Duration::from_secs(expected)
        );
        let id = retry.operation.id;
        retry.resolve(id).expect("the core waits for the timer");
        let reconnect = cmd.expect_event();
        let _ = app.update(
            Event::Connection(connection::Event::StreamClosed { connection }),
            &mut model,
        );
        let mut cmd = app.update(reconnect, &mut model);
        let _ = link_up(&mut cmd, &mut model);
    }
}

#[test]
fn a_clean_end_connects_again_at_once_with_no_wait() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.broker.hold_writes = true;
    shell.broker.hold_closes = true;
    shell.advance(crate::features::power::IDLE_TIMEOUT);
    // A tap wakes the device while the DISCONNECT of the sleep waits.
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    shell.take_log();

    // The DISCONNECT leaves, so the session ends clean. The core starts no
    // retry timer.
    shell.broker.release_writes();
    shell.run();
    assert!(shell.closes.last().expect("a Close request").clean);
    shell.broker.release_closes();
    shell.run();
    let log = shell.take_log();
    let retry = format!("TimeNotifyAfter {}ms", RECONNECT_MIN.as_millis());
    assert!(!log.contains(&retry), "{log:?}");
    assert!(log.contains(&"MqttConnect".into()), "{log:?}");
    assert_eq!(shell.view().connection, Connection::Online);
    let events = shell.advance(RECONNECT_MAX);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_)))),
        "{events:?}"
    );
}

/// Returns the lines of `logs` with the area `mqtt`.
fn mqtt_lines(logs: &[alloc::string::String]) -> Vec<&str> {
    logs.iter()
        .map(alloc::string::String::as_str)
        .filter(|line| {
            line.split_once(' ')
                .is_some_and(|(_, rest)| rest.starts_with("mqtt:"))
        })
        .collect()
}

#[test]
fn a_stream_that_the_host_ends_writes_one_line_with_the_reason_and_the_wait() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.view().connection, Connection::Online);
    shell.take_logs();

    // The broker stops: the host ends the stream, and the next connect is
    // refused.
    shell.broker.stop();
    shell.run();
    let logs = shell.take_logs();
    assert_eq!(
        mqtt_lines(&logs),
        [
            "warn mqtt: the broker host ended the stream: the broker closed the stream, so connect again in 1 s"
        ],
        "{logs:?}"
    );
    shell.advance(RECONNECT_MIN);
    let logs = shell.take_logs();
    assert_eq!(
        mqtt_lines(&logs),
        [
            "warn mqtt: the broker host ended the stream: connection refused, so connect again in 2 s"
        ],
        "{logs:?}"
    );
    // The ring keeps each line as a warning.
    assert!(
        shell
            .model
            .logs
            .lines()
            .all(|line| line.level == crate::features::logs::Kept::Warn),
    );
}

#[test]
fn a_stream_failure_in_the_steps_of_a_sleep_writes_one_line_and_does_not_retry() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.take_logs();
    // The write of `asleep` and the DISCONNECT does not end, so the send
    // timer of the session ends it.
    shell.broker.hold_writes = true;
    shell.advance(crate::features::power::IDLE_TIMEOUT);
    shell.advance_session(crate::mqtt::SEND_TIMEOUT);
    assert!(shell.sleep.is_some());
    let logs = shell.take_logs();
    assert_eq!(
        mqtt_lines(&logs),
        ["warn mqtt: the shell did not send the bytes in time, during the sleep"],
        "{logs:?}"
    );
    let events = shell.advance(RECONNECT_MAX);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_)))),
        "{events:?}"
    );
}

#[test]
fn a_close_with_no_answer_in_the_steps_of_a_sleep_writes_a_warning() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.take_logs();
    shell.broker.hold_closes = true;
    shell.advance(crate::features::power::IDLE_TIMEOUT);
    assert!(shell.sleep.is_none());

    // After the time limit, the sleep continues, and the core writes a
    // warning.
    shell.advance(CLOSE_TIMEOUT);
    assert!(shell.sleep.is_some());
    let logs = shell.take_logs();
    assert_eq!(
        mqtt_lines(&logs),
        ["warn mqtt: the close of connection 2 got no answer in 6 s"],
        "{logs:?}"
    );
}

#[test]
fn a_close_with_an_answer_in_time_writes_no_line() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.take_logs();
    shell.advance(crate::features::power::IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    let logs = shell.take_logs();
    assert_eq!(mqtt_lines(&logs), Vec::<&str>::new(), "{logs:?}");
    assert_eq!(shell.model.logs.lines().len(), 0);
}
