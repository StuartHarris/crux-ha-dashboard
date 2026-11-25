//! Tests of the key feature: short and long presses, the edges in the
//! wrong order, the wake, and a sleep or a start during a press.

use alloc::{string::String, vec::Vec};
use core::time::Duration;

use crux_core::App;

use super::{Event as KeyEvent, LONG_PRESS, Press};
use crate::{
    Control, Dashboard, Depth, Event, KeyEdge, PowerSource, Route,
    capabilities::power::Wake,
    features::{
        connection::Link,
        light::{DEFAULT_BATTERY_LEVEL, DEFAULT_STAND_LEVEL},
    },
    tests::{asleep, drive_to_connected, flows::house, shell::Shell, start_at},
};

/// The page of a room, where the tests start.
const ROOM: &str = "/room/living_room";

/// The button topic of the test settings.
const BUTTON: &str = "dashboard/hall/set/button";

// The tests below advance the clock by parts of `LONG_PRESS`.
const _: () = assert!(LONG_PRESS.as_millis() == 1000);

/// The line of a long press while the device is awake.
const LONG_LINE: &str = "debug device: the key asked for a full refresh";

/// Starts the house on `power` on the page of a room, connects, and
/// clears the logs.
fn connected(power: PowerSource) -> Shell {
    let mut shell = house();
    shell.send(start_at(power, Some(ROOM)));
    assert_eq!(shell.model.connection.link(), Link::Up);
    shell.take_log();
    shell.take_logs();
    shell
}

/// Returns the route of [`ROOM`].
fn room() -> Route {
    Route::from_path(ROOM).expect("a valid path")
}

/// Sends a `Down` edge of the key.
fn down(shell: &mut Shell) {
    shell.send(Event::Key(KeyEdge::Down));
}

/// Sends an `Up` edge of the key.
fn up(shell: &mut Shell) {
    shell.send(Event::Key(KeyEdge::Up));
}

/// Returns the depth of the last frame, or `None` for a partial refresh.
fn last_full(shell: &Shell) -> Option<Depth> {
    shell.frames.last().and_then(|frame| frame.full)
}

/// Returns the lines of the log of the area `device`.
fn device_lines(shell: &mut Shell) -> Vec<String> {
    shell
        .take_logs()
        .into_iter()
        .filter(|line| line.contains(" device: "))
        .collect()
}

#[test]
fn a_short_press_shows_the_index_page() {
    let mut shell = connected(PowerSource::Stand);
    let frames = shell.frames.len();

    shell.send(Event::Key(KeyEdge::Down));
    // The `Down` edge does not draw.
    assert_eq!(shell.frames.len(), frames);
    shell.advance(Duration::from_millis(999));
    up(&mut shell);

    assert_eq!(shell.model.route, Route::Home);
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(last_full(&shell), Some(Depth::Light));
    assert!(matches!(shell.model.key.press(), Press::Idle));
    // The `Up` edge cleared the long-press timer, so no refresh follows.
    shell.advance(LONG_PRESS);
    assert_eq!(shell.frames.len(), frames + 1);
}

#[test]
fn a_short_press_on_the_index_page_does_not_draw() {
    let mut shell = connected(PowerSource::Stand);
    shell.send(Event::Navigate(Route::Home));
    let frames = shell.frames.len();
    let seq = shell.model.full_refresh.seq;

    down(&mut shell);
    up(&mut shell);

    assert_eq!(shell.model.route, Route::Home);
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.model.full_refresh.seq, seq);
}

#[test]
fn a_long_press_asks_for_a_deep_full_refresh_while_the_key_is_down() {
    let mut shell = connected(PowerSource::Stand);
    let seq = shell.model.full_refresh.seq;
    let frames = shell.frames.len();

    down(&mut shell);
    shell.advance(LONG_PRESS);

    // The refresh starts while the key is still down.
    assert_eq!(shell.model.full_refresh.seq, seq + 1);
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(last_full(&shell), Some(Depth::Deep));
    assert_eq!(shell.model.refresh.since_deep, 0);
    assert!(matches!(shell.model.key.press(), Press::Acted));
    assert_eq!(device_lines(&mut shell), [LONG_LINE]);

    // The `Up` edge does nothing: the page stays, and nothing draws.
    up(&mut shell);
    assert_eq!(shell.model.route, room());
    assert_eq!(shell.frames.len(), frames + 1);
    assert!(matches!(shell.model.key.press(), Press::Idle));
}

#[test]
fn a_press_is_a_touch() {
    let mut shell = connected(PowerSource::Battery);
    let idle = shell.model.power.idle;
    assert_eq!(shell.light, 0);

    down(&mut shell);

    // The light goes on, and the idle timer starts again.
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());
    assert_ne!(shell.model.power.idle, idle);
}

#[test]
fn an_up_with_no_down_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    let mut command = app.update(Event::Key(KeyEdge::Up), &mut model);

    command.expect_no_effect_or_events();
    assert!(command.is_done());
    assert!(matches!(model.key.press(), Press::Idle));
}

#[test]
fn a_repeated_down_keeps_the_first_timer() {
    let mut shell = connected(PowerSource::Stand);
    let seq = shell.model.full_refresh.seq;

    down(&mut shell);
    assert_eq!(shell.light, DEFAULT_STAND_LEVEL.get());
    shell.advance(Duration::from_millis(600));
    let timers = shell.waiting();
    down(&mut shell);
    // The second `Down` starts no timer.
    assert_eq!(shell.waiting(), timers);

    // The press is long at `LONG_PRESS` after the first `Down`.
    shell.advance(Duration::from_millis(400));
    assert_eq!(shell.model.full_refresh.seq, seq + 1);
}

#[test]
fn a_down_after_a_long_press_does_nothing() {
    let mut shell = connected(PowerSource::Stand);
    down(&mut shell);
    shell.advance(LONG_PRESS);
    let seq = shell.model.full_refresh.seq;
    let timers = shell.waiting();

    down(&mut shell);
    shell.advance(LONG_PRESS);

    assert!(matches!(shell.model.key.press(), Press::Acted));
    assert_eq!(shell.model.full_refresh.seq, seq);
    assert_eq!(shell.waiting().len(), timers.len());
}

#[test]
fn a_stale_long_press_timer_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::Key(KeyEdge::Down), &mut model);
    let Press::Held { timer } = model.key.press() else {
        panic!("the key is not held: {:?}", model.key.press());
    };
    let id = timer.id();
    let _ = app.update(Event::Key(KeyEdge::Up), &mut model);
    let seq = model.full_refresh.seq;

    // The expiry was on its way when the key went up.
    let mut command = app.update(Event::KeyTimer(KeyEvent::LongPress(id)), &mut model);
    command.expect_no_effect_or_events();
    assert!(command.is_done());
    assert_eq!(model.full_refresh.seq, seq);

    // An expiry with an old id while the key is down is also stale.
    let _ = app.update(Event::Key(KeyEdge::Down), &mut model);
    let mut command = app.update(Event::KeyTimer(KeyEvent::LongPress(id)), &mut model);
    command.expect_no_effect_or_events();
    assert!(matches!(model.key.press(), Press::Held { .. }));
}

#[test]
fn a_long_press_while_a_restart_waits_is_ignored_with_a_line() {
    let mut shell = connected(PowerSource::Stand);
    shell.broker.hold_closes = true;
    shell.broker.publish(BUTTON, b"restart", false);
    shell.run();
    let seq = shell.model.full_refresh.seq;
    shell.take_logs();

    down(&mut shell);
    shell.advance(LONG_PRESS);

    assert_eq!(shell.model.full_refresh.seq, seq);
    assert_eq!(
        device_lines(&mut shell),
        [
            "debug device: the key asked for a full refresh, but the chip restarts, so the core \
             ignored it"
        ]
    );
}

#[test]
fn a_sleep_while_the_key_is_down_clears_the_long_press_timer() {
    let mut shell = connected(PowerSource::Battery);
    down(&mut shell);

    // The shell panics if a timer of the core waits at the `Sleep` request.
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    assert!(matches!(shell.model.key.press(), Press::Idle));
    assert!(shell.take_logs().contains(
        &"debug device: the sleep starts while the key is down, so the press does nothing".into()
    ));
    let seq = shell.model.full_refresh.seq;

    // The `Up` edge after the sleep does nothing, and the device sleeps.
    up(&mut shell);
    assert!(asleep(&shell.model));
    assert!(shell.sleep.is_some());
    assert_eq!(shell.model.full_refresh.seq, seq);
    assert_eq!(shell.model.route, room());
}

#[test]
fn a_press_that_wakes_the_device_shows_the_index_page_with_one_render() {
    let mut shell = connected(PowerSource::Battery);
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    shell.take_log();
    let frames = shell.frames.len();

    // The shell answers the `Sleep` request with `Wake::Key`.
    down(&mut shell);
    assert!(shell.log.contains(&"Wake Key".into()), "{:?}", shell.log);
    assert!(!asleep(&shell.model));
    assert_eq!(shell.light, DEFAULT_BATTERY_LEVEL.get());
    assert_eq!(shell.model.connection.link(), Link::Up);
    // The wake draws the index page at once, with one light full refresh.
    assert_eq!(shell.model.route, Route::Home);
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(last_full(&shell), Some(Depth::Light));
    assert!(matches!(shell.model.key.press(), Press::Acted));
    // The press starts no long-press timer.
    assert!(!shell.waiting().contains(&LONG_PRESS));

    // The `Up` edge does nothing, also after `LONG_PRESS`.
    let seq = shell.model.full_refresh.seq;
    shell.advance(LONG_PRESS);
    up(&mut shell);
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(shell.model.full_refresh.seq, seq);
    assert!(matches!(shell.model.key.press(), Press::Idle));
}

#[test]
fn a_press_that_wakes_the_device_on_the_index_page_draws_only_the_wake() {
    let mut shell = connected(PowerSource::Battery);
    shell.send(Event::Navigate(Route::Home));
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());
    let frames = shell.frames.len();

    shell.wake(Wake::Key);
    up(&mut shell);

    assert_eq!(shell.model.route, Route::Home);
    assert_eq!(shell.frames.len(), frames + 1);
}

#[test]
fn a_full_refresh_during_a_hold_makes_the_press_short() {
    let mut shell = connected(PowerSource::Stand);
    down(&mut shell);
    // Home Assistant asks for a full refresh while the key is down.
    shell.broker.publish(BUTTON, b"refresh", false);
    shell.run();
    assert!(matches!(shell.model.key.press(), Press::ShortOnly));
    assert!(!shell.waiting().contains(&LONG_PRESS));
    let seq = shell.model.full_refresh.seq;

    // The `Up` edge arrives late, but the press is short.
    shell.advance(Duration::from_millis(1800));
    assert_eq!(shell.model.full_refresh.seq, seq);
    up(&mut shell);

    assert_eq!(shell.model.route, Route::Home);
    assert_eq!(shell.model.full_refresh.seq, seq + 1);
    assert_eq!(last_full(&shell), Some(Depth::Light));
    assert!(matches!(shell.model.key.press(), Press::Idle));
}

#[test]
fn a_stale_long_press_timer_in_short_only_does_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::Key(KeyEdge::Down), &mut model);
    let Press::Held { timer } = model.key.press() else {
        panic!("the key is not held: {:?}", model.key.press());
    };
    let id = timer.id();
    // A page change is a light full refresh.
    let _ = app.update(Event::NextPage, &mut model);
    assert!(matches!(model.key.press(), Press::ShortOnly));
    let seq = model.full_refresh.seq;

    let mut command = app.update(Event::KeyTimer(KeyEvent::LongPress(id)), &mut model);

    command.expect_no_effect_or_events();
    assert!(command.is_done());
    assert_eq!(model.full_refresh.seq, seq);
    assert!(matches!(model.key.press(), Press::ShortOnly));
}

#[test]
fn a_press_during_a_hold_wakes_the_device() {
    let mut shell = connected(PowerSource::Battery);
    shell.send(Event::SleepNow);
    // A wake with no cause holds the device awake with no `Sleep` request.
    shell.stray();
    assert!(shell.sleep.is_none());
    assert!(asleep(&shell.model));

    down(&mut shell);

    assert!(!asleep(&shell.model));
    assert_eq!(shell.model.route, Route::Home);
    assert!(matches!(shell.model.key.press(), Press::Acted));
}

#[test]
fn a_key_wake_after_a_lost_up_still_wakes_the_device() {
    let mut shell = connected(PowerSource::Battery);
    down(&mut shell);
    shell.advance(LONG_PRESS);
    assert!(matches!(shell.model.key.press(), Press::Acted));
    shell.send(Event::SleepNow);
    assert!(shell.sleep.is_some());

    // The shell lost the `Up` edge. The next press still wakes the device.
    shell.wake(Wake::Key);

    assert!(!asleep(&shell.model));
    assert_eq!(shell.model.route, Route::Home);
}

#[test]
fn a_start_forgets_the_press() {
    let mut shell = connected(PowerSource::Stand);
    down(&mut shell);
    shell.send(start_at(PowerSource::Stand, Some(ROOM)));
    assert!(matches!(shell.model.key.press(), Press::Idle));
    let frames = shell.frames.len();

    up(&mut shell);
    shell.advance(LONG_PRESS);

    assert_eq!(shell.model.route, room());
    assert_eq!(shell.frames.len(), frames);
}

#[test]
fn a_tap_while_the_key_is_down_cancels_the_press() {
    let mut shell = connected(PowerSource::Stand);
    down(&mut shell);
    shell.take_logs();

    // The tap turns the page. It acts, and it cancels the press.
    shell.send(Event::Tapped(Some(Control::NextPage)));
    let page = shell.model.route.clone();
    assert_ne!(page, room());
    assert!(matches!(shell.model.key.press(), Press::Cancelled));
    assert!(!shell.waiting().contains(&LONG_PRESS));
    assert_eq!(
        device_lines(&mut shell),
        ["debug device: a tap on the panel while the key is down cancels the press"]
    );
    let seq = shell.model.full_refresh.seq;
    let frames = shell.frames.len();

    // No long press follows, and the `Up` edge does not show the index page.
    shell.advance(LONG_PRESS);
    up(&mut shell);
    assert_eq!(shell.model.route, page);
    assert_eq!(shell.model.full_refresh.seq, seq);
    assert_eq!(shell.frames.len(), frames);
    assert!(matches!(shell.model.key.press(), Press::Idle));
}

#[test]
fn a_tap_cancels_a_short_only_press() {
    let mut shell = connected(PowerSource::Stand);
    down(&mut shell);
    shell.broker.publish(BUTTON, b"refresh", false);
    shell.run();
    assert!(matches!(shell.model.key.press(), Press::ShortOnly));

    shell.send(Event::Tapped(None));
    assert!(matches!(shell.model.key.press(), Press::Cancelled));
    let frames = shell.frames.len();

    up(&mut shell);
    assert_eq!(shell.model.route, room());
    assert_eq!(shell.frames.len(), frames);
}

#[test]
fn a_tap_after_a_long_press_does_not_change_the_press() {
    let mut shell = connected(PowerSource::Stand);
    down(&mut shell);
    shell.advance(LONG_PRESS);
    assert!(matches!(shell.model.key.press(), Press::Acted));

    shell.send(Event::Tapped(None));
    assert!(matches!(shell.model.key.press(), Press::Acted));
}
