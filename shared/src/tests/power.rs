//! Tests of the power feature: the idle timer, sleep and the power source.

use core::time::Duration;

use crux_core::App;

use super::{ExpectFullRefreshTimer, connection::drive_to_connected, start};
use crate::{
    Connection, Control, Dashboard, EffectTestExt, Event, Model, PowerSource,
    capabilities::{network::WifiStatus, power::WakeSource},
    features::{connection::Link, power::IDLE_TIMEOUT},
};

#[test]
fn start_on_battery_also_starts_the_idle_timer() {
    let app = Dashboard;
    let mut model = Model::default();

    app.update(start(PowerSource::Battery), &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_wifi_on()
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });
    assert_eq!(app.view(&model).power, PowerSource::Battery);
}

#[test]
fn the_idle_timeout_disconnects_and_sleeps() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    model.last_sync = chrono::DateTime::from_timestamp(super::NOW.cast_signed(), 0);

    // The core does not read the clock. The last image shows the time of the
    // last state message.
    app.update(Event::IdleTimeout(model.power.idle), &mut model)
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
    assert_eq!(view.updated, "Last changed 09:41");
    assert_eq!(model.connection.wifi, Link::Down);
}

#[test]
fn a_tap_restarts_the_idle_timer_and_the_old_timer_is_ignored() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    let old = model.power.idle;

    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
        });

    app.update(Event::IdleTimeout(old), &mut model)
        .expect_no_effects();
}

#[test]
fn on_the_stand_there_is_no_idle_timer() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);

    app.update(Event::NextPage, &mut model)
        .expect_render()
        .expect_full_refresh_timer();
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
    let _ = app.update(Event::IdleTimeout(model.power.idle), &mut model);
    assert!(model.power.asleep);

    let event = app
        .update(Event::PowerSourceChanged(PowerSource::Stand), &mut model)
        .expect_render()
        .expect_full_refresh_timer()
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
    let mut model = asleep_on_battery();

    app.update(Event::Woke, &mut model)
        .expect_render()
        .expect_full_refresh_timer()
        .expect_wifi_on()
        .expect_only_time_notify_after();
    assert_eq!(model.full_refresh_seq, 1);
}

/// Returns a model of a device on battery that sleeps, with settings.
fn asleep_on_battery() -> Model {
    Model {
        power: crate::features::power::State {
            source: PowerSource::Battery,
            asleep: true,
            ..crate::features::power::State::default()
        },
        settings: Some(super::settings()),
        ..Model::default()
    }
}

#[test]
fn a_tap_while_asleep_wakes_and_does_not_act_on_the_control() {
    let app = Dashboard;
    for control in [
        None,
        Some(Control::PreviousPage),
        Some(Control::Home),
        Some(Control::NextPage),
        Some(Control::Tile(0)),
        Some(Control::Scene(0)),
        Some(Control::Brighter(1)),
        Some(Control::Dimmer(1)),
    ] {
        let mut model = asleep_on_battery();
        // On this page, tile 1 is a dimmable light. While the device is
        // awake, each control here acts.
        model.route = crate::Route::room("living_room");
        super::set_state(&mut model, "light.living_room_rocket_lamp", "on");
        let route = model.route.clone();

        // The same effects as `Woke`: draw, connect and start the idle timer.
        app.update(Event::Tapped(control), &mut model)
            .expect_render()
            .expect_full_refresh_timer()
            .expect_wifi_on()
            .expect_only_time_notify_after_with(|notify| {
                assert_eq!(Duration::from(notify.duration), IDLE_TIMEOUT);
            });
        assert!(!model.power.asleep, "{control:?}");
        assert_eq!(model.full_refresh_seq, 1, "{control:?}");
        assert_eq!(model.route, route, "{control:?}");
        assert_eq!(model.connection.outbox.len(), 0, "{control:?}");
    }
}

#[test]
fn a_tap_on_no_control_while_awake_does_not_restart_the_idle_timer() {
    let app = Dashboard;
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    let idle = model.power.idle;

    // A touch by accident does not keep the device awake.
    app.update(Event::Tapped(None), &mut model)
        .expect_no_effects();
    assert_eq!(model.power.idle, idle);

    let _ = app.update(Event::IdleTimeout(idle), &mut model);
    assert!(model.power.asleep);
}

#[test]
fn page_events_while_asleep_do_nothing() {
    let app = Dashboard;
    let mut model = asleep_on_battery();

    for event in [
        Event::NextPage,
        Event::PreviousPage,
        Event::Navigate(crate::Route::room("kitchen")),
    ] {
        app.update(event, &mut model).expect_no_effects();
    }
    assert!(model.power.asleep);
}

#[test]
fn no_other_timer_waits_for_the_idle_timeout() {
    use crate::features::{
        connection::{RECONNECT_MAX, RECONNECT_MIN},
        refresh::{DRAW_DELAY, FULL_REFRESH_INTERVAL},
    };

    assert_ne!(DRAW_DELAY, IDLE_TIMEOUT);
    assert_ne!(FULL_REFRESH_INTERVAL, IDLE_TIMEOUT);
    let mut wait = RECONNECT_MIN;
    while wait < RECONNECT_MAX {
        assert_ne!(wait, IDLE_TIMEOUT);
        wait *= 2;
    }
    assert_ne!(RECONNECT_MAX, IDLE_TIMEOUT);
}
