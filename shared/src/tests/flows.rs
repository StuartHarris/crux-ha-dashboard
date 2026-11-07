//! Flow tests: the core runs in the test shell against the fake broker and
//! the fake Home Assistant.

use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use super::{shell::Shell, start, start_at};
use crate::{
    Connection, Event, PowerSource, Route, TileState,
    capabilities::power::WakeSource,
    features::{
        connection::{RECONNECT_MAX, RECONNECT_MIN},
        power::IDLE_TIMEOUT,
    },
};

/// Home Assistant publishes the state of some entities before the dashboard
/// starts.
fn house() -> Shell {
    let mut shell = Shell::new();
    let broker = &mut shell.broker;
    broker.set_state(
        "light.all_lights",
        "on",
        &[("brightness", "255"), ("friendly_name", "\"All lights\"")],
    );
    broker.set_state(
        "weather.home",
        "cloudy",
        &[("temperature", "14.2"), ("temperature_unit", "\"°C\"")],
    );
    broker.set_state("light.living_room_lights", "off", &[("brightness", "null")]);
    broker.set_state(
        "light.living_room_rocket_lamp",
        "on",
        &[("brightness", "128")],
    );
    broker.set_state(
        "sensor.living_room_thermometer_temperature",
        "21.4",
        &[("unit_of_measurement", "\"°C\"")],
    );
    broker.set_state("light.living_room_standard_lamp", "unavailable", &[]);
    broker.set_state(
        "climate.dining_room_thermostat",
        "heat",
        &[("current_temperature", "19.5"), ("temperature", "21")],
    );
    shell
}

fn texts(shell: &Shell) -> Vec<(String, TileState, String)> {
    shell
        .view()
        .tiles
        .into_iter()
        .map(|tile| (tile.label, tile.state, tile.text))
        .collect()
}

fn tile(label: &str, state: TileState, text: &str) -> (String, TileState, String) {
    (label.into(), state, text.into())
}

#[test]
fn on_the_stand_the_dashboard_shows_live_state_and_controls_lights() {
    let mut shell = house();

    shell.send(start(PowerSource::Stand));

    assert!(shell.wifi_on);
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("online")
    );
    // The dashboard subscribes to the state and the attributes that it shows,
    // and gets no other retained attributes.
    let filters = shell.broker.filters();
    assert!(filters.iter().any(|filter| filter
        == "homeassistant/statestream/climate/dining_room_thermostat/current_temperature"));
    assert_ne!(shell.broker.delivered.len(), 0);
    let unwanted: Vec<_> = shell
        .broker
        .delivered
        .iter()
        .filter(|topic| topic.ends_with("friendly_name"))
        .collect();
    assert_eq!(unwanted, [] as [&String; 0]);
    let view = shell.view();
    assert_eq!(view.connection, Connection::Online);
    assert_eq!(view.updated, "Live");
    assert_eq!(
        texts(&shell),
        [
            tile("All lights", TileState::On, "On 100%"),
            tile("Weather", TileState::Value, "14.2 °C"),
        ]
    );
    assert_eq!(view.tiles[1].detail, "Cloudy");

    shell.send(Event::NextPage);
    assert_eq!(
        texts(&shell),
        [
            tile("Lights", TileState::Off, "Off"),
            tile("Rocket lamp", TileState::On, "On 50%"),
            tile("Standard lamp", TileState::Unavailable, "Unavailable"),
            tile("Temperature", TileState::Value, "21.4 °C"),
            tile("Humidity", TileState::Unknown, "-"),
        ]
    );

    // A tap goes to Home Assistant, and its new state comes back.
    shell.send(Event::TileTapped(0));
    assert_eq!(shell.broker.state("light.living_room_lights"), Some("on"));
    assert_eq!(shell.view().tiles[0].state, TileState::On);

    // A change in Home Assistant shows at once.
    shell
        .broker
        .set_state("light.living_room_rocket_lamp", "off", &[]);
    shell.run();
    assert_eq!(shell.view().tiles[1].text, "Off");
    assert!(!shell.frames.last().unwrap().full);

    shell.send(Event::SceneTapped(1));
    let call = shell.broker.service_calls.last().unwrap();
    assert_eq!(call.service, "scene.turn_on");
    assert_eq!(call.entity_id, "scene.living_room_living_room_dimmed");

    // On the stand nothing sleeps.
    assert_eq!(shell.timers().len(), 0);
    assert_eq!(shell.sleep, None);
}

#[test]
fn on_battery_the_dashboard_wakes_syncs_accepts_taps_then_sleeps() {
    let mut shell = house();

    shell.send(start_at(PowerSource::Battery, Some("/room/dining_room")));
    assert_eq!(shell.timers(), [IDLE_TIMEOUT]);
    assert_eq!(shell.view().tiles[4].text, "19.5°");
    assert_eq!(shell.view().tiles[4].detail, "Set 21°");

    shell.send(Event::TileTapped(1));
    assert_eq!(
        shell.broker.state("light.dining_room_tall_lamp"),
        Some("on")
    );
    assert_eq!(shell.view().tiles[1].text, "On");
    // The tap restarted the idle timer. The first timer is now stale.
    assert_eq!(shell.timers(), [IDLE_TIMEOUT, IDLE_TIMEOUT]);

    shell.take_log();
    shell.fire_timers();

    assert_eq!(
        shell.take_log(),
        [
            "TimeNow",
            "MqttPublish dashboard/hall/status asleep",
            "MqttDisconnect",
            "WifiOff",
            "Render",
            "Sleep",
        ]
    );
    assert_eq!(shell.sleep.as_ref().unwrap().wake_on, [WakeSource::Touch]);
    assert!(!shell.wifi_on);
    assert!(!shell.broker.is_connected());
    // A disconnect is not a lost connection, so the broker keeps `asleep`.
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("asleep")
    );

    let frame = shell.frames.last().unwrap();
    assert!(frame.full);
    let last = &frame.view;
    assert!(last.asleep);
    assert_eq!(last.connection, Connection::Offline);
    assert_eq!(last.updated, "Updated 09:41");
    assert_eq!(last.path, "/room/dining_room");
    assert_eq!(shell.timers().len(), 0);
}

#[test]
fn a_battery_wake_shows_the_changes_from_while_it_slept() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.fire_timers();
    assert!(shell.model.power.asleep);

    shell.broker.set_state("light.all_lights", "off", &[]);
    shell.broker.set_state("weather.home", "sunny", &[]);
    shell.clock += 3600;

    shell.send(Event::Woke);
    assert!(shell.wifi_on);
    assert_eq!(shell.broker.connections, 2);
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(
        texts(&shell),
        [
            tile("All lights", TileState::Off, "Off"),
            tile("Weather", TileState::Value, "14.2 °C"),
        ]
    );
    assert_eq!(shell.view().tiles[1].detail, "Sunny");

    shell.fire_timers();
    assert_eq!(shell.view().updated, "Updated 10:42");
}

#[test]
fn a_move_to_the_stand_while_asleep_connects_and_stays_awake() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.fire_timers();
    assert!(shell.model.power.asleep);

    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert!(shell.wifi_on);
    assert!(shell.broker.is_connected());
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("online")
    );
    assert_eq!(shell.timers().len(), 0);
    assert!(!shell.view().asleep);
}

#[test]
fn a_move_off_the_stand_sleeps_after_the_idle_timeout() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.timers().len(), 0);

    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert_eq!(shell.timers(), [IDLE_TIMEOUT]);
    assert!(shell.broker.is_connected());

    shell.fire_timers();
    assert!(shell.sleep.is_some());
    assert!(!shell.wifi_on);
}

#[test]
fn a_move_to_the_stand_cancels_the_idle_timeout() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));

    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    shell.fire_timers();

    assert_eq!(shell.sleep, None);
    assert!(shell.broker.is_connected());
}

#[test]
fn on_the_stand_a_lost_connection_reconnects_after_a_backoff() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));

    shell.broker.drop_connection();
    shell.run();
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("offline")
    );
    let view = shell.view();
    assert_eq!(view.connection, Connection::Connecting);
    assert_eq!(view.updated, "Updated 09:41");
    assert_eq!(shell.timers(), [RECONNECT_MIN]);

    // While the dashboard is offline, Home Assistant changes a light.
    shell.broker.set_state("light.all_lights", "off", &[]);

    shell.fire_timers();
    assert!(shell.broker.is_connected());
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("online")
    );
    assert_eq!(shell.view().tiles[0].text, "Off");
    assert_eq!(shell.view().updated, "Live");
}

#[test]
fn while_the_broker_is_down_the_wait_doubles_up_to_a_limit() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));

    let mut waits = vec![];
    for _ in 0..9 {
        waits.extend(shell.timers());
        shell.fire_timers();
    }
    let secs = |seconds| Duration::from_secs(seconds);
    assert_eq!(
        waits,
        [
            secs(1),
            secs(2),
            secs(4),
            secs(8),
            secs(16),
            secs(32),
            RECONNECT_MAX,
            RECONNECT_MAX,
            RECONNECT_MAX
        ]
    );

    shell.broker.down = false;
    shell.fire_timers();
    assert!(shell.broker.is_connected());
    assert_eq!(shell.model.connection.backoff, RECONNECT_MIN);
}

#[test]
fn taps_without_a_connection_are_sent_when_the_core_connects() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));

    shell.send(Event::TileTapped(0));
    shell.send(Event::Navigate(Route::room("loft")));
    shell.send(Event::SceneTapped(0));
    assert_eq!(shell.broker.service_calls.len(), 0);

    shell.broker.down = false;
    shell.fire_timers();
    let calls: Vec<_> = shell
        .broker
        .service_calls
        .iter()
        .map(|call| (call.service.as_str(), call.entity_id.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("light.toggle", "light.all_lights"),
            ("scene.turn_on", "scene.loft_loft_bright"),
        ]
    );
    assert_eq!(shell.broker.state("light.all_lights"), Some("off"));
}

#[test]
fn wifi_that_fails_on_battery_wake_is_tried_again_until_the_device_sleeps() {
    let mut shell = house();
    shell.wifi_works = false;
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.timers(), [IDLE_TIMEOUT, RECONNECT_MIN]);
    assert_eq!(shell.view().connection, Connection::Connecting);

    shell.fire_timers();
    assert!(shell.sleep.is_some());
    // The core never connected, so it has no time of an update.
    assert_eq!(shell.view().updated, "");

    // A reconnect timer from before the sleep is stale, so the device stays
    // asleep with Wi-Fi off.
    shell.take_log();
    shell.fire_timers();
    assert!(shell.model.power.asleep);
    assert!(!shell.take_log().iter().any(|effect| effect == "WifiOn"));
    assert_eq!(shell.timers().len(), 0);
}

#[test]
fn a_full_refresh_is_kept_when_a_partial_redraw_follows_before_the_shell_draws() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert!(shell.frames.last().unwrap().full);
    shell.send(Event::TileTapped(0));
    assert!(!shell.frames.last().unwrap().full);

    // The page changes, and a message for the new page arrives before the
    // shell draws. The shell draws once, with a full refresh.
    let frames = shell.frames.len();
    shell.queue(Event::NextPage);
    shell
        .broker
        .set_state("light.living_room_lights", "on", &[("brightness", "26")]);
    shell.run();
    assert_eq!(shell.frames.len(), frames + 1);
    let frame = shell.frames.last().unwrap();
    assert!(frame.full);
    assert_eq!(frame.view.tiles[0].text, "On 10%");
}

#[test]
fn on_the_stand_a_wifi_drop_asks_for_wifi_again_before_it_reconnects() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));

    // Wi-Fi drops, so the connection drops too.
    shell.wifi_on = false;
    shell.broker.drop_connection();
    shell.run();
    shell.take_log();

    shell.fire_timers();
    let log = shell.take_log();
    let wifi = log.iter().position(|effect| effect == "WifiOn");
    let connect = log.iter().position(|effect| effect == "MqttConnect");
    assert!(wifi.is_some() && wifi < connect, "{log:?}");
    assert!(shell.wifi_on);
    assert!(shell.broker.is_connected());
}

#[test]
fn on_battery_a_tap_that_waits_for_a_connection_delays_the_sleep_once() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Battery));
    shell.send(Event::TileTapped(0));
    assert_eq!(shell.model.connection.outbox.len(), 1);

    // The first idle timeout waits one more idle period for the connection.
    shell.fire_timers();
    assert_eq!(shell.sleep, None);
    assert!(shell.timers().contains(&IDLE_TIMEOUT));

    // The connection still fails, so the core drops the tap and sleeps.
    shell.fire_timers();
    assert!(shell.sleep.is_some());
    let view = shell.view();
    assert_eq!(view.status, "Asleep, on battery, 1 tap not sent");
    assert_eq!(shell.broker.service_calls.len(), 0);

    // The next connection clears the notice.
    shell.broker.down = false;
    shell.send(Event::Woke);
    assert_eq!(shell.view().status, "Online, on battery");
}

#[test]
fn on_battery_a_tap_that_is_sent_before_the_extra_period_ends_is_not_lost() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Battery));
    shell.send(Event::TileTapped(0));
    shell.fire_timers();
    assert_eq!(shell.sleep, None);

    // The reconnect timer fires before the extra idle period ends.
    shell.broker.down = false;
    shell.fire_shortest_timers();
    assert_eq!(shell.broker.service_calls.len(), 1);
    assert_eq!(shell.sleep, None);

    shell.fire_timers();
    assert!(shell.sleep.is_some());
    assert_eq!(shell.view().status, "Asleep, on battery");
}

#[test]
fn a_wake_after_a_long_outage_retries_after_the_shortest_wait() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));
    for _ in 0..8 {
        shell.fire_timers();
    }
    assert_eq!(shell.model.connection.backoff, RECONNECT_MAX);

    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    while shell.sleep.is_none() {
        shell.fire_timers();
    }
    shell.send(Event::Woke);
    assert!(
        shell.timers().contains(&RECONNECT_MIN),
        "{:?}",
        shell.timers()
    );
}

#[test]
fn after_a_deep_sleep_the_view_shows_the_last_update_that_the_shell_kept() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.fire_timers();
    let kept = shell.view().last_sync;
    assert_eq!(kept, Some(super::NOW + 30));

    // A deep sleep loses the core. The shell starts a new core with the
    // values that it kept, and the broker is not there.
    let mut shell = house();
    shell.broker.down = true;
    shell.send(crate::Event::Start(crate::Start {
        last_sync: kept,
        ..start_parts(PowerSource::Battery)
    }));
    assert_eq!(shell.view().updated, "Updated 09:41");
}

fn start_parts(power: PowerSource) -> crate::Start {
    let Event::Start(start) = start(power) else {
        unreachable!()
    };
    start
}
