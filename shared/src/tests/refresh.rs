//! Tests of the refresh feature: the full refresh after a number of partial
//! renders and after a time.

use core::time::Duration;

use crux_core::App;

use super::{ExpectFullRefreshTimer, connection::drive_to_connected, shell::Shell, start};
use crate::{
    Dashboard, EffectTestExt, Event, Model, PowerSource,
    capabilities::power::Wake,
    features::power::IDLE_TIMEOUT,
    features::refresh::{
        DRAW_DELAY, FULL_REFRESH_INTERVAL, PARTIALS_PER_FULL, redraw, redraw_soon,
    },
};

/// Changes the view model: the status text shows `count` taps not sent.
const fn change_the_view(model: &mut Model, count: usize) {
    model.unsent = count;
}

#[test]
fn the_render_after_the_last_partial_render_is_a_full_refresh() {
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    // The start was a full refresh, and `Connected` drew once.
    let seq = model.full_refresh_seq;
    let partials = model.refresh.partials;

    for count in partials..PARTIALS_PER_FULL {
        change_the_view(&mut model, count as usize + 1);
        redraw(&mut model, false).expect_only_render();
    }
    assert_eq!(model.full_refresh_seq, seq);

    change_the_view(&mut model, 100);
    redraw(&mut model, false)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(model.full_refresh_seq, seq + 1);
    assert_eq!(model.refresh.partials, 0);

    // The count starts again.
    change_the_view(&mut model, 101);
    redraw(&mut model, false).expect_only_render();
    assert_eq!(model.full_refresh_seq, seq + 1);
    assert_eq!(model.refresh.partials, 1);
}

#[test]
fn a_render_with_no_change_to_the_view_does_not_draw_or_count() {
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let partials = model.refresh.partials;
    redraw(&mut model, false).expect_no_effects();
    assert_eq!(model.refresh.partials, partials);

    // A requested full refresh always draws.
    redraw(&mut model, true)
        .expect_render()
        .expect_full_refresh_timer();
}

#[test]
fn a_repeated_state_message_does_not_draw_or_count() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    let frames = shell.frames.len();
    let partials = shell.model.refresh.partials;

    // Home Assistant publishes the same state again.
    shell.take_log();
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.model.refresh.partials, partials);
    assert!(!shell.take_log().contains(&"Render".into()));
}

#[test]
fn a_full_refresh_starts_the_count_again() {
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    for count in 0..10 {
        change_the_view(&mut model, count + 1);
        let _ = redraw(&mut model, false);
    }
    assert!(model.refresh.partials >= 10);
    let _ = Dashboard.update(Event::NextPage, &mut model);
    assert_eq!(model.refresh.partials, 0);
}

#[test]
fn the_timer_asks_for_a_full_refresh_while_the_device_is_awake() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    // `Connected` drew once, so a partial render occurred.
    assert!(model.refresh.partials > 0);
    let seq = model.full_refresh_seq;
    let id = model.refresh.timer;

    app.update(Event::FullRefreshDue(id), &mut model)
        .expect_render()
        .expect_full_refresh_timer();
    assert_eq!(model.full_refresh_seq, seq + 1);

    // The old timer is stale.
    app.update(Event::FullRefreshDue(id), &mut model)
        .expect_no_effects();
}

#[test]
fn the_timer_with_no_partial_render_only_starts_again() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
    let _ = app.update(Event::NextPage, &mut model);
    assert_eq!(model.refresh.partials, 0);
    let seq = model.full_refresh_seq;
    let id = model.refresh.timer;

    app.update(Event::FullRefreshDue(id), &mut model)
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), FULL_REFRESH_INTERVAL);
        });
    assert_eq!(model.full_refresh_seq, seq);
    assert_ne!(model.refresh.timer, id);
}

#[test]
fn the_timer_does_nothing_while_the_device_sleeps() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    let id = model.refresh.timer;
    model.power.asleep = true;

    app.update(Event::FullRefreshDue(id), &mut model)
        .expect_no_effects();
}

#[test]
fn on_the_stand_the_panel_has_a_full_refresh_each_interval() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    // A state message draws with a partial refresh.
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    assert!(!shell.frames.last().unwrap().full);
    let frames = shell.frames.len();

    assert_eq!(full_refreshes_due(&mut shell), 1);
    assert_eq!(shell.frames.len(), frames + 1);
    assert!(shell.frames.last().unwrap().full);

    // The full refresh started the next timer. With no partial render
    // since, the next interval draws nothing.
    let frames = shell.frames.len();
    assert_eq!(full_refreshes_due(&mut shell), 1);
    assert_eq!(shell.frames.len(), frames);
}

/// Moves the clock by [`FULL_REFRESH_INTERVAL`], and returns the number of
/// full-refresh timers that fired.
fn full_refreshes_due(shell: &mut Shell) -> usize {
    shell
        .advance(FULL_REFRESH_INTERVAL)
        .iter()
        .filter(|event| matches!(event, Event::FullRefreshDue(_)))
        .count()
}

#[test]
fn each_full_refresh_starts_the_timer_again() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.take_log();

    shell.send(Event::NextPage);
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    // Only the new timer runs.
    assert_eq!(full_refreshes_due(&mut shell), 1);
}

#[test]
fn no_full_refresh_timer_runs_while_the_device_sleeps() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    shell.advance(IDLE_TIMEOUT);
    assert!(shell.view().asleep);
    assert_eq!(full_refreshes_due(&mut shell), 0);

    // The wake draws with a full refresh and starts the timer again.
    shell.take_log();
    shell.wake(Wake::Timer);
    assert!(shell.frames.last().unwrap().full);
    let timer = alloc::format!("TimeNotifyAfter {}ms", FULL_REFRESH_INTERVAL.as_millis());
    assert!(shell.take_log().contains(&timer));
}

#[test]
fn many_state_messages_give_a_full_refresh_after_the_partial_renders() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    let first = shell.frames.len();

    // Each change of a visible tile draws once, with no extra draw for
    // `last_updated`.
    for index in 0..=PARTIALS_PER_FULL {
        let state = if index % 2 == 0 { "on" } else { "off" };
        shell.broker.set_state("light.all_lights", state, &[]);
        shell.run();
    }
    let full: usize = shell.frames[first..]
        .iter()
        .filter(|frame| frame.full)
        .count();
    assert_eq!(full, 1);
}

#[test]
fn changes_for_another_room_over_many_minutes_draw_nothing() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    // The index page does not show the humidity of a room.
    shell
        .broker
        .set_state("sensor.living_room_thermometer_humidity", "40", &[]);
    shell.run();
    let frames = shell.frames.len();
    shell.take_log();

    // More than a minute passes with changes at the start and the end.
    for (humidity, minutes) in [("41", 0), ("42", 1), ("43", 2)] {
        shell.broker.time += minutes * 60;
        shell
            .broker
            .set_state("sensor.living_room_thermometer_humidity", humidity, &[]);
        shell.run();
    }
    assert_eq!(shell.frames.len(), frames);
    // The changes give no effects: no render and no draw timer.
    let log = shell.take_log();
    assert_eq!(log, [] as [&str; 0]);
    assert_eq!(shell.view().updated, "");

    // The living room page shows the time of the last change: 09:44 in BST.
    shell.send(Event::Navigate(crate::Route::room("living_room")));
    assert_eq!(shell.view().updated, "Last changed 09:44");
}

#[test]
fn the_retained_messages_at_a_connection_give_one_draw_timer_and_one_render() {
    let mut shell = Shell::new();
    shell.auto_draw = false;
    for (entity_id, state) in [
        ("light.all_lights", "on"),
        ("weather.home", "rainy"),
        ("light.loft_loft_1", "on"),
        ("sensor.living_room_thermometer_humidity", "40"),
    ] {
        shell.broker.set_state(entity_id, state, &[]);
    }
    shell.send(start(PowerSource::Stand));
    // The start draws, and `Connected` draws the connection. The burst of
    // retained messages then starts one draw timer, and does not draw.
    let frames = shell.frames.len();
    let log = shell.take_log();
    let draws = log
        .iter()
        .filter(|line| *line == "TimeNotifyAfter 200ms")
        .count();
    assert_eq!(draws, 1, "{log:?}");
    let renders = log.iter().filter(|line| *line == "Render").count();
    assert_eq!(renders, 2, "{log:?}");

    // The timer draws all the changes once.
    let events = shell.advance(DRAW_DELAY);
    assert!(matches!(events[..], [Event::DrawDue(_)]), "{events:?}");
    assert_eq!(shell.frames.len(), frames + 1);
    assert_eq!(shell.take_log(), ["Render"]);
    let view = &shell.frames.last().unwrap().view;
    assert_eq!(view.updated, "Last changed 09:41");
    let all_lights = view.index().unwrap().all_lights.as_ref().unwrap();
    assert_eq!(all_lights.state, crate::TileState::On);
}

#[test]
fn a_draw_timer_that_expires_while_the_device_sleeps_draws_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
    super::set_state(&mut model, "light.all_lights", "on");
    let _timer = redraw_soon(&mut model);
    let id = model.refresh.draw;
    model.power.asleep = true;

    app.update(Event::DrawDue(id), &mut model)
        .expect_no_effects();

    // The timer no longer waits, so the next change after the wake starts a
    // new timer.
    model.power.asleep = false;
    redraw_soon(&mut model).expect_only_time_notify_after_with(|notify| {
        assert_eq!(Duration::from(notify.duration), DRAW_DELAY);
    });
}

#[test]
fn a_draw_timer_that_waits_when_the_device_sleeps_draws_nothing() {
    let mut shell = Shell::new();
    shell.auto_draw = false;
    shell.send(start(PowerSource::Battery));
    // A change comes just before the idle timeout.
    let before = IDLE_TIMEOUT.saturating_sub(DRAW_DELAY / 2);
    shell.advance(before);
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    assert!(shell.model.refresh.draw_waits());

    // The idle timeout puts the device to sleep. The last image is a full
    // refresh with the change.
    shell.advance(IDLE_TIMEOUT.saturating_sub(before));
    assert!(shell.view().asleep);
    let frames = shell.frames.len();
    assert!(shell.frames.last().unwrap().full);

    // The full refresh cleared the draw timer, so the timer does not fire.
    assert!(!shell.model.refresh.draw_waits());
    shell.take_log();
    let events = shell.advance(DRAW_DELAY);
    assert_eq!(events, []);
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.take_log(), [] as [&str; 0]);
}

#[test]
fn a_draw_timer_after_a_newer_timer_draws_nothing() {
    let app = Dashboard;
    let (mut model, _wire) = drive_to_connected(PowerSource::Stand);

    // A change starts the first timer.
    super::set_state(&mut model, "light.all_lights", "on");
    let _first = redraw_soon(&mut model);
    let first = model.refresh.draw;

    // A page turn draws the whole screen, so the first timer is stale. The
    // next change starts a newer timer.
    let _ = app.update(Event::NextPage, &mut model);
    super::set_state(&mut model, "light.living_room_lights", "on");
    let _second = redraw_soon(&mut model);
    let second = model.refresh.draw;
    assert_ne!(first, second);

    app.update(Event::DrawDue(first), &mut model)
        .expect_no_effects();
    app.update(Event::DrawDue(second), &mut model)
        .expect_only_render();
    // A timer draws only once.
    app.update(Event::DrawDue(second), &mut model)
        .expect_no_effects();
}

#[test]
fn a_full_refresh_clears_the_draw_timer_that_waits() {
    let mut shell = Shell::new();
    shell.auto_draw = false;
    shell.send(start(PowerSource::Stand));
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    assert!(shell.model.refresh.draw_waits());
    shell.take_log();

    // A page turn is a full refresh, so the draw timer is stale.
    shell.send(Event::NextPage);
    assert!(!shell.model.refresh.draw_waits());
    let log = shell.take_log();
    let clears = log.iter().filter(|effect| *effect == "TimeClear").count();
    // The full-refresh timer and the draw timer.
    assert_eq!(clears, 2, "{log:?}");

    let events = shell.advance(DRAW_DELAY);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::DrawDue(_))),
        "{events:?}"
    );
}
