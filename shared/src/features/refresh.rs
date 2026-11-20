//! The refresh feature: the decision to ask for a full refresh of the panel.
//!
//! An e-paper panel has two kinds of refresh. A full refresh drives all the
//! grey levels and clears ghost images, but the panel flashes. A partial
//! refresh is fast and does not flash, but it leaves a small ghost image.
//! The ghost images become worse with each partial refresh and with time.
//!
//! The core asks for a full refresh when it increases
//! [`ViewModel::full_refresh_seq`](crate::ViewModel::full_refresh_seq). It
//! asks in these cases:
//!
//! - The whole screen changes: at the start, at a wake, on a page change
//!   and before sleep. The other features ask with [`redraw`].
//! - After [`PARTIALS_PER_FULL`] partial renders since the last full
//!   refresh.
//! - [`FULL_REFRESH_INTERVAL`] after the last full refresh, while the device
//!   is awake.
//!
//! The core asks the shell to draw only when the view model changed since
//! the last render that it asked for. A state message that repeats a value
//! therefore draws nothing and does not count. The core counts its own
//! partial renders, not the refreshes of the panel. A shell can draw many
//! requests as one refresh, so the count is the largest number of partial
//! refreshes between two full refreshes.
//!
//! The core does not look at pixels. The partial waveform of the panel
//! cannot draw grey, so a shell also does a full refresh when the area that
//! changed has grey pixels. Refer to `ui::Refresher`. The core does not know
//! about such a full refresh, so it does not start its count again. The
//! next full refresh of the core then comes earlier than necessary. The
//! panel shows black and white only, so this is rare.
//!
//! # The draw timer
//!
//! A change from the state stream does not draw at once. Home Assistant
//! sends the state, then `last_updated`, then the attributes, a few
//! milliseconds apart. When a dashboard connects, the broker and the seed
//! automation send a burst of messages. The order of these messages is not
//! certain. If each message drew at once, one change gave two or more
//! partial refreshes. Each partial refresh leaves a ghost image and counts
//! towards the next full refresh.
//!
//! Thus a change that the page shows starts the draw timer of
//! [`DRAW_DELAY`] with [`redraw_soon`], if no draw timer waits. When the
//! timer expires, [`draw_due`] draws once for all the changes. A change that
//! the page does not show starts no timer.
//!
//! Each full refresh makes the draw timer that waits stale, because the full
//! refresh already drew the newest view. Examples are a page turn, a wake
//! and the image before sleep. The core clears a stale draw timer. If the
//! expiry of a stale timer still arrives, the core ignores it. While the
//! device sleeps, the draw timer draws nothing.
//!
//! # The full-refresh timer and sleep
//!
//! Each full refresh starts the timer again and clears the timer before it.
//! Ghost images come only from partial refreshes. If no partial render
//! occurred since the last full refresh when the timer expires, the core
//! does not ask for a full refresh. It starts the timer again, so that the
//! timer always runs while the device is awake. That costs one timer each
//! [`FULL_REFRESH_INTERVAL`], and the rule stays simple.
//!
//! While the device sleeps, no timer runs. The last image before sleep is a
//! full refresh, and it clears the timer but does not start a new one. The
//! panel keeps that image with no power, so the image does not need another
//! refresh. The wake draws with a full refresh, which starts the timer
//! again. A deep sleep loses the core and its timers, and the next `Start`
//! draws with a full refresh.

use core::time::Duration;

use crate::{Cmd, Event, Model, ViewModel, features::timer::Slot, view::view};
use crux_core::render::render;

/// The number of partial renders after which the core asks for a full
/// refresh. The render after this number is a full refresh.
///
/// The count does not include a full refresh that a shell does for grey
/// pixels, because the core does not know about it. Refer to the module
/// documentation.
pub const PARTIALS_PER_FULL: u32 = 30;

/// The time after the last full refresh at which the core asks for another
/// full refresh, while the device is awake. The core asks only if a partial
/// render occurred since the last full refresh.
pub const FULL_REFRESH_INTERVAL: Duration = Duration::from_mins(30);

/// The wait between a change that the page shows and its draw.
///
/// The changes that arrive in this time give one draw. Home Assistant sends
/// the messages of one change a few milliseconds apart, so 200 ms is enough
/// for them. A person does not see this short delay. Refer to the module
/// documentation.
pub const DRAW_DELAY: Duration = Duration::from_millis(200);

/// The state of the refresh policy.
#[derive(Debug, Default)]
pub struct State {
    /// The number of partial renders since the last full refresh.
    pub partials: u32,
    /// The id of the newest full-refresh timer.
    pub timer: u32,
    /// The full-refresh timer that runs.
    full: Slot,
    /// The view of the last render that the core asked for.
    last: Option<ViewModel>,
    /// The id of the newest draw timer.
    pub draw: u32,
    /// The draw timer that waits. While it waits, a new change does not
    /// start another one.
    draw_timer: Slot,
}

impl State {
    /// Returns `true` if a draw timer waits.
    #[must_use]
    pub const fn draw_waits(&self) -> bool {
        self.draw_timer.is_running()
    }
}

/// Asks the shell to draw. If `full` is `true`, the core also asks for a
/// full refresh of the panel. A later partial render does not cancel it.
///
/// A partial render occurs only if the view model changed since the last
/// render that the core asked for. Otherwise the core asks for nothing, and
/// the render does not count. A partial render is a full refresh if
/// [`PARTIALS_PER_FULL`] partial renders came before it. A full refresh
/// always renders.
pub fn redraw(model: &mut Model, full: bool) -> Cmd {
    let mut next = view(model);
    let state = &mut model.refresh;
    if !full && state.last.as_ref() == Some(&next) {
        return Cmd::done();
    }
    if !full && state.partials < PARTIALS_PER_FULL {
        state.partials += 1;
        state.last = Some(next);
        return render();
    }
    state.partials = 0;
    // The full refresh draws the newest view, so the draw timer that waits
    // is stale.
    if state.draw_timer.is_running() {
        state.draw = state.draw.wrapping_add(1);
        state.draw_timer.stop();
    }
    model.full_refresh_seq = model.full_refresh_seq.wrapping_add(1);
    // The view is the same, except for the new number.
    next.full_refresh_seq = model.full_refresh_seq;
    model.refresh.last = Some(next);
    render().and(restart_timer(model))
}

/// Draws after [`DRAW_DELAY`], for a change from the state stream that the
/// page shows. If a draw timer already waits, the core does not start
/// another one. That timer draws this change too.
pub fn redraw_soon(model: &mut Model) -> Cmd {
    let state = &mut model.refresh;
    if state.draw_timer.is_running() {
        return Cmd::done();
    }
    state.draw = state.draw.wrapping_add(1);
    let id = state.draw;
    state.draw_timer.start(id, DRAW_DELAY, Event::DrawDue(id))
}

/// The draw timer `id` expired, so the core draws the changes that arrived
/// since the timer started. The core ignores an older timer. While the
/// device sleeps, the core draws nothing.
///
/// The draw is a partial render through [`redraw`]. If the view is the same
/// as the last view, the core asks for nothing. An example is a change of
/// `last_updated` in the same minute as the footer time.
pub fn draw_due(id: u32, model: &mut Model) -> Cmd {
    if !model.refresh.draw_timer.expired(id) {
        return Cmd::done();
    }
    if model.power.asleep {
        return Cmd::done();
    }
    redraw(model, false)
}

/// The full-refresh timer `id` expired. The core ignores an older timer,
/// and it ignores the timer while the device sleeps.
///
/// If a partial render occurred since the last full refresh, the core asks
/// for a full refresh. Otherwise, the panel has no new ghost image, so the
/// core only starts the timer again.
pub fn timer_expired(id: u32, model: &mut Model) -> Cmd {
    if !model.refresh.full.expired(id) || model.power.asleep {
        return Cmd::done();
    }
    if model.refresh.partials == 0 {
        return restart_timer(model);
    }
    redraw(model, true)
}

/// Clears the timer that runs. If the device is awake, starts a new timer
/// of [`FULL_REFRESH_INTERVAL`].
fn restart_timer(model: &mut Model) -> Cmd {
    let state = &mut model.refresh;
    state.timer = state.timer.wrapping_add(1);
    state.full.stop();
    if model.power.asleep {
        return Cmd::done();
    }
    let id = state.timer;
    state
        .full
        .start(id, FULL_REFRESH_INTERVAL, Event::FullRefreshDue(id))
}

#[cfg(test)]
mod tests {
    //! Tests of the refresh feature: the full refresh after a number of partial
    //! renders and after a time.

    use core::time::Duration;

    use crux_core::App;

    use crate::tests::{ExpectFullRefreshTimer, drive_to_connected, shell::Shell, start};
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
        crate::tests::set_state(&mut model, "light.all_lights", "on");
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
        crate::tests::set_state(&mut model, "light.all_lights", "on");
        let _first = redraw_soon(&mut model);
        let first = model.refresh.draw;

        // A page turn draws the whole screen, so the first timer is stale. The
        // next change starts a newer timer.
        let _ = app.update(Event::NextPage, &mut model);
        crate::tests::set_state(&mut model, "light.living_room_lights", "on");
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
}
