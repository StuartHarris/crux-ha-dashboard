//! Tests of the refresh feature: the full refresh after a number of partial
//! renders and after a time.

use core::time::Duration;

use crux_core::App;

use super::{ExpectFullRefreshTimer, connection::drive_to_connected, shell::Shell, start};
use crate::{
    Dashboard, EffectTestExt, Event, Model, PowerSource,
    features::refresh::{FULL_REFRESH_INTERVAL, PARTIALS_PER_FULL, redraw},
};

/// Changes the view model: the status text shows `count` taps not sent.
const fn change_the_view(model: &mut Model, count: usize) {
    model.unsent = count;
}

#[test]
fn the_render_after_the_last_partial_render_is_a_full_refresh() {
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Stand);
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
    let (mut model, _cmd, _request) = drive_to_connected(PowerSource::Battery);
    let id = model.refresh.timer;
    model.power.asleep = true;

    app.update(Event::FullRefreshDue(id), &mut model)
        .expect_no_effects();
}

#[test]
fn on_the_stand_the_panel_has_a_full_refresh_each_interval() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    assert_eq!(shell.full_refresh_timers(), 1);
    // A state message draws with a partial refresh.
    shell.broker.set_state("light.all_lights", "on", &[]);
    shell.run();
    assert!(!shell.frames.last().unwrap().full);
    let frames = shell.frames.len();

    shell.fire_full_refresh_timer();
    assert_eq!(shell.frames.len(), frames + 1);
    assert!(shell.frames.last().unwrap().full);
    // The full refresh starts the next timer.
    assert_eq!(shell.full_refresh_timers(), 1);

    // With no partial render since, the next interval draws nothing.
    let frames = shell.frames.len();
    shell.fire_full_refresh_timer();
    assert_eq!(shell.frames.len(), frames);
    assert_eq!(shell.full_refresh_timers(), 1);
}

#[test]
fn each_full_refresh_starts_the_timer_again() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    shell.take_log();

    shell.send(Event::NextPage);
    let log = shell.take_log();
    assert!(log.contains(&"TimeClear".into()), "{log:?}");
    assert_eq!(shell.full_refresh_timers(), 1);
}

#[test]
fn no_full_refresh_timer_runs_while_the_device_sleeps() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Battery));
    assert_eq!(shell.full_refresh_timers(), 1);

    shell.fire_timers();
    assert!(shell.view().asleep);
    assert_eq!(shell.full_refresh_timers(), 0);

    // The wake draws with a full refresh and starts the timer again.
    shell.send(Event::Woke);
    assert!(shell.frames.last().unwrap().full);
    assert_eq!(shell.full_refresh_timers(), 1);
}

#[test]
fn many_state_messages_give_a_full_refresh_after_the_partial_renders() {
    let mut shell = Shell::new();
    shell.send(start(PowerSource::Stand));
    let first = shell.frames.len();

    // Each message for a visible tile draws once, with no extra draw for the
    // time.
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
        for _ in 0..minutes {
            shell.fire_timers();
        }
        shell
            .broker
            .set_state("sensor.living_room_thermometer_humidity", humidity, &[]);
        shell.run();
    }
    assert_eq!(shell.frames.len(), frames);
    let log = shell.take_log();
    assert!(!log.contains(&"Render".into()), "{log:?}");
    // Each change gets a change time, so the core reads the clock once in
    // each new minute. The answers do not draw.
    let reads = log.iter().filter(|line| *line == "TimeNow").count();
    assert_eq!(reads, 2, "{log:?}");
    assert_eq!(shell.view().updated, "");
}
