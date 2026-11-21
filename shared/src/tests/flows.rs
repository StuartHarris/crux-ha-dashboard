//! Flow tests: the core runs in the test shell against the fake broker and
//! the fake Home Assistant.

use crate::features::{connection, pending, power, wifi};
use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use super::{
    shell::Shell, start, start_at, tap_all_lights, tap_minus, tap_plus, tap_room, tap_scene,
    tap_tile,
};
use crate::{
    Connection, Event, LightState,
    LightState::{Off, On, Unavailable},
    PowerSource, RoomSummary, Route, TileState,
    capabilities::power::{Wake, WakeSource},
    features::{
        connection::{RECONNECT_MAX, RECONNECT_MIN},
        pending::{PENDING_TIMEOUT, SCENE_TIMEOUT, TARGET_TIMEOUT},
        power::IDLE_TIMEOUT,
    },
};

/// Home Assistant publishes the state of some entities before the dashboard
/// starts.
pub fn house() -> Shell {
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
        .tiles()
        .iter()
        .map(|tile| (tile.label.clone(), tile.state, tile.text.clone()))
        .collect()
}

/// Returns the All lights toggle and the weather in the header of the index
/// page.
fn header(shell: &Shell) -> Vec<(String, TileState, String)> {
    let view = shell.view();
    let index = view.index().expect("the index page");
    [&index.all_lights, &index.weather]
        .into_iter()
        .flatten()
        .map(|tile| (tile.label.clone(), tile.state, tile.text.clone()))
        .collect()
}

/// Returns the detail of the weather in the header of the index page.
fn weather_detail(shell: &Shell) -> String {
    let view = shell.view();
    let index = view.index().expect("the index page");
    index.weather.as_ref().expect("the weather").detail.clone()
}

/// Returns the summary of each room on the index page.
fn summaries(shell: &Shell) -> Vec<RoomSummary> {
    let view = shell.view();
    view.index().expect("the index page").rooms.clone()
}

fn summary(title: &str, lights: &[LightState], temperature: Option<&str>) -> RoomSummary {
    RoomSummary {
        title: title.into(),
        lights: lights.to_vec(),
        temperature: temperature.map(String::from),
        any_on: lights.contains(&On),
    }
}

fn tile(label: &str, state: TileState, text: &str) -> (String, TileState, String) {
    (label.into(), state, text.into())
}

/// Returns the ids of the idle timers that sent `events`, in order.
fn idle_timeouts(events: &[Event]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Power(power::Event::IdleTimeout(id)) => Some(*id),
            _ => None,
        })
        .collect()
}

/// Returns the number of times that `effect` is in `log`.
fn count(log: &[String], effect: &str) -> usize {
    log.iter().filter(|entry| *entry == effect).count()
}

/// Returns the number of `Join` requests in `log`.
fn joins(log: &[String]) -> usize {
    log.iter()
        .filter(|entry| entry.starts_with("RadioJoin"))
        .count()
}

#[test]
fn on_the_stand_the_dashboard_shows_live_state_and_controls_lights() {
    let mut shell = house();

    shell.send(start(PowerSource::Stand));

    assert!(shell.radio.associated.is_some());
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
    // The retained messages arrive at once, with the `last_updated` time of
    // each entity. The core starts one draw timer for all of them.
    assert_eq!(view.updated, "Last changed 09:41");
    assert_eq!(count(&shell.take_log(), "TimeNotifyAfter 200ms"), 1);
    assert_eq!(
        header(&shell),
        [
            tile("All lights", TileState::On, "On 100%"),
            tile("Weather", TileState::Value, "14.2 °C"),
        ]
    );
    assert_eq!(weather_detail(&shell), "Cloudy");
    assert_eq!(texts(&shell), []);

    shell.send(Event::NextPage);
    assert_eq!(
        texts(&shell),
        [
            tile("Lights", TileState::Off, "Off"),
            tile("Rocket lamp", TileState::On, "On 50%"),
            tile("Standard lamp", TileState::Unavailable, "Unavailable"),
            tile("Temperature", TileState::Value, "21.4 °C"),
            tile("Humidity", TileState::Unknown, "No data"),
        ]
    );

    // A tap goes to Home Assistant, and its new state comes back.
    shell.send(tap_tile(0));
    assert_eq!(shell.broker.state("light.living_room_lights"), Some("on"));
    assert_eq!(shell.view().tiles()[0].state, TileState::On);

    // A change in Home Assistant shows at once.
    shell
        .broker
        .set_state("light.living_room_rocket_lamp", "off", &[]);
    shell.run();
    assert_eq!(shell.view().tiles()[1].text, "Off");
    assert!(!shell.frames.last().unwrap().full);

    shell.send(tap_scene(1));
    let call = shell.broker.service_calls.last().unwrap();
    assert_eq!(call.service, "scene.turn_on");
    assert_eq!(call.entity_id, "scene.living_room_living_room_dimmed");
    // A scene has no state that confirms it, so its mark stays for a short
    // time.
    assert!(shell.view().scenes()[1].pending);
    let events = shell.advance(SCENE_TIMEOUT);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Pending(pending::Event::Timeout(_)))),
        "{events:?}"
    );
    assert!(!shell.view().scenes()[1].pending);

    // On the stand nothing sleeps, and no idle timer runs.
    let events = shell.advance(IDLE_TIMEOUT);
    assert_eq!(idle_timeouts(&events), [] as [u32; 0]);
    assert_eq!(shell.sleep, None);
}

#[test]
fn on_battery_the_dashboard_wakes_syncs_accepts_taps_then_sleeps() {
    let mut shell = house();

    shell.send(start_at(PowerSource::Battery, Some("/room/dining_room")));
    assert_eq!(shell.view().tiles()[4].text, "19.5°");
    assert_eq!(shell.view().tiles()[4].detail, "Set 21°");

    // The lamp has a state, so its new state confirms the tap.
    shell
        .broker
        .set_state("light.dining_room_tall_lamp", "off", &[]);
    shell.run();
    shell.send(tap_tile(1));
    assert_eq!(
        shell.broker.state("light.dining_room_tall_lamp"),
        Some("on")
    );
    assert_eq!(shell.view().tiles()[1].text, "On");

    // The tap started the light timer and the idle timer again. The core
    // cleared the idle timer of the start, so only the new one fires.
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    let events = shell.advance(IDLE_TIMEOUT);
    let idle = events
        .iter()
        .filter(|event| matches!(event, Event::Power(power::Event::IdleTimeout(_))))
        .count();
    assert_eq!(idle, 1, "{events:?}");

    // The last image shows the time of the last update from Home Assistant.
    assert_eq!(
        shell.take_log(),
        [
            // The light timer turned the light off before the idle timeout.
            "FrontLight 0",
            // Each step of the sleep waits for the answer of the shell.
            "MqttPublish dashboard/hall/status asleep",
            "MqttDisconnect",
            // The answer to the close of the stream comes after the shell
            // sent the DISCONNECT packet.
            "StreamClose",
            // The core does not wait for the answer for longer than
            // `CLOSE_TIMEOUT`. The answer clears that timer.
            "TimeNotifyAfter 6000ms",
            "TimeClear",
            "WifiOff",
            // The time limit of the `Off` request. The answer clears it.
            "TimeNotifyAfter 3000ms",
            "TimeClear",
            // No full-refresh timer runs while the device sleeps.
            "TimeClear",
            "Render",
            "Sleep",
        ]
    );
    assert_eq!(shell.sleep.as_ref().unwrap().wake_on, [WakeSource::Touch]);
    assert!(!shell.radio.on);
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
    assert_eq!(last.updated, "Last changed 09:41");
    assert_eq!(last.path, "/room/dining_room");
}

#[test]
fn a_battery_wake_shows_the_changes_from_while_it_slept() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());

    shell.broker.time += 3600;
    shell.broker.set_state("light.all_lights", "off", &[]);
    shell.broker.set_state("weather.home", "sunny", &[]);

    shell.wake(Wake::Timer);
    assert!(shell.radio.associated.is_some());
    assert_eq!(shell.broker.connections, 2);
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(
        header(&shell),
        [
            tile("All lights", TileState::Off, "Off"),
            tile("Weather", TileState::Value, "14.2 °C"),
        ]
    );
    assert_eq!(weather_detail(&shell), "Sunny");

    assert_eq!(shell.view().updated, "Last changed 10:41");
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.view().asleep);
    assert_eq!(
        shell.frames.last().unwrap().view.updated,
        "Last changed 10:41"
    );
}

#[test]
fn a_move_to_the_stand_while_asleep_connects_and_stays_awake() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.model.power.is_asleep());

    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert!(shell.radio.associated.is_some());
    assert!(shell.broker.is_connected());
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("online")
    );
    // On the stand, the device does not sleep again. No idle timer with the
    // current id fires.
    let events = shell.advance(IDLE_TIMEOUT);
    assert!(!idle_timeouts(&events).contains(&shell.model.power.idle));
    assert!(!shell.view().asleep);
}

#[test]
fn a_move_off_the_stand_sleeps_after_the_idle_timeout() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.advance(IDLE_TIMEOUT);
    assert_eq!(shell.sleep, None);

    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert!(shell.broker.is_connected());

    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert!(!shell.radio.on);
}

#[test]
fn a_move_to_the_stand_cancels_the_idle_timeout() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    let idle = shell.model.power.idle;

    // The move clears the idle timer of the start.
    shell.take_log();
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");

    // An expiry of that timer that still arrives does nothing.
    shell.take_log();
    shell.send(Event::Power(power::Event::IdleTimeout(idle)));
    assert_eq!(shell.take_log(), Vec::<String>::new());
    assert_eq!(shell.sleep, None);

    // The timer does not fire.
    let events = shell.advance(IDLE_TIMEOUT);
    assert_eq!(idle_timeouts(&events), [] as [u32; 0]);
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
    assert_eq!(view.updated, "Last changed 09:41");

    // While the dashboard is offline, Home Assistant changes a light two
    // minutes later.
    shell.broker.time += 120;
    shell.broker.set_state("light.all_lights", "off", &[]);

    // The new connection gets the retained change and its time.
    shell.take_log();
    let events = shell.advance(RECONNECT_MIN);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_))))
    );
    assert!(shell.broker.is_connected());
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("online")
    );
    assert_eq!(header(&shell)[0], tile("All lights", TileState::Off, "Off"));
    assert_eq!(count(&shell.take_log(), "TimeNotifyAfter 200ms"), 1);
    assert_eq!(shell.view().updated, "Last changed 09:43");
}

#[test]
fn while_the_broker_is_down_the_wait_doubles_up_to_a_limit() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));

    // Only the reconnect timer, the timers of the `Close` requests, the time
    // limits of the radio requests and the full-refresh timer run. The
    // shell answered each `Close` request and each radio request, so the
    // core ignores their timers. A reconnect timer is due before the
    // full-refresh timer.
    let mut waits = vec![];
    for _ in 0..9 {
        let before = shell.now();
        loop {
            let events = shell.advance_to_next();
            let reconnects = events
                .iter()
                .filter(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_))))
                .count();
            let closes = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        Event::Connection(connection::Event::CloseTimeout { .. })
                            | Event::Wifi(wifi::Event::RadioTimeout(_))
                    )
                })
                .count();
            assert!(
                reconnects <= 1 && reconnects + closes == events.len(),
                "{events:?}"
            );
            if reconnects == 1 {
                break;
            }
        }
        waits.push(shell.now().saturating_sub(before));
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

    // The next try after the longest wait connects.
    shell.broker.down = false;
    shell.advance(RECONNECT_MAX.saturating_sub(Duration::from_millis(1)));
    assert!(!shell.broker.is_connected());
    let events = shell.advance(Duration::from_millis(1));
    assert!(
        matches!(
            events[..],
            [Event::Connection(connection::Event::Reconnect(_))]
        ),
        "{events:?}"
    );
    assert!(shell.broker.is_connected());
    assert_eq!(shell.model.connection.backoff, RECONNECT_MIN);
}

#[test]
fn taps_without_a_connection_are_sent_when_the_core_connects() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));

    shell.send(tap_all_lights());
    shell.send(Event::Navigate(Route::room("loft")));
    shell.send(tap_scene(0));
    assert_eq!(shell.broker.service_calls.len(), 0);

    // Each tap also tried to connect. Each new reconnect timer cleared the
    // timer before it, so only one reconnect timer fires. It connects and
    // sends the calls.
    shell.broker.down = false;
    let mut fired = vec![];
    while !shell.broker.is_connected() {
        let events = shell.advance_to_next();
        let [Event::Connection(connection::Event::Reconnect(id))] = events[..] else {
            panic!("one reconnect timer: {events:?}");
        };
        fired.push(id);
    }
    assert_eq!(fired.len(), 1, "{fired:?}");
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
fn a_join_that_fails_on_battery_is_tried_again_until_the_device_sleeps() {
    let mut shell = house();
    shell.radio.blocked = true;
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.view().connection, Connection::Connecting);

    // The core joins again after each wait, until the idle timeout.
    shell.take_log();
    shell.advance(IDLE_TIMEOUT);
    assert!(joins(&shell.take_log()) > 1);
    assert!(shell.sleep.is_some());
    // The core never connected, so it has no time of an update.
    assert_eq!(shell.view().updated, "");

    // The sleep cleared the reconnect timer, so no reconnect comes and the
    // device stays asleep with the radio off.
    shell.take_log();
    let events = shell.advance(RECONNECT_MAX);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_)))),
        "{events:?}"
    );
    assert!(shell.model.power.is_asleep());
    assert_eq!(joins(&shell.take_log()), 0);
    assert!(!shell.radio.on);
}

#[test]
fn a_full_refresh_is_kept_when_a_partial_redraw_follows_before_the_shell_draws() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    shell.send(tap_all_lights());
    assert!(!shell.frames.last().unwrap().full);

    // The page changes, and the connection drops before the shell draws.
    // The drop asks for a partial redraw. The shell draws once, with a full
    // refresh. (A message from the state stream does not redraw at once. It
    // starts the draw timer.)
    let frames = shell.frames.len();
    shell.queue(Event::NextPage);
    shell.broker.drop_connection();
    shell.run();
    assert_eq!(shell.frames.len(), frames + 1);
    let frame = shell.frames.last().unwrap();
    assert!(frame.full);
    assert_eq!(frame.view.connection, Connection::Connecting);
    assert_eq!(frame.view.path, "/room/living_room");
}

#[test]
fn on_the_stand_a_wifi_drop_joins_again_before_it_reconnects() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));

    // Wi-Fi drops, so the connection drops too.
    shell.radio.drop_link();
    shell.broker.drop_connection();
    shell.run();
    shell.take_log();

    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    let join = log
        .iter()
        .position(|effect| effect.starts_with("RadioJoin"));
    let connect = log.iter().position(|effect| effect == "MqttConnect");
    assert!(join.is_some() && join < connect, "{log:?}");
    assert!(shell.radio.associated.is_some());
    assert!(shell.broker.is_connected());
}

#[test]
fn on_battery_a_tap_that_waits_for_a_connection_delays_the_sleep_once() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Battery));
    shell.send(tap_all_lights());
    assert_eq!(shell.model.connection.outbox.len(), 1);

    // The first idle timeout waits one more idle period for the connection.
    shell.advance(IDLE_TIMEOUT);
    assert_eq!(shell.sleep, None);

    // The connection still fails, so the core drops the tap and sleeps.
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    let view = shell.view();
    assert_eq!(view.status, "Asleep, 1 tap not sent");
    assert_eq!(shell.broker.service_calls.len(), 0);

    // The next connection clears the notice.
    shell.broker.down = false;
    shell.wake(Wake::Timer);
    assert_eq!(shell.view().status, "Online");
}

#[test]
fn on_battery_a_tap_that_is_sent_before_the_extra_period_ends_is_not_lost() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Battery));
    shell.send(tap_all_lights());

    // The broker comes back just before the idle timeout. At the idle
    // timeout, the core starts one more idle period, and then a reconnect
    // timer fires and sends the tap.
    let second = Duration::from_secs(1);
    shell.advance(IDLE_TIMEOUT.saturating_sub(second));
    shell.broker.down = false;
    let events = shell.advance(second);
    let idle = events
        .iter()
        .rposition(|event| matches!(event, Event::Power(power::Event::IdleTimeout(_))));
    let reconnect = events
        .iter()
        .position(|event| matches!(event, Event::Connection(connection::Event::Reconnect(_))));
    assert!(idle.is_some() && idle < reconnect, "{events:?}");
    assert_eq!(shell.broker.service_calls.len(), 1);
    assert_eq!(shell.sleep, None);

    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert_eq!(shell.view().status, "Asleep");
}

#[test]
fn a_wake_after_a_long_outage_retries_after_the_shortest_wait() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));
    while shell.model.connection.backoff < RECONNECT_MAX {
        shell.advance_to_next();
    }

    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    while shell.sleep.is_none() {
        shell.advance_to_next();
    }
    shell.wake(Wake::Timer);
    // The wake tries at once, and again after the shortest wait.
    shell.take_log();
    shell.advance(RECONNECT_MIN);
    assert_eq!(count(&shell.take_log(), "MqttConnect"), 1);
}

#[test]
fn after_a_deep_sleep_the_view_shows_the_last_update_that_the_shell_kept() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    // The shell keeps the time of the last state message, not the time of
    // the sleep.
    let kept = shell.view().last_sync;
    assert_eq!(kept, Some(super::NOW));

    // A deep sleep loses the core. The shell starts a new core with the
    // values that it kept, and the broker is not there.
    let mut shell = house();
    shell.broker.down = true;
    shell.send(crate::Event::Start(crate::Start {
        last_sync: kept,
        ..start_parts(PowerSource::Battery)
    }));
    assert_eq!(shell.view().updated, "Last changed 09:41");
}

fn start_parts(power: PowerSource) -> crate::Start {
    let Event::Start(start) = start(power) else {
        unreachable!()
    };
    start
}

#[test]
fn the_footer_shows_the_time_of_the_last_update_not_a_clock() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.view().updated, "Last changed 09:41");

    // Time passes with no message: nothing changes on the screen.
    let frames = shell.frames.len();
    shell.advance(Duration::from_secs(600));
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.view().updated, "Last changed 09:41");

    // An update shows the time of Home Assistant, in one draw.
    shell.broker.set_state("light.all_lights", "off", &[]);
    shell.run();
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(shell.view().updated, "Last changed 09:51");

    // Another update in the same minute draws the tile, and the time stays.
    shell.broker.time += 30;
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    assert_eq!(shell.frames.len(), frames + 2);
    assert_eq!(shell.view().updated, "Last changed 09:51");

    // After a lost connection, the time is the time of the last update, not
    // the time of the disconnect.
    shell.broker.time += 600;
    shell.broker.drop_connection();
    shell.run();
    assert_eq!(shell.view().updated, "Last changed 09:51");
    assert_eq!(shell.view().last_sync, Some(super::NOW + 600));
}

#[test]
fn on_battery_a_tap_while_asleep_wakes_and_toggles_the_light() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.view().asleep);
    let frames = shell.frames.len();

    // The last image shows the toggle, so the tap that wakes the device also
    // acts on it.
    shell.send(tap_all_lights());
    assert!(!shell.view().asleep);
    assert!(shell.frames[frames..].iter().any(|frame| frame.full));
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(shell.broker.service_calls.len(), 1);
    assert_eq!(shell.broker.state("light.all_lights"), Some("off"));

    // The next tap acts on the toggle again.
    shell.send(tap_all_lights());
    assert_eq!(shell.broker.state("light.all_lights"), Some("on"));
}

#[test]
fn the_plus_and_minus_buttons_change_the_brightness_of_a_light() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    assert_eq!(
        texts(&shell)[1],
        tile("Rocket lamp", TileState::On, "On 50%")
    );
    assert_eq!(shell.view().tiles()[1].detail, "50%");

    // Home Assistant adds 25% (64 of 255) to the brightness, and the new
    // brightness comes back on the state stream.
    shell.send(tap_plus(1));
    let call = shell.broker.service_calls.last().unwrap();
    assert_eq!(call.service, "light.turn_on");
    assert_eq!(call.entity_id, "light.living_room_rocket_lamp");
    assert_eq!(call.brightness_step_pct, Some(25));
    assert_eq!(
        texts(&shell)[1],
        tile("Rocket lamp", TileState::On, "On 75%")
    );
    assert_eq!(shell.view().tiles()[1].detail, "75%");
    assert!(!shell.frames.last().unwrap().full);

    // Each step down takes 25% off. At 0, Home Assistant turns the light off.
    shell.send(tap_minus(1));
    shell.send(tap_minus(1));
    assert_eq!(
        texts(&shell)[1],
        tile("Rocket lamp", TileState::On, "On 25%")
    );
    shell.send(tap_minus(1));
    assert_eq!(texts(&shell)[1], tile("Rocket lamp", TileState::Off, "Off"));
    assert_eq!(shell.view().tiles()[1].detail, "");

    // A step up turns a light that is off on at 25%.
    shell.send(tap_plus(1));
    assert_eq!(
        texts(&shell)[1],
        tile("Rocket lamp", TileState::On, "On 25%")
    );
    assert_eq!(shell.broker.service_calls.len(), 5);

    // A tap on the rest of the tile still toggles the light.
    shell.send(tap_tile(1));
    let call = shell.broker.service_calls.last().unwrap();
    assert_eq!(call.service, "light.toggle");
    assert_eq!(call.brightness_step_pct, None);
    assert_eq!(texts(&shell)[1].1, TileState::Off);
}

#[test]
fn an_onoff_light_has_no_brightness_steps() {
    let mut shell = house();
    shell.broker.set_state(
        "light.dining_room_candle_light_left",
        "on",
        &[("brightness", "128")],
    );
    shell
        .broker
        .set_state("light.dining_room_tall_lamp", "off", &[]);
    shell
        .broker
        .set_state("light.dining_room_candle_light_right", "off", &[]);
    shell.send(start_at(PowerSource::Stand, Some("/room/dining_room")));
    let view = shell.view();
    let tiles = view.tiles();
    assert!(tiles[1].steps);
    assert!(!tiles[2].steps);
    assert!(!tiles[3].steps);
    // The tile of an on/off light shows no brightness under its value.
    assert_eq!(tiles[2].text, "On 50%");
    assert_eq!(tiles[2].detail, "");

    shell.send(tap_plus(2));
    shell.send(tap_minus(3));
    assert_eq!(shell.broker.service_calls.len(), 0);

    // A tap on the tile toggles the light.
    shell.send(tap_tile(2));
    assert_eq!(
        shell.broker.state("light.dining_room_candle_light_left"),
        Some("off")
    );
}

#[test]
fn a_dimmable_light_has_steps_only_while_it_is_on_or_off() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Stand, Some("/room/living_room")));
    let steps = |shell: &Shell| -> Vec<bool> {
        shell.view().tiles().iter().map(|tile| tile.steps).collect()
    };
    // The group is off, the rocket lamp is on and the standard lamp is
    // unavailable. The sensors are not lights.
    assert_eq!(steps(&shell), [true, true, false, false, false]);
    assert_eq!(
        texts(&shell)[2],
        tile("Standard lamp", TileState::Unavailable, "Unavailable")
    );

    // A step on the unavailable lamp does nothing.
    shell.send(tap_plus(2));
    shell.send(tap_minus(2));
    assert_eq!(shell.broker.service_calls.len(), 0);

    // When the lamp is back, its tile has the steps again.
    shell
        .broker
        .set_state("light.living_room_standard_lamp", "off", &[]);
    shell.run();
    assert_eq!(steps(&shell), [true, true, true, false, false]);

    // A − step on a light that is off does nothing. A + step turns it on.
    shell.send(tap_minus(2));
    assert_eq!(shell.broker.service_calls.len(), 0);
    shell.send(tap_plus(2));
    assert_eq!(shell.broker.service_calls.len(), 1);
    assert_eq!(
        texts(&shell)[2],
        tile("Standard lamp", TileState::On, "On 25%")
    );

    // When the state is unknown, the tile has no steps.
    shell
        .broker
        .set_state("light.living_room_standard_lamp", "unknown", &[]);
    shell.run();
    assert!(!steps(&shell)[2]);
}

#[test]
fn on_battery_a_brightness_step_while_asleep_wakes_and_changes_the_light() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Battery, Some("/room/living_room")));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.view().asleep);

    // The tap that wakes the device also steps the brightness. The call
    // waits for the connection, then goes out.
    shell.send(tap_plus(1));
    assert!(!shell.view().asleep);
    assert_eq!(shell.broker.service_calls.len(), 1);
    assert_eq!(
        texts(&shell)[1],
        tile("Rocket lamp", TileState::On, "On 75%")
    );

    shell.send(tap_plus(1));
    assert_eq!(
        texts(&shell)[1],
        tile("Rocket lamp", TileState::On, "On 100%")
    );
}

#[test]
fn a_dry_run_sends_no_service_call_until_it_stops() {
    let mut shell = house();
    let Event::Start(mut start) = start(PowerSource::Stand) else {
        unreachable!()
    };
    start.settings.read_only = true;
    shell.send(Event::Start(start));
    assert!(shell.view().read_only);

    shell.take_log();
    shell.send(tap_all_lights());
    shell.send(Event::Navigate(Route::room("living_room")));
    shell.send(tap_scene(0));
    shell.send(tap_plus(1));
    shell.send(tap_minus(1));
    assert_eq!(shell.broker.service_calls.len(), 0);
    assert_eq!(shell.model.connection.outbox.len(), 0);
    // The status message is not a service call, so the core still sends it.
    assert_eq!(
        shell.broker.retained("dashboard/hall/status"),
        Some("online")
    );
    assert!(
        !shell
            .take_log()
            .iter()
            .any(|effect| effect.starts_with("MqttPublish")),
    );

    shell.send(Event::SetReadOnly(false));
    assert!(!shell.view().read_only);
    shell.send(Event::Navigate(Route::Home));
    shell.send(tap_all_lights());
    assert_eq!(shell.broker.state("light.all_lights"), Some("off"));
}

#[test]
fn the_index_shows_a_summary_of_each_room_and_a_tap_shows_the_room() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    let view = shell.view();
    assert_eq!(view.title, "Home");
    assert_eq!(view.path, "/");

    // The Living Room has two lamps and a group of them. The group is not
    // in the summary. The standard lamp is unavailable. The other rooms
    // have no state yet.
    let rooms = summaries(&shell);
    assert_eq!(rooms.len(), 9);
    assert_eq!(
        rooms[..2],
        [
            summary("Living room", &[On, Unavailable], Some("21.4°")),
            summary("Dining room", &[Unavailable; 3], Some("19.5°")),
        ]
    );
    assert!(rooms[0].any_on);
    assert_eq!(rooms[8], summary("Outside", &[Unavailable; 2], None));

    // A change in Home Assistant changes the summary with a partial refresh.
    shell.broker.set_state("light.porch_light", "on", &[]);
    shell
        .broker
        .set_state("light.back_garden_garden_light", "off", &[]);
    shell.run();
    assert_eq!(summaries(&shell)[8], summary("Outside", &[On, Off], None));
    assert!(!shell.frames.last().unwrap().full);

    // A tap on a room shows the room with a full refresh.
    shell.send(tap_room(8));
    let frame = shell.frames.last().unwrap();
    assert!(frame.full);
    assert_eq!(frame.view.title, "Outside");
    assert_eq!(frame.view.path, "/room/outside");
    assert_eq!(texts(&shell)[0], tile("Porch", TileState::On, "On"));

    // On a room page, the core ignores a room button and the All lights
    // toggle. Only the index page has them.
    let frames = shell.frames.len();
    shell.send(tap_room(0));
    shell.send(tap_all_lights());
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.view().path, "/room/outside");
    assert_eq!(shell.broker.service_calls.len(), 0);

    shell.send(Event::Navigate(Route::Home));
    assert!(shell.frames.last().unwrap().full);
    assert!(shell.view().index().is_some());
}

#[test]
fn the_all_lights_toggle_calls_home_assistant() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));

    shell.send(tap_all_lights());
    let call = shell.broker.service_calls.last().unwrap();
    assert_eq!(call.service, "light.toggle");
    assert_eq!(call.entity_id, "light.all_lights");
    assert_eq!(header(&shell)[0], tile("All lights", TileState::Off, "Off"));
    assert!(!shell.frames.last().unwrap().full);

    // The index has no tiles, so the core ignores a tile on the home page.
    shell.send(tap_tile(0));
    shell.send(tap_scene(0));
    assert_eq!(shell.broker.service_calls.len(), 1);
}

#[test]
fn a_touch_wake_on_a_light_sends_the_toggle_after_the_reconnect() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.take_log();

    // The tap that wakes the device is on the All lights toggle. The shell
    // answers `Sleep` with the control. The core keeps the toggle until the
    // connection is open, then sends it.
    shell.send(tap_all_lights());
    let log = shell.take_log();
    assert_eq!(log[0], "Wake Touch(Some(AllLights))");
    assert_eq!(joins(&log), 1, "{log:?}");
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.view().connection, Connection::Online);
    assert_eq!(shell.broker.service_calls.len(), 1);
    assert_eq!(shell.sleep, None);
}

#[test]
fn the_stand_while_asleep_wakes_and_the_shell_answers_sleep() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.take_log();

    // The shell sends the change, then answers `Sleep` with `PowerSource`.
    // The core woke at the change, so it ignores the answer.
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    let log = shell.take_log();
    assert_eq!(log.last().map(String::as_str), Some("Wake PowerSource"));
    assert_eq!(joins(&log), 1, "{log:?}");
    assert_eq!(shell.sleep, None);
    assert!(!shell.model.power.is_asleep());
    assert_eq!(shell.view().connection, Connection::Online);
    // On the stand, the device does not sleep again. No idle timer with the
    // current id fires.
    let events = shell.advance(IDLE_TIMEOUT);
    assert!(!idle_timeouts(&events).contains(&shell.model.power.idle));
    assert_eq!(shell.sleep, None);
}

#[test]
fn the_plus_and_minus_buttons_change_the_target_temperature_of_a_thermostat() {
    let mut shell = house();
    shell.send(start_at(PowerSource::Stand, Some("/room/dining_room")));
    let thermostat = |shell: &Shell| shell.view().tiles()[4].clone();
    let tile = thermostat(&shell);
    assert!(tile.steps);
    assert_eq!(tile.text, "19.5°");
    assert_eq!(tile.detail, "Set 21°");

    // Home Assistant sets the new target, and it comes back on the state
    // stream. The new target confirms the step, so the mark goes.
    shell.send(tap_plus(4));
    let call = shell.broker.service_calls.last().unwrap();
    assert_eq!(call.service, "climate.set_temperature");
    assert_eq!(call.entity_id, "climate.dining_room_thermostat");
    assert_eq!(call.temperature, Some(21.5));
    let tile = thermostat(&shell);
    assert_eq!(tile.state, TileState::Value);
    assert_eq!(tile.detail, "Set 21.5°");
    assert!(shell.model.pending.marks.is_empty());

    shell.send(tap_minus(4));
    shell.send(tap_minus(4));
    assert_eq!(thermostat(&shell).detail, "Set 20.5°");
    assert_eq!(shell.broker.service_calls.len(), 3);

    // A tap on the rest of the tile does nothing.
    shell.send(tap_tile(4));
    assert_eq!(shell.broker.service_calls.len(), 3);
}

#[test]
fn a_thermostat_step_past_its_limit_shows_the_mark_until_the_timeout() {
    let mut shell = house();
    shell.broker.set_state(
        "climate.dining_room_thermostat",
        "heat",
        &[("current_temperature", "19.5"), ("temperature", "32.0")],
    );
    shell.send(start_at(PowerSource::Stand, Some("/room/dining_room")));

    // The core does not know the limit, so it sends the call. Home Assistant
    // keeps the target at 32 °C and publishes nothing.
    shell.send(tap_plus(4));
    assert_eq!(
        shell.broker.service_calls.last().unwrap().temperature,
        Some(32.5)
    );
    assert_eq!(shell.view().tiles()[4].state, TileState::Pending);

    // A thermostat can be slow, so the mark stays after the timeout of a
    // light.
    let events = shell.advance(PENDING_TIMEOUT);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Pending(pending::Event::Timeout(_)))),
        "{events:?}"
    );
    assert_eq!(shell.view().tiles()[4].state, TileState::Pending);

    let events = shell.advance(TARGET_TIMEOUT.saturating_sub(PENDING_TIMEOUT));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Pending(pending::Event::Timeout(_)))),
        "{events:?}"
    );
    let tile = shell.view().tiles()[4].clone();
    assert_eq!(tile.state, TileState::Value);
    assert_eq!(tile.detail, "Set 32.0°");

    // A step down goes from the target of Home Assistant.
    shell.send(tap_minus(4));
    assert_eq!(
        shell.broker.service_calls.last().unwrap().temperature,
        Some(31.5)
    );
    assert_eq!(shell.view().tiles()[4].detail, "Set 31.5°");
}

#[test]
fn a_move_to_the_stand_sets_the_backoff_to_the_minimum() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(start(PowerSource::Stand));
    while shell.model.connection.backoff < RECONNECT_MAX {
        shell.advance_to_next();
    }

    // A move to battery does not change the backoff.
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert_eq!(shell.model.connection.backoff, RECONNECT_MAX);

    // A move back to the stand sets the backoff to the minimum. The core
    // tries at once, and again after the shortest wait.
    shell.take_log();
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    shell.advance(RECONNECT_MIN);
    let log = shell.take_log();
    assert_eq!(count(&log, "MqttConnect"), 2, "{log:?}");
}

#[test]
fn a_second_start_without_a_last_sync_keeps_the_footer_time() {
    let mut shell = house();
    shell.broker.down = true;
    shell.send(Event::Start(crate::Start {
        last_sync: Some(super::NOW),
        ..start_parts(PowerSource::Stand)
    }));
    assert_eq!(shell.view().updated, "Last changed 09:41");

    // The shell has no time of a state message to give.
    shell.send(Event::Start(crate::Start {
        last_sync: None,
        ..start_parts(PowerSource::Stand)
    }));
    assert_eq!(shell.view().last_sync, Some(super::NOW));
    assert_eq!(shell.view().updated, "Last changed 09:41");
}

#[test]
fn before_start_the_core_draws_pages_but_sends_no_calls_and_does_not_sleep() {
    let mut shell = house();

    // A move to battery draws and starts an idle timer.
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    let log = shell.take_log();
    assert!(log.contains(&"Render".into()), "{log:?}");
    let idle = alloc::format!("TimeNotifyAfter {}ms", IDLE_TIMEOUT.as_millis());
    assert_eq!(count(&log, &idle), 1, "{log:?}");

    // The idle timer expires, but the device does not sleep.
    let events = shell.advance(IDLE_TIMEOUT);
    assert_eq!(idle_timeouts(&events).len(), 1);
    assert_eq!(shell.take_log(), [] as [String; 0]);
    assert!(!super::asleep(&shell.model));
    assert_eq!(shell.sleep, None);

    // A page turn changes the route and draws.
    let frames = shell.frames.len();
    let route = shell.model.route.clone();
    shell.send(Event::NextPage);
    assert_ne!(shell.model.route, route);
    assert_eq!(shell.frames.len(), frames + 1);

    // A tap on a tile sends nothing and keeps nothing for later.
    shell.send(Event::Navigate(Route::room("living_room")));
    shell.take_log();
    shell.send(tap_tile(0));
    let log = shell.take_log();
    assert!(
        !log.iter().any(|entry| entry.starts_with("Mqtt")),
        "{log:?}"
    );
    assert_eq!(shell.model.connection.outbox.len(), 0);
    assert_eq!(shell.broker.service_calls.len(), 0);
    assert!(!shell.broker.is_connected());
}
