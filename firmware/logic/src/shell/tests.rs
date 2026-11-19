use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use embedded_graphics::prelude::Point;
use shared::{
    Connection, Event, PowerSource,
    capabilities::{
        mqtt::{Message, MqttEvent, SubscribeOutcome},
        network::WifiStatus,
        power::Wake,
    },
    features::{
        light::{BATTERY_LEVEL, LIGHT_TIMEOUT, STAND_LEVEL},
        power::IDLE_TIMEOUT,
        refresh::DRAW_DELAY,
    },
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
    force_battery: false,
};

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
            Job::WifiQuiet(_) => "wifi quiet".into(),
            Job::Sleep(_) => "sleep".into(),
            Job::FrontLight { token, level } => alloc::format!("light {level} {token}"),
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
    let mut shell = Shell::new();
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

/// Returns the id, the token and the duration of each timer that `jobs`
/// start, in order.
fn started_timers(jobs: &[Job]) -> Vec<(crux_time::TimerId, u32, Duration)> {
    jobs.iter()
        .filter_map(|job| match job {
            Job::Timer {
                token,
                op: TimerOp::NotifyAfter(notify),
            } => Some((notify.id, *token, Duration::from(notify.duration))),
            _ => None,
        })
        .collect()
}

/// The signal of the user intent that ends the idle time now. The tests of
/// the sleep use it, so they do not need the idle timer.
const fn sleep_now() -> Signal {
    Signal::Event(Event::SleepNow)
}

#[test]
fn start_draws_and_turns_wifi_on() {
    let (_, jobs) = started(PowerSource::Stand, true);
    // The core turns the light off, because its state after a reset is not
    // known. Timer 1 is the full-refresh timer of the core.
    assert_eq!(
        names(&jobs),
        ["light 0 1", "render", "start timer 1", "wifi on 1"]
    );
}

#[test]
fn wifi_then_mqtt_connect_publish_the_status_and_subscribe() {
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
        ["publish dashboard/test/status 1", "subscribe 1", "render"]
    );
    assert_eq!(shell.view().connection, Connection::Online);
}

/// Ends the stream of connection `connection` with a lost link, fires the
/// reconnect timer, and answers the new `WifiOn` with `wifi_token`. Returns
/// the jobs after the broker accepts the new connection `next`.
fn reconnect(shell: &mut Shell, connection: u32, wifi_token: u32, next: u32) -> Vec<Job> {
    let jobs = shell.handle(Signal::Mqtt {
        connection,
        event: MqttEvent::Disconnected {
            reason: Some("connection lost".into()),
        },
    });
    // The only timer that the end of the stream starts is the reconnect
    // timer.
    let [(id, token, _)] = started_timers(&jobs)[..] else {
        panic!("not one timer: {:?}", names(&jobs));
    };
    shell.handle(Signal::Timer { id, token });
    let jobs = shell.handle(joined(wifi_token));
    assert_eq!(names(&jobs), [alloc::format!("connect {next}")]);
    shell.handle(Signal::Mqtt {
        connection: next,
        event: MqttEvent::Connected,
    })
}

#[test]
fn each_connection_subscribes_again() {
    let mut shell = online(true);
    shell.handle(Signal::Subscribed {
        connection: 1,
        outcome: SubscribeOutcome::Done,
    });

    let jobs = reconnect(&mut shell, 1, 2, 2);
    assert_eq!(
        names(&jobs),
        ["publish dashboard/test/status 2", "subscribe 2", "render"]
    );
}

#[test]
fn the_subscribe_answer_makes_the_link_quiet() {
    let mut shell = online(true);
    let jobs = shell.handle(Signal::Subscribed {
        connection: 1,
        outcome: SubscribeOutcome::Done,
    });
    assert_eq!(names(&jobs), ["wifi quiet"]);
}

#[test]
fn the_end_of_the_stream_fails_the_subscribe_that_waits() {
    let mut shell = online(true);

    // The stream ends before the answer, so the shell answers `Failed`, and
    // the link is not quiet. The new connection subscribes again.
    let jobs = reconnect(&mut shell, 1, 2, 2);
    assert!(!names(&jobs).contains(&"wifi quiet".into()));
    assert_eq!(
        names(&jobs),
        ["publish dashboard/test/status 2", "subscribe 2", "render"]
    );
    // A late answer for the old connection has no request.
    let late = Signal::Subscribed {
        connection: 1,
        outcome: SubscribeOutcome::Done,
    };
    assert_eq!(shell.handle(late), Vec::new());
}

#[test]
fn a_refused_subscribe_does_not_make_the_link_quiet() {
    let mut shell = online(true);
    let jobs = shell.handle(Signal::Subscribed {
        connection: 1,
        outcome: SubscribeOutcome::Failed,
    });
    assert_eq!(names(&jobs), [] as [String; 0]);
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
    let tile = shell.view().tiles()[0].entity_id.clone();
    let (domain, object) = tile.split_once('.').unwrap();
    let message = |key: &str, payload: &[u8]| Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Message(Message {
            topic: alloc::format!("homeassistant/statestream/{domain}/{object}/{key}"),
            payload: payload.to_vec(),
        }),
    };
    // The state starts the draw timer of the core, and does not draw yet.
    let jobs = shell.handle(message("state", b"on"));
    assert_eq!(names(&jobs), ["start timer 2"]);
    let Job::Timer {
        token,
        op: TimerOp::NotifyAfter(notify),
    } = &jobs[0]
    else {
        panic!("not a timer");
    };
    assert_eq!(Duration::from(notify.duration), DRAW_DELAY);
    let (id, token) = (notify.id, *token);

    // The time of the update from Home Assistant waits for the same timer.
    // 12:00 UTC is 12:00 in the UK in November.
    let jobs = shell.handle(message("last_updated", b"2026-11-06T12:00:07.250000+00:00"));
    assert_eq!(names(&jobs), Vec::<String>::new());

    // The timer draws both once.
    let jobs = shell.handle(Signal::Timer { id, token });
    assert_eq!(names(&jobs), ["render"]);
    assert_eq!(shell.view().updated, "Last changed 12:00");
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
fn a_tap_on_dead_space_while_awake_only_turns_the_light_on() {
    // The shell sends the tap. The core turns the light on and starts the
    // light timer, but it does not draw, also on battery.
    // On battery, the idle timer has token 2, so the light timer has 3.
    for (power, level, timer) in [
        (PowerSource::Stand, STAND_LEVEL, 2),
        (PowerSource::Battery, BATTERY_LEVEL, 3),
    ] {
        let (mut shell, _) = started(power, true);
        let jobs = shell.handle(Signal::Tap(Point::new(100, 20)));
        assert_eq!(
            names(&jobs),
            [
                alloc::format!("light {level} 2"),
                alloc::format!("start timer {timer}"),
            ],
            "{power:?}"
        );
        let timers = started_timers(&jobs);
        assert_eq!(timers.len(), 1, "{power:?}");
        assert_eq!(timers[0].2, LIGHT_TIMEOUT, "{power:?}");
    }
}

#[test]
fn a_light_answer_is_also_for_the_older_requests() {
    let (mut shell, _) = started(PowerSource::Battery, true);
    shell.handle(Signal::Tap(Point::new(100, 20)));
    let jobs = shell.handle(sleep_now());
    assert_eq!(names(&jobs), ["light 0 3"]);

    // No answer came for jobs 1 to 3. An answer with a newer token answers
    // all of them, so the sleep continues.
    let jobs = shell.handle(Signal::Light { token: 5 });
    assert_eq!(names(&jobs), ["wifi off"]);
    // Each request ended, so a second answer does nothing.
    assert_eq!(shell.handle(Signal::Light { token: 5 }), Vec::new());
}

#[test]
fn on_battery_the_sleep_waits_for_the_light_before_it_disconnects() {
    let (mut shell, _) = started(PowerSource::Battery, true);
    shell.handle(Signal::Light { token: 1 });
    shell.handle(joined(1));
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Connected,
    });
    let jobs = shell.handle(Signal::Tap(Point::new(100, 20)));
    assert!(names(&jobs).contains(&"light 25 2".into()));
    shell.handle(Signal::Light { token: 2 });

    // The first step of the sleep turns the light off, and the core waits.
    let jobs = shell.handle(sleep_now());
    assert_eq!(names(&jobs), ["light 0 3"]);
    assert!(shell.view().asleep);

    // An answer for an older job does not answer job 3.
    assert_eq!(shell.handle(Signal::Light { token: 2 }), Vec::new());

    // The light is off. The next step disconnects.
    let jobs = shell.handle(Signal::Light { token: 3 });
    assert_eq!(
        names(&jobs),
        ["publish dashboard/test/status 1", "disconnect 1"]
    );
}

#[test]
fn a_tap_while_the_light_goes_off_for_the_sleep_wakes_and_the_late_answer_is_ignored() {
    let (mut shell, _) = started(PowerSource::Battery, true);
    shell.handle(Signal::Tap(Point::new(100, 20)));
    let jobs = shell.handle(sleep_now());
    assert_eq!(names(&jobs), ["light 0 3"]);

    // The tap wakes the device and turns the light on again.
    let jobs = shell.handle(Signal::Tap(Point::new(100, 20)));
    assert!(
        names(&jobs).contains(&"light 25 4".into()),
        "{:?}",
        names(&jobs)
    );
    assert!(!shell.view().asleep);

    // The answer of the sleep step comes late. The core does not disconnect.
    let jobs = shell.handle(Signal::Light { token: 3 });
    assert_eq!(jobs, Vec::new());
    assert!(!shell.view().asleep);
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
    let mut shell = online(true);
    // Off the stand, the core starts the idle timer, and no other timer.
    let jobs = shell.handle(Signal::Event(Event::PowerSourceChanged(
        PowerSource::Battery,
    )));
    let timers = started_timers(&jobs);
    assert_eq!(timers.len(), 1, "{:?}", names(&jobs));
    let (id, token, duration) = timers[0];
    assert_eq!(duration, IDLE_TIMEOUT);

    let jobs = shell.handle(Signal::Timer { id, token });
    // The first step of the sleep. The core waits for the disconnect.
    assert_eq!(
        names(&jobs),
        ["publish dashboard/test/status 1", "disconnect 1"]
    );
    assert!(shell.view().asleep);

    // Any tap during the steps wakes the device. The old stream has not
    // ended yet.
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
    let (id, token, _) = started_timers(&jobs)[0];
    let stale = Signal::Timer {
        id,
        token: token.wrapping_sub(1),
    };
    assert_eq!(shell.handle(stale), Vec::new());
    // The idle timer and the full-refresh timer.
    assert_eq!(shell.timer_count(), 2);
}

#[test]
fn a_timer_that_the_core_clears_cancels_its_task_and_the_count_does_not_grow() {
    // A page turn draws with a full refresh. The core clears the
    // full-refresh timer and starts a new one.
    let mut shell = online(true);
    let (_, first, _) = started_timers(&shell.send(Event::NextPage))[0];
    let count = shell.timer_count();

    let jobs = shell.send(Event::NextPage);
    let names = names(&jobs);
    let cancel = names
        .iter()
        .position(|name| *name == alloc::format!("cancel timer {first}"));
    let timers = started_timers(&jobs);
    assert_eq!(timers.len(), 1, "{names:?}");
    let (_, second, _) = timers[0];
    let start = names
        .iter()
        .position(|name| *name == alloc::format!("start timer {second}"));
    // The old task stops before the new one starts.
    assert!(cancel.is_some() && cancel < start, "{names:?}");
    assert_ne!(first, second);
    assert_eq!(shell.timer_count(), count);
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
    let (mut shell, _) = started(PowerSource::Battery, true);
    // The idle time ends before Wi-Fi joins: the core turns Wi-Fi off.
    let jobs = shell.handle(sleep_now());
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
    assert_eq!(**renders[0], shell.view());
}

#[test]
fn the_render_before_sleep_stays_before_sleep() {
    let (mut shell, _) = started(PowerSource::Battery, true);
    // No connection is open, so the disconnect ends at once.
    let jobs = shell.handle(sleep_now());
    assert_eq!(names(&jobs), ["wifi off"]);
    let jobs = shell.handle(Signal::RadioOff);
    let names = names(&jobs);
    // The core clears the full-refresh timer before it sleeps.
    assert!(names.contains(&"cancel timer 1".into()), "{names:?}");
    // Only the idle timer of the start runs. It is stale, because
    // `SleepNow` ended the idle time.
    assert_eq!(shell.timer_count(), 1);
    let render = names.iter().position(|name| name == "render").unwrap();
    let sleep = names.iter().position(|name| name == "sleep").unwrap();
    assert!(render < sleep, "{names:?}");
    assert_eq!(names.iter().filter(|name| *name == "render").count(), 1);
    let Job::Render(view) = &jobs[render] else {
        unreachable!()
    };
    assert!(view.asleep);
}

/// Starts on battery, connects, and ends the idle time with `SleepNow`.
/// Returns the shell and the jobs of the end of the idle time.
fn idle_timeout() -> (Shell, Vec<Job>) {
    idle_timeout_with(true)
}

/// As [`idle_timeout`], with the dry run on or off.
fn idle_timeout_with(read_only: bool) -> (Shell, Vec<Job>) {
    let (mut shell, _) = started(PowerSource::Battery, read_only);
    shell.handle(joined(1));
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Connected,
    });
    let jobs = shell.handle(sleep_now());
    (shell, jobs)
}

/// Ends the idle time and answers each step, until the `Sleep` job.
fn asleep() -> Shell {
    asleep_with(true)
}

/// As [`asleep`], with the dry run on or off.
fn asleep_with(read_only: bool) -> Shell {
    let (mut shell, _) = idle_timeout_with(read_only);
    shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Disconnected { reason: None },
    });
    shell.handle(Signal::RadioOff);
    assert!(shell.is_sleeping());
    shell
}

#[test]
fn each_step_of_the_sleep_waits_for_the_answer_of_its_task() {
    let (mut shell, jobs) = idle_timeout();
    // Step 1: `asleep`, then the disconnect.
    assert_eq!(
        names(&jobs),
        ["publish dashboard/test/status 1", "disconnect 1"]
    );
    assert!(shell.view().asleep);

    // The stream ends: the disconnect is done. Step 2: Wi-Fi off.
    let jobs = shell.handle(Signal::Mqtt {
        connection: 1,
        event: MqttEvent::Disconnected { reason: None },
    });
    assert_eq!(names(&jobs), ["wifi off"]);

    // The radio is off. Step 3: the last image, then the sleep.
    let jobs = shell.handle(Signal::RadioOff);
    let names = names(&jobs);
    let render = names.iter().position(|name| name == "render").unwrap();
    let sleep = names.iter().position(|name| name == "sleep").unwrap();
    assert!(render < sleep, "{names:?}");
    assert!(shell.is_sleeping());

    // A touch with no point wakes the device. The core connects again.
    let jobs = shell.handle(Signal::Woke(Wake::Touch(None)));
    assert!(!shell.is_sleeping());
    assert!(!shell.view().asleep);
    assert!(
        self::names(&jobs).contains(&"wifi on 2".into()),
        "{:?}",
        self::names(&jobs)
    );
}

#[test]
fn a_disconnect_with_no_connection_ends_at_once() {
    // Wi-Fi never joins, so no connection opens.
    let (mut shell, _) = started(PowerSource::Battery, true);
    let jobs = shell.handle(sleep_now());
    // The disconnect ends at once, so Wi-Fi off follows in the same batch.
    assert_eq!(names(&jobs), ["wifi off"]);
}

#[test]
fn a_tap_while_sleep_waits_is_the_wake_and_acts_on_its_control() {
    let mut shell = asleep();
    // A tap on the next-page button wakes the device and turns the page.
    let path = shell.view().path;
    let jobs = shell.handle(Signal::Tap(ui::layout::next().center()));
    assert!(!shell.is_sleeping());
    assert!(!shell.view().asleep);
    assert_ne!(shell.view().path, path);
    assert!(
        names(&jobs).contains(&"wifi on 2".into()),
        "{:?}",
        names(&jobs)
    );
}

#[test]
fn a_tap_on_a_light_while_sleep_waits_is_sent_after_the_reconnect() {
    let mut shell = asleep_with(false);
    let jobs = shell.handle(Signal::Tap(ui::layout::tile(0).unwrap().center()));
    assert!(!shell.is_sleeping());
    // No connection is open, so the toggle waits in the outbox.
    let command = String::from("publish dashboard/test/cmd 2");
    assert!(!names(&jobs).contains(&command), "{:?}", names(&jobs));

    shell.handle(joined(2));
    let jobs = shell.handle(Signal::Mqtt {
        connection: 2,
        event: MqttEvent::Connected,
    });
    assert!(names(&jobs).contains(&command), "{:?}", names(&jobs));
}

#[test]
fn a_change_of_the_power_source_while_sleep_waits_ends_it() {
    let mut shell = asleep();
    // A change of the charge does not end the sleep.
    shell.handle(Signal::Event(Event::BatteryChanged(Some(50))));
    assert!(shell.is_sleeping());

    shell.handle(Signal::Event(Event::PowerSourceChanged(PowerSource::Stand)));
    assert!(!shell.is_sleeping());
    assert!(!shell.view().asleep);
}

#[test]
fn a_wake_with_no_sleep_request_is_ignored() {
    let mut shell = online(true);
    assert_eq!(shell.handle(Signal::Woke(Wake::Timer)), Vec::new());
    assert_eq!(shell.handle(Signal::RadioOff), Vec::new());
}
