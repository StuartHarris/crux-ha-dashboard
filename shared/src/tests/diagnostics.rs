//! Tests of the Wi-Fi and boot diagnostics: the facts that the core
//! publishes in the state message, the start line, the lines from before a
//! restart, and the wake reason.
//!
//! The flows run the core in the test shell against the fake broker and
//! the fake radio.

use alloc::{string::String, vec, vec::Vec};
use core::time::Duration;

use crux_core::App;
use serde_json::{Value, json};

use super::{POWER_ON, flows::house, shell::Shell, start, start_at};
use crate::{
    Boot, BootLine, Dashboard, Effect, Event, Model, PowerSource, Reading, ResetReason, Start,
    capabilities::{log::Level, power::Wake},
    fake::radio::{HALL, IP},
    features::{
        connection::{Link, RECONNECT_MIN},
        device::{STATE_MIN_INTERVAL, Snapshot, Wifi},
        logs::{BOOT_PREFIX, BOOT_SOURCE, Kept},
        power::{IDLE_TIMEOUT, WakeReason},
    },
};

/// The state topic of the test settings.
const STATE: &str = "dashboard/hall/state";

/// The log topic of the test settings.
const LOG: &str = "dashboard/hall/log";

/// Returns the `Start` event on the stand with the boot facts `boot` and
/// the lines `lines` from before the restart.
fn start_after(boot: Boot, lines: Vec<BootLine>) -> Event {
    let Event::Start(start) = start(PowerSource::Stand) else {
        unreachable!()
    };
    Event::Start(Start {
        boot,
        before_restart: lines,
        ..start
    })
}

/// Returns each state message that the broker got, as JSON.
fn states(shell: &Shell) -> Vec<Value> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == STATE)
        .map(|publish| serde_json::from_slice(&publish.payload).expect("JSON"))
        .collect()
}

/// Returns the value of `key` in the last state message.
fn last(shell: &Shell, key: &str) -> Value {
    states(shell).last().expect("a state message")[key].clone()
}

/// Returns each message on the log topic that the broker got, as JSON.
fn lines(shell: &Shell) -> Vec<Value> {
    shell
        .broker
        .published
        .iter()
        .filter(|publish| publish.topic == LOG)
        .map(|publish| serde_json::from_slice(&publish.payload).expect("JSON"))
        .collect()
}

/// Returns `true` if the timer of the throttle window waits.
fn window_waits(shell: &Shell) -> bool {
    shell.waiting().contains(&STATE_MIN_INTERVAL)
}

/// Ends the session as when the link fails, and connects again after the
/// wait.
fn fail_and_reconnect(shell: &mut Shell, wait: Duration) {
    shell.broker.drop_connection();
    shell.run();
    assert_ne!(shell.model.connection.link(), Link::Up);
    shell.advance(wait);
    assert_eq!(shell.model.connection.link(), Link::Up);
}

/// Starts on battery, and sleeps at the idle timeout.
fn asleep_on_battery() -> Shell {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell
}

// The Wi-Fi facts.

#[test]
fn the_state_has_the_address_of_the_lease_and_the_access_point_and_the_signal_of_the_join() {
    let mut shell = house();
    shell.radio.ip = [10, 0, 0, 7];
    shell.send(start(PowerSource::Stand));
    let state = states(&shell).remove(0);
    assert_eq!(state["ip"], "10.0.0.7");
    assert_eq!(state["bssid"], "02:00:00:00:00:01");
    assert_eq!(state["rssi"], -55);
    assert_eq!(state["reconnects"], 0);
}

#[test]
fn a_wake_scan_that_sees_the_access_point_publishes_its_new_signal_in_the_window() {
    let mut shell = asleep_on_battery();
    shell.hold_radio = true;

    // A tap wakes the device. The join gives -55 dBm.
    shell.send(Event::Tapped(None));
    shell.answer_radio();
    // The signal changes before the scan.
    shell.radio.access_points[0].rssi = -61;
    shell.answer_radio();
    assert_eq!(shell.model.connection.link(), Link::Up);
    let before = states(&shell).len();
    assert_eq!(last(&shell, "rssi"), -55);
    assert!(!window_waits(&shell));

    // The scan after the subscribe sees the access point. The window is
    // open, so the new signal goes at once, and the window is throttled.
    shell.answer_radio();
    assert_eq!(states(&shell).len(), before + 1);
    assert_eq!(last(&shell, "rssi"), -61);
    assert_eq!(last(&shell, "bssid"), "02:00:00:00:00:01");
    assert!(window_waits(&shell));

    // The next sleep ends the session, which clears the timer of the
    // window. No timer of the core waits at the `Sleep` request.
    shell.hold_radio = false;
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    assert_eq!(shell.timers_at_sleep.last(), Some(&Vec::new()));
}

#[test]
fn each_failed_session_counts_one_reconnect_until_the_next_start() {
    let mut shell = house();
    shell.send(start(PowerSource::Stand));
    assert_eq!(last(&shell, "reconnects"), 0);

    fail_and_reconnect(&mut shell, RECONNECT_MIN);
    assert_eq!(last(&shell, "reconnects"), 1);
    fail_and_reconnect(&mut shell, RECONNECT_MIN * 2);
    assert_eq!(last(&shell, "reconnects"), 2);
    // The timer ids of the reconnects are a separate count.
    assert_eq!(shell.model.connection.reconnects, 2);

    // A start is a boot, so the count starts again.
    shell.send(start(PowerSource::Stand));
    assert_eq!(last(&shell, "reconnects"), 0);
}

#[test]
fn a_link_that_fails_and_a_sleep_do_not_count_as_reconnects() {
    // The router blocks the device, so the join fails and the link is down
    // before any session.
    let mut shell = house();
    shell.radio.blocked = true;
    shell.send(start(PowerSource::Stand));
    assert_ne!(shell.model.connection.link(), Link::Up);
    shell.radio.blocked = false;
    shell.advance(Duration::from_secs(30));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(last(&shell, "reconnects"), 0);

    // A sleep closes the session cleanly, and the wake connects again.
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(last(&shell, "reconnects"), 0);
}

#[test]
fn the_wifi_facts_come_only_from_a_link_with_an_address() {
    let mut model = Model::default();
    let _ = Dashboard.update(start(PowerSource::Stand), &mut model);
    // The core joins, but has no lease yet.
    let snapshot = Snapshot::of(&model).expect("a snapshot after Start");
    assert_eq!(snapshot.wifi, None);
    // Before `Start`, there is no snapshot.
    assert_eq!(Snapshot::of(&Model::default()), None);
}

#[test]
fn an_association_during_a_sleep_has_no_wifi_facts() {
    let mut model = Model::default();
    let _ = Dashboard.update(start(PowerSource::Battery), &mut model);
    // The device associated with an access point after a sleep started. The
    // core keeps no address for it, so the snapshot has no Wi-Fi facts.
    model.wifi.phase = crate::features::wifi::Phase::Associated(HALL);
    let snapshot = Snapshot::of(&model).expect("a snapshot after Start");
    assert_eq!(snapshot.wifi, None);
}

// The urgent and the throttled fields.

/// Returns a snapshot on the stand with a link to [`HALL`].
fn snapshot() -> Snapshot {
    Snapshot {
        battery: Some(80),
        stand: true,
        reading: Reading::default(),
        wifi: Some(Wifi {
            bssid: HALL.bssid,
            ip: IP,
            rssi: Some(-55),
        }),
        reconnects: 0,
        boot: POWER_ON,
        wake: WakeReason::Boot,
    }
}

#[test]
fn the_address_the_access_point_the_boot_and_the_wake_are_urgent() {
    let sent = snapshot();
    let with_wifi = |change: fn(&mut Wifi)| {
        let mut snapshot = snapshot();
        if let Some(wifi) = &mut snapshot.wifi {
            change(wifi);
        }
        snapshot
    };
    let urgent = [
        with_wifi(|wifi| wifi.ip = [192, 168, 1, 51]),
        with_wifi(|wifi| wifi.bssid = [0x02, 0, 0, 0, 0, 0x02]),
        Snapshot {
            wifi: None,
            ..snapshot()
        },
        Snapshot {
            boot: Boot {
                restarts: 1,
                ..POWER_ON
            },
            ..snapshot()
        },
        Snapshot {
            boot: Boot {
                reset: ResetReason::Software,
                ..POWER_ON
            },
            ..snapshot()
        },
        Snapshot {
            wake: WakeReason::Touch,
            ..snapshot()
        },
        Snapshot {
            wake: WakeReason::Stand,
            ..snapshot()
        },
    ];
    for now in urgent {
        assert!(now.is_urgent_after(&sent), "{now:?}");
    }
}

#[test]
fn the_signal_and_the_reconnects_are_throttled() {
    let sent = snapshot();
    let throttled = [
        Snapshot {
            wifi: Some(Wifi {
                rssi: Some(-70),
                ..sent.wifi.unwrap()
            }),
            ..snapshot()
        },
        Snapshot {
            wifi: Some(Wifi {
                rssi: None,
                ..sent.wifi.unwrap()
            }),
            ..snapshot()
        },
        Snapshot {
            reconnects: 5,
            ..snapshot()
        },
    ];
    for now in throttled {
        assert_ne!(now, sent);
        assert!(!now.is_urgent_after(&sent), "{now:?}");
    }
}

#[test]
fn the_state_has_null_wifi_facts_with_no_link_and_the_names_of_the_reset_and_the_wake() {
    let text = |snapshot: &Snapshot| -> Value {
        serde_json::from_slice(&snapshot.to_json()).expect("JSON")
    };
    let state = text(&Snapshot {
        wifi: None,
        reconnects: 3,
        boot: Boot {
            reset: ResetReason::Watchdog,
            restarts: 2,
        },
        wake: WakeReason::Stand,
        ..snapshot()
    });
    assert_eq!(state["rssi"], Value::Null);
    assert_eq!(state["bssid"], Value::Null);
    assert_eq!(state["ip"], Value::Null);
    assert_eq!(state["reconnects"], 3);
    assert_eq!(state["restarts"], 2);
    assert_eq!(state["reset"], "watchdog");
    assert_eq!(state["wake"], "stand");

    for (reset, name) in [
        (ResetReason::PowerOn, "power_on"),
        (ResetReason::Software, "software"),
        (ResetReason::Panic, "panic"),
        (ResetReason::Watchdog, "watchdog"),
        (ResetReason::Brownout, "brownout"),
        (ResetReason::Other, "other"),
    ] {
        let boot = Boot { reset, restarts: 1 };
        assert_eq!(text(&Snapshot { boot, ..snapshot() })["reset"], name);
    }
    for (wake, name) in [
        (WakeReason::Boot, "boot"),
        (WakeReason::Touch, "touch"),
        (WakeReason::Stand, "stand"),
    ] {
        assert_eq!(text(&Snapshot { wake, ..snapshot() })["wake"], name);
    }
}

// The start line and the restart count.

/// Returns the level and the text of the start line for `boot`.
fn start_line(boot: Boot) -> (Level, String) {
    let mut model = Model::default();
    let mut cmd = Dashboard.update(start_after(boot, Vec::new()), &mut model);
    let lines: Vec<(Level, String)> = cmd
        .effects()
        .filter_map(|effect| match effect {
            Effect::Log(request) => Some((request.operation.level, request.operation.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    lines.into_iter().next().unwrap()
}

#[test]
fn the_start_line_tells_the_reset_and_a_fault_is_a_warning() {
    for (reset, level, text) in [
        (ResetReason::PowerOn, Level::Info, "a power-on"),
        (ResetReason::Software, Level::Info, "a software reset"),
        (ResetReason::Other, Level::Info, "a reset of another kind"),
        (ResetReason::Panic, Level::Warn, "a panic"),
        (ResetReason::Watchdog, Level::Warn, "a watchdog reset"),
        (ResetReason::Brownout, Level::Warn, "a brownout"),
    ] {
        let restarts = u32::from(reset != ResetReason::PowerOn);
        let suffix = if restarts > 0 { ", restart 1" } else { "" };
        assert_eq!(
            start_line(Boot { reset, restarts }),
            (level, alloc::format!("power: started after {text}{suffix}")),
            "{reset:?}"
        );
    }
}

#[test]
fn the_start_line_and_the_state_have_the_restart_count() {
    let boot = Boot {
        reset: ResetReason::Watchdog,
        restarts: 3,
    };
    assert_eq!(
        start_line(boot),
        (
            Level::Warn,
            "power: started after a watchdog reset, restart 3".into()
        )
    );

    let mut shell = house();
    shell.send(start_after(boot, Vec::new()));
    assert_eq!(last(&shell, "restarts"), 3);
    assert_eq!(last(&shell, "reset"), "watchdog");
    assert_eq!(
        lines(&shell),
        [json!({
            "level": "warn",
            "source": "power",
            "age": 0,
            "msg": "started after a watchdog reset, restart 3",
        })]
    );
}

// The lines from before the restart.

#[test]
fn the_lines_from_before_the_restart_go_first_with_their_prefix_and_the_age_0() {
    let mut shell = house();
    let boot = Boot {
        reset: ResetReason::Software,
        restarts: 1,
    };
    let kept = vec![
        BootLine {
            level: Kept::Warn,
            source: "wifi".into(),
            message: "restart the chip: no address after 3 joins".into(),
        },
        BootLine {
            level: Kept::Error,
            source: "esp_radio".into(),
            message: "driver error 12".into(),
        },
    ];
    // The broker is down for 30 s, so the lines wait in the ring.
    shell.broker.down = true;
    shell.send(start_after(boot, kept));
    shell.advance(Duration::from_secs(30));
    shell.broker.down = false;
    shell.advance(Duration::from_secs(30));
    assert_eq!(shell.model.connection.link(), Link::Up);

    let published = lines(&shell);
    // The start line waited too, so it has an age.
    let age = published[2]["age"].as_u64().expect("an age");
    assert!(age >= 30, "{age}");
    assert_eq!(
        published[..3],
        [
            json!({
                "level": "warn",
                "source": BOOT_SOURCE,
                "age": 0,
                "msg": "before the restart: wifi: restart the chip: no address after 3 joins",
            }),
            json!({
                "level": "error",
                "source": "boot",
                "age": 0,
                "msg": "before the restart: esp_radio: driver error 12",
            }),
            json!({
                "level": "info",
                "source": "power",
                "age": age,
                "msg": "started after a software reset, restart 1",
            }),
        ]
    );
    assert!(BOOT_PREFIX.starts_with("before the restart"));
}

#[test]
fn a_long_line_from_before_the_restart_is_cut_with_its_prefix() {
    let mut model = Model::default();
    let long = "x".repeat(300);
    let event = start_after(
        Boot {
            reset: ResetReason::Panic,
            restarts: 1,
        },
        vec![BootLine {
            level: Kept::Error,
            source: String::new(),
            message: long,
        }],
    );
    let _ = Dashboard.update(event, &mut model);
    let line = model.logs.lines().next().expect("a line");
    let text = line.message.as_str();
    assert!(text.starts_with("before the restart: xxx"), "{text}");
    assert_eq!(text.len(), crate::features::logs::MAX_MESSAGE);
}

#[test]
fn a_second_start_keeps_no_lines_from_before_the_restart() {
    let mut model = Model::default();
    let boot = Boot {
        reset: ResetReason::Panic,
        restarts: 1,
    };
    let kept = || {
        vec![BootLine {
            level: Kept::Warn,
            source: "wifi".into(),
            message: "restart the chip: no address after 3 joins".into(),
        }]
    };
    let from_before = |model: &Model| {
        model
            .logs
            .lines()
            .filter(|line| line.source == crate::features::logs::Source::Boot)
            .count()
    };
    let _ = Dashboard.update(start_after(boot, kept()), &mut model);
    assert_eq!(from_before(&model), 1);

    // A second `Start` (for example, after the shell started again with
    // the same core) gives the same lines. The core keeps them only once.
    let _ = Dashboard.update(start_after(boot, kept()), &mut model);
    assert_eq!(from_before(&model), 1);
}

// The wake reason.

#[test]
fn the_wake_reason_is_boot_then_the_touch_or_the_stand_that_woke_the_device() {
    let mut shell = asleep_on_battery();
    assert_eq!(last(&shell, "wake"), "boot");

    // The check of the power source and a wake with no cause keep the
    // device asleep, so they change nothing.
    shell.check();
    shell.stray();
    shell.advance(Duration::from_secs(3));
    assert!(shell.sleep.is_some());
    assert_eq!(shell.model.power.wake(), WakeReason::Boot);

    // A touch wakes the device.
    shell.wake(Wake::Touch(None));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(last(&shell, "wake"), "touch");

    // A move to the stand and off it while the device is awake is not a
    // wake.
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    shell.send(Event::PowerSourceChanged(PowerSource::Battery));
    assert_eq!(last(&shell, "stand"), "OFF");
    assert_eq!(last(&shell, "wake"), "touch");

    // The stand wakes the device from the next sleep.
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.sleep.is_some());
    shell.send(Event::PowerSourceChanged(PowerSource::Stand));
    assert_eq!(shell.model.connection.link(), Link::Up);
    assert_eq!(last(&shell, "wake"), "stand");

    // A start is a boot.
    shell.send(start_at(PowerSource::Battery, None));
    assert_eq!(last(&shell, "wake"), "boot");
}

#[test]
fn a_wake_in_the_first_step_of_a_sleep_goes_at_once_also_in_a_throttled_window() {
    let mut shell = house();
    shell.send(start(PowerSource::Battery));
    // A change that is not urgent throttles the window.
    let reading = Reading {
        current_ma: Some(-40),
        ..Reading::default()
    };
    shell.send(Event::PowerRead(reading));
    assert!(window_waits(&shell));

    // A tap turns the light on, so the first step of the sleep waits for
    // the light while the connection is still up.
    shell.send(Event::Tapped(None));
    shell.hold_light = true;
    shell.send(Event::SleepNow);
    assert!(shell.model.power.is_asleep());
    assert_eq!(shell.model.connection.link(), Link::Up);
    let before = states(&shell).len();

    // A tap wakes the device in that step. The wake reason is urgent, so
    // the state goes at once, and the timer of the window still waits.
    shell.hold_light = false;
    shell.send(Event::Tapped(None));
    assert!(!shell.model.power.is_asleep());
    assert_eq!(states(&shell).len(), before + 1);
    assert_eq!(last(&shell, "wake"), "touch");
    assert!(window_waits(&shell));
}
