use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use crux_time::Instant;
use embedded_graphics::prelude::Point;
use shared::{
    Connection, Event, PowerSource,
    capabilities::{
        mqtt::{Message, MqttEvent},
        network::WifiStatus,
    },
    features::power::IDLE_TIMEOUT,
};

use super::{Job, MqttOp, Shell, Signal, TimerOp, start};
use crate::config::Config;

const CONFIG: Config<'static> = Config {
    wifi_ssid: "network",
    wifi_password: "wifi-secret",
    wifi_auth: crate::config::WifiAuth::Wpa2,
    mqtt_host: "broker",
    mqtt_port: 1883,
    mqtt_username: "dashboard",
    mqtt_password: "mqtt-secret",
    device_id: "test",
    read_only: true,
};

/// 2026-11-07 12:00:00 UTC.
fn clock() -> Instant {
    Instant::new(1_793_966_400, 0)
}

/// A short name for each job, to compare lists of jobs.
fn names(jobs: &[Job]) -> Vec<String> {
    jobs.iter()
        .map(|job| match job {
            Job::Render(_) => "render".into(),
            Job::Mqtt { connection, op } => match op {
                MqttOp::Connect(_) => alloc::format!("connect {connection}"),
                MqttOp::Subscribe(_) => alloc::format!("subscribe {connection}"),
                MqttOp::Publish(publish) => {
                    alloc::format!("publish {} {connection}", publish.topic)
                }
                MqttOp::Disconnect(_) => alloc::format!("disconnect {connection}"),
            },
            Job::WifiOn { token, .. } => alloc::format!("wifi on {token}"),
            Job::WifiOff(_) => "wifi off".into(),
            Job::Sleep(_) => "sleep".into(),
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(_),
            } => alloc::format!("start timer {token}"),
            Job::Timer {
                token,
                op: TimerOp::ClearTimer(_),
            } => alloc::format!("cancel timer {token}"),
        })
        .collect()
}

fn started(power: PowerSource, read_only: bool) -> (Shell, Vec<Job>) {
    let mut shell = Shell::new(clock);
    let config = Config {
        read_only,
        ..CONFIG
    };
    let start = start(&config, power, Some("/room/living_room".into()), None);
    let jobs = shell.send(Event::Start(start));
    (shell, jobs)
}

/// Starts on the stand and connects. Returns the shell, with connection 1
/// online.
fn online(read_only: bool) -> Shell {
    let (mut shell, _) = started(PowerSource::Stand, read_only);
    shell.handle(joined(1));
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Connected,
    });
    shell
}

/// The answer of the Wi-Fi task to the request with `token`.
const fn joined(token: u32) -> Signal {
    Signal::Wifi {
        token,
        status: WifiStatus::Connected,
    }
}

/// Returns the id and the token of the idle timer that `jobs` start.
fn timer_of(jobs: &[Job]) -> Option<(crux_time::TimerId, u32)> {
    jobs.iter().find_map(|job| match job {
        Job::Timer {
            token,
            op: TimerOp::NotifyAfter(notify),
        } if Duration::from(notify.duration) == IDLE_TIMEOUT => Some((notify.id, *token)),
        _ => None,
    })
}

#[test]
fn start_draws_and_turns_wifi_on() {
    let (_, jobs) = started(PowerSource::Stand, true);
    // Timer 1 is the full-refresh timer of the core.
    assert_eq!(names(&jobs), ["render", "start timer 1", "wifi on 1"]);
}

#[test]
fn wifi_then_mqtt_connect_subscribe_and_publish_the_status() {
    let (mut shell, _) = started(PowerSource::Stand, true);
    let jobs = shell.handle(joined(1));
    assert_eq!(names(&jobs), ["connect 1"]);
    let Job::Mqtt {
        op: MqttOp::Connect(connect),
        ..
    } = &jobs[0]
    else {
        panic!("not a connect");
    };
    assert_eq!(connect.host, "broker");
    assert_eq!(connect.client_id, "dashboard-test");

    let jobs = shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Connected,
    });
    assert_eq!(
        names(&jobs),
        ["subscribe 1", "publish dashboard/test/status 1", "render"]
    );
    assert_eq!(shell.view().connection, Connection::Online);
}

#[test]
fn a_second_wifi_answer_is_ignored() {
    let (mut shell, _) = started(PowerSource::Stand, true);
    shell.handle(joined(1));
    assert_eq!(shell.handle(joined(1)), Vec::new());
}

#[test]
fn a_message_on_the_stream_redraws_a_visible_tile() {
    let mut shell = online(true);
    let tile = shell.view().tiles[0].entity_id.clone();
    let (domain, object) = tile.split_once('.').unwrap();
    let topic = alloc::format!("homeassistant/statestream/{domain}/{object}/state");
    let message = |payload: &[u8]| Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Message(Message {
            topic: topic.clone(),
            payload: payload.to_vec(),
        }),
    };
    // The first message also reads the clock, and the core starts the
    // minute timer.
    let jobs = shell.handle(message(b"on"));
    assert_eq!(names(&jobs), ["render", "start timer 2"]);
    assert_eq!(shell.view().updated, "Live · 12:00");

    // Within the minute, the core does not read the clock again.
    let jobs = shell.handle(message(b"off"));
    assert_eq!(names(&jobs), ["render"]);
}

#[test]
fn the_read_only_setting_of_the_config_makes_a_dry_run_in_the_core() {
    for (read_only, expected) in [
        (true, vec![]),
        (false, vec![String::from("publish dashboard/test/cmd 1")]),
    ] {
        let mut shell = online(read_only);
        let jobs = shell.handle(Signal::Tap(ui::layout::tile(0).unwrap().center()));
        let publishes: Vec<String> = names(&jobs)
            .into_iter()
            .filter(|name| name.starts_with("publish"))
            .collect();
        assert_eq!(publishes, expected, "read_only = {read_only}");
    }
}

#[test]
fn a_tap_on_dead_space_while_awake_does_nothing() {
    // The shell sends the tap, and the core ignores it, also on battery.
    let mut shell = online(true);
    assert_eq!(shell.handle(Signal::Tap(Point::new(100, 20))), Vec::new());
    let (mut shell, _) = started(PowerSource::Battery, true);
    assert_eq!(shell.handle(Signal::Tap(Point::new(100, 20))), Vec::new());
}

#[test]
fn a_tap_on_the_next_button_turns_the_page() {
    let mut shell = online(true);
    let before = shell.view().page_index;
    let jobs = shell.handle(Signal::Tap(ui::layout::next().center()));
    assert!(names(&jobs).contains(&"render".into()));
    assert_eq!(shell.view().page_index, before + 1);
}

#[test]
fn the_items_of_an_ended_stream_are_ignored() {
    let mut shell = online(true);
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Disconnected { reason: None },
    });
    let late = Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Connected,
    };
    assert_eq!(shell.handle(late), Vec::new());
}

#[test]
fn on_battery_the_idle_timer_sleeps_and_a_tap_wakes() {
    let (mut shell, jobs) = started(PowerSource::Battery, true);
    let (id, token) = timer_of(&jobs).unwrap();
    shell.handle(joined(1));
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Connected,
    });

    let jobs = shell.handle(Signal::Timer { id, token });
    assert_eq!(
        names(&jobs),
        [
            // The core clears the full-refresh timer before it sleeps.
            "cancel timer 1",
            "publish dashboard/test/status 1",
            "disconnect 1",
            "wifi off",
            "render",
            "sleep"
        ]
    );
    assert!(shell.view().asleep);
    assert_eq!(shell.timer_count(), 0);

    // Any tap wakes the device. The old stream has not ended yet.
    let jobs = shell.handle(Signal::Tap(Point::new(1, 1)));
    let names = names(&jobs);
    assert!(names.contains(&"wifi on 2".into()), "{names:?}");
    assert!(!shell.view().asleep);
    shell.handle(joined(2));

    // The old stream ends after the new connection opened. The new
    // connection stays current.
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Disconnected { reason: None },
    });
    let jobs = shell.handle(Signal::Mqtt {
        connection: 2,
        event: MqttEvent::Connected,
    });
    assert!(self::names(&jobs).contains(&"subscribe 2".into()));
}

#[test]
fn a_timer_signal_with_an_old_token_is_ignored() {
    let (mut shell, jobs) = started(PowerSource::Battery, true);
    let (id, token) = timer_of(&jobs).unwrap();
    let stale = Signal::Timer {
        id,
        token: token.wrapping_sub(1),
    };
    assert_eq!(shell.handle(stale), Vec::new());
    // The idle timer and the full-refresh timer.
    assert_eq!(shell.timer_count(), 2);
}

#[test]
fn a_new_timer_with_the_same_id_cancels_the_old_task() {
    // A tap on battery restarts the idle timer. The core uses a new timer
    // id, so the old timer keeps its task until the core clears it.
    let (mut shell, jobs) = started(PowerSource::Battery, true);
    let (_, first) = timer_of(&jobs).unwrap();
    let jobs = shell.handle(Signal::Tap(ui::layout::next().center()));
    let (_, second) = timer_of(&jobs).unwrap();
    assert_ne!(first, second);
    assert!(shell.timer_count() >= 1);
}

#[test]
fn start_has_the_settings_of_the_config() {
    let start = start(&CONFIG, PowerSource::Stand, None, Some(5));
    assert_eq!(start.settings.host, "broker");
    assert_eq!(start.settings.port, 1883);
    assert_eq!(start.settings.device_id, "test");
    let credentials = start.settings.credentials.unwrap();
    assert_eq!(credentials.username, "dashboard");
    assert_eq!(start.last_sync, Some(5));
    assert!(start.settings.read_only);

    let anonymous = Config {
        mqtt_username: "",
        ..CONFIG
    };
    let start = super::start(&anonymous, PowerSource::Stand, None, None);
    assert_eq!(start.settings.credentials, None);
}

#[test]
fn a_late_wifi_answer_after_wifi_off_is_dropped() {
    let (mut shell, jobs) = started(PowerSource::Battery, true);
    // The idle timer expires before Wi-Fi joins: the core turns Wi-Fi off.
    let (id, token) = timer_of(&jobs).unwrap();
    let jobs = shell.handle(Signal::Timer { id, token });
    assert!(
        names(&jobs).contains(&"wifi off".into()),
        "{:?}",
        names(&jobs)
    );
    // The join of request 1 ends after `WifiOff`. The shell drops the answer.
    assert_eq!(shell.handle(joined(1)), Vec::new());
    assert!(shell.view().asleep);
}

#[test]
fn an_answer_is_also_for_the_older_requests() {
    let (mut shell, _) = started(PowerSource::Stand, true);
    // The answer with a newer token than any request answers request 1.
    let jobs = shell.handle(joined(5));
    assert_eq!(names(&jobs), ["connect 1"]);
    // An answer with an older token than all requests is dropped.
    assert_eq!(shell.handle(joined(0)), Vec::new());
}

#[test]
fn a_batch_has_at_most_one_render_with_the_last_view() {
    let mut shell = online(true);
    // A page turn and the sleep both render. Each batch has one render.
    let jobs = shell.handle(Signal::Tap(ui::layout::next().center()));
    let renders: Vec<_> = jobs
        .iter()
        .filter_map(|job| match job {
            Job::Render(view) => Some(view),
            _ => None,
        })
        .collect();
    assert_eq!(renders.len(), 1);
    assert_eq!(*renders[0], shell.view());
}

#[test]
fn the_render_before_sleep_stays_before_sleep() {
    let (mut shell, jobs) = started(PowerSource::Battery, true);
    let (id, token) = timer_of(&jobs).unwrap();
    let jobs = shell.handle(Signal::Timer { id, token });
    let names = names(&jobs);
    let render = names.iter().position(|name| name == "render").unwrap();
    let sleep = names.iter().position(|name| name == "sleep").unwrap();
    assert!(render < sleep, "{names:?}");
    assert_eq!(names.iter().filter(|name| *name == "render").count(), 1);
    let Job::Render(view) = &jobs[render] else {
        unreachable!()
    };
    assert!(view.asleep);
}
