//! Tests of the power feature: the idle timer, sleep and the power source.

use core::time::Duration;

use crux_core::App;

use super::{connection::drive_to_connected, instant, start};
use crate::{
    Connection, Dashboard, EffectTestExt, Event, Model, PowerSource,
    capabilities::{network::WifiStatus, power::WakeSource},
    features::{connection::Link, power::IDLE_TIMEOUT},
};

#[test]
fn start_on_battery_also_starts_the_idle_timer() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(start(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_wifi_on()
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });
    assert_eq!(app.view(&model).power, PowerSource::Battery);
}

#[test]
fn the_idle_timeout_reads_the_clock_then_disconnects_and_sleeps() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);

    let event = app
        .update(Event::IdleTimeout(model.power.idle), &mut model)
        .resolve_time_now(|_| instant(super::NOW))
        .expect_event();

    app.update(event, &mut model)
        .expect_mqtt_publish_with(|publish| {
            assert_eq!(publish.topic, "dashboard/hall/status");
            assert_eq!(publish.payload, b"asleep");
            assert!(publish.retain);
        })
        .expect_mqtt_disconnect()
        .expect_wifi_off()
        .expect_render()
        .expect_only_sleep_with(|sleep| assert_eq!(sleep.wake_on, [WakeSource::Touch]));

    let view = app.view(&model);
    assert!(view.asleep);
    // The start asked for the first full refresh, and the sleep for the second.
    assert_eq!(view.full_refresh_seq, 2);
    assert_eq!(view.connection, Connection::Offline);
    assert_eq!(view.updated, "Updated 09:41");
    assert_eq!(model.connection.wifi, Link::Down);
}

#[test]
fn a_tap_restarts_the_idle_timer_and_the_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    let old = model.power.idle;

    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });

    app.update(Event::IdleTimeout(old), &mut model)
        .expect_no_effects();
}

#[test]
fn a_tap_while_the_core_reads_the_clock_cancels_the_sleep() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);

    let event = app
        .update(Event::IdleTimeout(model.power.idle), &mut model)
        .resolve_time_now(|_| instant(super::NOW))
        .expect_event();
    let _ = app.update(Event::NextPage, &mut model);

    app.update(event, &mut model).expect_no_effects();
    assert!(!model.power.asleep);
}

#[test]
fn on_the_stand_there_is_no_idle_timer() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    app.update(Event::NextPage, &mut model).expect_only_render();
    app.update(Event::IdleTimeout(model.power.idle), &mut model)
        .expect_no_effects();
}

#[test]
fn off_the_stand_starts_the_idle_timer() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    app.update(Event::PowerSourceChanged(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_only_time_notify_after();
    assert_eq!(model.connection.mqtt, Link::Up);
}

#[test]
fn on_the_stand_while_asleep_wakes_and_connects() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    let event = app
        .update(Event::IdleTimeout(model.power.idle), &mut model)
        .resolve_time_now(|_| instant(super::NOW))
        .expect_event();
    let _ = app.update(event, &mut model);
    assert!(model.power.asleep);

    let event = app
        .update(Event::PowerSourceChanged(PowerSource::Stand), &mut model)
        .expect_render()
        .resolve_wifi_on(|_| WifiStatus::Connected)
        .expect_event();
    assert_eq!(model.full_refresh_seq, 3);
    assert!(!model.power.asleep);

    let _connect = app
        .update(event, &mut model)
        .expect_one_effect()
        .expect_mqtt_connect();
}

#[test]
fn woke_turns_wifi_on_and_starts_the_idle_timer() {
    let app = Dashboard;
    let mut model = Model {
        power: crate::features::power::State {
            source: PowerSource::Battery,
            asleep: true,
            ..crate::features::power::State::default()
        },
        settings: Some(super::settings()),
        ..Model::default()
    };

    app.update(Event::Woke, &mut model)
        .expect_render()
        .expect_wifi_on()
        .expect_only_time_notify_after();
    assert_eq!(model.full_refresh_seq, 1);
}

#[test]
fn taps_while_asleep_do_nothing() {
    let app = Dashboard;
    let mut model = Model::default();
    model.power.asleep = true;

    for event in [
        Event::NextPage,
        Event::PreviousPage,
        Event::TileTapped(0),
        Event::SceneTapped(0),
    ] {
        app.update(event, &mut model).expect_no_effects();
    }
}
