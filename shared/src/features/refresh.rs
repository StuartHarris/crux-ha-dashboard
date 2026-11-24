//! The refresh feature: the decision to ask for a full refresh of the panel.
//!
//! An e-paper panel has two kinds of refresh. A full refresh drives all the
//! grey levels and clears ghost images, but the panel flashes. A partial
//! refresh is fast and does not flash, but it leaves a small ghost image.
//! The ghost images become worse with each partial refresh and with time.
//!
//! A full refresh has a [`Depth`]. A light full refresh flashes once. It is
//! clean for one page change, but faint ghost images collect over many
//! light full refreshes. A deep full refresh flashes more than once and
//! removes all ghost images.
//!
//! The core asks for a full refresh when it sets a new
//! [`ViewModel::full_refresh`](crate::ViewModel::full_refresh). The other
//! features ask with [`redraw`] and a [`Draw`]. The core asks in these
//! cases:
//!
//! - A light full refresh when a person looks at a new screen: a page
//!   change and a wake.
//! - A deep full refresh at the start and before sleep. After a boot or a
//!   deep sleep, the shell does not know the image on the panel. The image
//!   before sleep stays on the panel for the longest time, and nobody looks
//!   at the flashes.
//! - A deep full refresh when Home Assistant asks for a full refresh.
//! - A deep full refresh after [`RENDERS_PER_DEEP`] renders since the last
//!   deep full refresh.
//! - A deep full refresh [`FULL_REFRESH_INTERVAL`] after the last deep full
//!   refresh or the wake, while the device is awake.
//!
//! # The count of renders
//!
//! The core counts the renders since the last deep full refresh: the
//! partial renders and the light full refreshes. When the count is
//! [`RENDERS_PER_DEEP`], the next render is a deep full refresh, also if
//! the caller asked for a partial render or a light full refresh. A deep
//! full refresh starts the count again. A light full refresh does not.
//! Thus a device that only changes pages also gets a deep full refresh.
//!
//! The core asks the shell to draw only when the view model changed since
//! the last render that it asked for. A state message that repeats a value
//! therefore draws nothing and does not count. The core counts its own
//! renders, not the refreshes of the panel. A shell can draw many requests
//! as one refresh, so the count is the largest number of refreshes between
//! two deep full refreshes.
//!
//! A shell that draws many requests as one refresh sees only the last full
//! refresh. If the core asks for a deep full refresh and then a light full
//! refresh before the shell draws, the shell does a light full refresh, but
//! the count already started again. Two full refreshes before one draw are
//! rare. The next deep full refresh then comes after the interval or the
//! count.
//!
//! The core does not look at pixels. The partial waveform of the panel
//! cannot draw grey, so a shell also does a light full refresh when the
//! area that changed has grey pixels. Refer to `ui::Refresher`. The core
//! counted that render, so the count is correct.
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
//! device sleeps, the core starts no draw timer, and a draw timer draws
//! nothing.
//!
//! # The hourly report
//!
//! During the hourly report on battery, the core draws nothing, also when
//! the retained messages change the entities. After the radio turns off,
//! the core compares the view with the view of the image on the panel, with
//! the time left out ([`image_changed`]). If they differ, it draws once with
//! a partial refresh (a deep full refresh after [`RENDERS_PER_DEEP`]
//! renders). A change of only the time does not draw, so the panel keeps
//! the time of the last change that it drew. Refer to
//! [`power::radio_off`].
//!
//! # The full-refresh timer and sleep
//!
//! The full-refresh timer asks for a deep full refresh. Each deep full
//! refresh starts the timer again and clears the timer before it. A light
//! full refresh starts the timer only if no timer runs, so page changes do
//! not move the next deep full refresh later. If no render occurred since
//! the last deep full refresh when the timer expires, the panel has no new
//! ghost image. The core then does not ask for a full refresh. It starts
//! the timer again, so that the timer always runs while the device is
//! awake. That costs one timer each [`FULL_REFRESH_INTERVAL`], and the rule
//! stays simple.
//!
//! While the device sleeps, no timer runs. The last image before sleep is a
//! deep full refresh, and it clears the timer but does not start a new one.
//! The panel keeps that image with no power, so the image does not need
//! another refresh. The wake draws with a light full refresh, which starts
//! the timer again. A deep sleep loses the core and its timers, and the
//! next `Start` draws with a deep full refresh.
//!
//! # No draw before a restart
//!
//! While the chip waits to restart, the core draws nothing. A refresh that
//! starts just before the reset stops with the panel half drawn, and the
//! panel keeps that image until the full refresh at `Start`. Thus
//! [`redraw`] does nothing while a restart waits for the close of the
//! connection
//! ([`restart_waits`](crate::features::connection::State::restart_waits)),
//! and after the core asked the shell to restart the chip
//! ([`wifi::Phase::Restarting`]).

use alloc::string::String;
use core::time::Duration;

use crate::{
    Cmd, Depth, FullRefresh, Model, ViewModel,
    features::{power, timer::Slot, wifi},
};

/// The internal events of the refresh policy. The core makes them from its
/// timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The full-refresh timer with this id expired.
    FullRefreshDue(u32),
    /// The draw timer with this id expired. Refer to [`redraw_soon`].
    DrawDue(u32),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Refresh(event)
    }
}

/// Handles an internal event of the refresh policy.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::FullRefreshDue(id) => timer_expired(id, model),
        Event::DrawDue(id) => draw_due(id, model),
    }
}
use crux_core::render::render;

/// The number of renders since the last deep full refresh after which the
/// core asks for a deep full refresh. The render after this number is a
/// deep full refresh.
///
/// The count includes the partial renders and the light full refreshes.
/// Refer to the module documentation.
pub const RENDERS_PER_DEEP: u32 = 30;

/// The time after the last deep full refresh at which the core asks for
/// another deep full refresh, while the device is awake.
///
/// After a sleep, the time starts at the wake. The core asks only if a
/// render occurred since the last deep full refresh.
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
    /// The number of renders since the last deep full refresh: the partial
    /// renders and the light full refreshes.
    pub since_deep: u32,
    /// The last id that the core took for a full-refresh timer. Each new
    /// timer takes the next id. The `Slot` `full` decides if an expiry is
    /// stale.
    pub timer: u32,
    /// The full-refresh timer that runs. It asks for a deep full refresh.
    full: Slot,
    /// The view of the last render that the core asked for.
    last: Option<ViewModel>,
    /// The last id that the core took for a draw timer. Each new draw timer
    /// takes the next id. The `Slot` `draw_timer` decides if an expiry is
    /// stale.
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

/// What a caller of [`redraw`] asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Draw {
    /// Draw the changes. The shell uses a partial refresh where it can.
    Partial,
    /// Draw the whole screen with a full refresh of this depth.
    Full(Depth),
}

/// Asks the shell to draw, as `draw` tells.
///
/// A partial render occurs only if the view model changed since the last
/// render that the core asked for. Otherwise the core asks for nothing, and
/// the render does not count. A full refresh always renders. A later
/// partial render does not cancel it.
///
/// After [`RENDERS_PER_DEEP`] renders since the last deep full refresh, the
/// render is a deep full refresh, also if `draw` is
/// [`Draw::Partial`] or a light [`Draw::Full`].
///
/// A full refresh clears the draw timer that waits. If a sleep waits for
/// the pending marks and no mark is open, the full refresh then starts the
/// sleep. Refer to [`power::sleep_if_settled`]. Thus a caller that cancels
/// that sleep must do so before the full refresh.
///
/// While the chip waits to restart, or while a report runs, the core asks
/// for nothing, and the counts and timers do not change. Refer to the
/// [module docs](self).
pub fn redraw(model: &mut Model, draw: Draw) -> Cmd {
    if chip_restarts(model) || model.power.is_reporting() {
        return Cmd::done();
    }
    let mut next = ViewModel::from(&*model);
    let state = &mut model.refresh;
    let due = state.since_deep >= RENDERS_PER_DEEP;
    let depth = match draw {
        Draw::Partial if state.last.as_ref() == Some(&next) => return Cmd::done(),
        Draw::Partial if !due => {
            state.since_deep += 1;
            state.last = Some(next);
            return render();
        }
        Draw::Full(Depth::Light) if !due => Depth::Light,
        Draw::Partial | Draw::Full(_) => Depth::Deep,
    };
    // The full refresh draws the newest view, so the draw timer that waits
    // is stale.
    state.draw_timer.stop();
    let timer = match depth {
        Depth::Light => {
            state.since_deep += 1;
            // The timer measures the time from the last deep full refresh.
            // After a sleep, no timer runs, so the wake starts it.
            if state.full.is_running() {
                Cmd::done()
            } else {
                restart_timer(model)
            }
        }
        Depth::Deep => {
            state.since_deep = 0;
            restart_timer(model)
        }
    };
    model.full_refresh = model.full_refresh.next(depth);
    // The view is the same, except for the new full refresh.
    next.full_refresh = model.full_refresh;
    model.refresh.last = Some(next);
    // The draw timer can no longer start a sleep that waits for the marks,
    // so the full refresh starts it.
    render().and(timer).and(power::sleep_if_settled(model))
}

/// Returns `true` if the view, with the time left out, is not the view of
/// the image on the panel. The core then draws at the end of a report.
/// Refer to [`power::radio_off`].
///
/// The view of the image is the view of the last render that the core
/// asked for. The compare leaves out the footer time (`updated` and
/// `last_sync`) and `full_refresh.seq`. Home Assistant moves the time of an
/// entity also when only an attribute that the page does not show changes,
/// or when a value changes but shows the same text. Such a change does not
/// draw, and the panel keeps the time of the last change that it drew. With
/// no render before, the function returns `true`.
#[must_use]
pub fn image_changed(model: &Model) -> bool {
    let Some(last) = &model.refresh.last else {
        return true;
    };
    let now = ViewModel::from(model);
    without_time(now) != without_time(last.clone())
}

/// Returns `view` with no footer time and with [`FullRefresh::NONE`].
fn without_time(view: ViewModel) -> ViewModel {
    ViewModel {
        updated: String::new(),
        last_sync: None,
        full_refresh: FullRefresh::NONE,
        ..view
    }
}

/// Returns `true` if a restart waits for the close of the connection, or if
/// the core asked the shell to restart the chip.
fn chip_restarts(model: &Model) -> bool {
    model.connection.restart_waits() || model.wifi.phase == wifi::Phase::Restarting
}

/// Draws after [`DRAW_DELAY`], for a change from the state stream that the
/// page shows. If a draw timer already waits, the core does not start
/// another one. That timer draws this change too.
///
/// While the device is asleep, the core starts no draw timer, because no
/// timer can wait at a `Sleep` request. After a report, the steps of the
/// sleep draw the changes ([`image_changed`]).
pub fn redraw_soon(model: &mut Model) -> Cmd {
    let state = &mut model.refresh;
    if state.draw_timer.is_running() || model.power.is_asleep() {
        return Cmd::done();
    }
    state.draw = state.draw.wrapping_add(1);
    let id = state.draw;
    state
        .draw_timer
        .start(id, DRAW_DELAY, Event::DrawDue(id).into())
}

/// The draw timer `id` expired, so the core draws the changes that arrived
/// since the timer started. The core ignores an older timer. While the
/// device sleeps, the core draws nothing.
///
/// The draw is a partial render through [`redraw`]. If the view is the same
/// as the last view, the core asks for nothing. An example is a change of
/// `last_updated` in the same minute as the footer time.
///
/// If a sleep waits for the pending marks and no mark is open, the sleep
/// then starts. Refer to [`power::sleep_if_settled`].
pub fn draw_due(id: u32, model: &mut Model) -> Cmd {
    if !model.refresh.draw_timer.expired(id) {
        return Cmd::done();
    }
    if model.power.is_asleep() {
        return Cmd::done();
    }
    redraw(model, Draw::Partial).and(power::sleep_if_settled(model))
}

/// The full-refresh timer `id` expired. The core ignores an older timer,
/// and it ignores the timer while the device sleeps.
///
/// If a render occurred since the last deep full refresh, the core asks
/// for a deep full refresh. Otherwise, the panel has no new ghost image, so
/// the core only starts the timer again.
pub fn timer_expired(id: u32, model: &mut Model) -> Cmd {
    if !model.refresh.full.expired(id) || model.power.is_asleep() {
        return Cmd::done();
    }
    if model.refresh.since_deep == 0 {
        return restart_timer(model);
    }
    redraw(model, Draw::Full(Depth::Deep))
}

/// Clears the timer that runs. If the device is awake, starts a new timer
/// of [`FULL_REFRESH_INTERVAL`].
fn restart_timer(model: &mut Model) -> Cmd {
    let state = &mut model.refresh;
    state.timer = state.timer.wrapping_add(1);
    state.full.stop();
    if model.power.is_asleep() {
        return Cmd::done();
    }
    let id = state.timer;
    state
        .full
        .start(id, FULL_REFRESH_INTERVAL, Event::FullRefreshDue(id).into())
}

#[cfg(test)]
mod tests {
    //! Tests of the refresh feature: the depth of each full refresh, and the
    //! deep full refresh after a number of renders and after a time.

    use crate::features::refresh;
    use core::time::Duration;

    use crux_core::App;

    use crate::tests::{ExpectFullRefreshTimer, drive_to_connected, shell::Shell, start};
    use crate::{
        Dashboard, Depth, EffectTestExt, Event, Model, PowerSource,
        capabilities::power::Wake,
        features::power::IDLE_TIMEOUT,
        features::refresh::{
            DRAW_DELAY, Draw, FULL_REFRESH_INTERVAL, RENDERS_PER_DEEP, redraw, redraw_soon,
        },
    };

    /// Changes the view model: the status text shows `count` taps not sent.
    const fn change_the_view(model: &mut Model, count: usize) {
        model.unsent = count;
    }

    #[test]
    fn the_render_after_the_last_counted_render_is_a_deep_full_refresh() {
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        // The start was a deep full refresh, and `Connected` drew once.
        let seq = model.full_refresh.seq;
        let since_deep = model.refresh.since_deep;

        for count in since_deep..RENDERS_PER_DEEP {
            change_the_view(&mut model, count as usize + 1);
            redraw(&mut model, Draw::Partial).expect_only_render();
        }
        assert_eq!(model.full_refresh.seq, seq);

        change_the_view(&mut model, 100);
        redraw(&mut model, Draw::Partial)
            .expect_render()
            .expect_full_refresh_timer();
        assert_eq!(model.full_refresh.seq, seq + 1);
        assert_eq!(model.full_refresh.depth, Depth::Deep);
        assert_eq!(model.refresh.since_deep, 0);

        // The count starts again.
        change_the_view(&mut model, 101);
        redraw(&mut model, Draw::Partial).expect_only_render();
        assert_eq!(model.full_refresh.seq, seq + 1);
        assert_eq!(model.refresh.since_deep, 1);
    }

    #[test]
    fn a_render_with_no_change_to_the_view_does_not_draw_or_count() {
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        let since_deep = model.refresh.since_deep;
        redraw(&mut model, Draw::Partial).expect_no_effects();
        assert_eq!(model.refresh.since_deep, since_deep);

        // A full refresh always draws.
        redraw(&mut model, Draw::Full(Depth::Light)).expect_only_render();
        redraw(&mut model, Draw::Full(Depth::Deep))
            .expect_render()
            .expect_full_refresh_timer();
    }

    #[test]
    fn an_unchanged_view_at_the_limit_does_not_draw() {
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        model.refresh.since_deep = RENDERS_PER_DEEP;
        let seq = model.full_refresh.seq;
        redraw(&mut model, Draw::Partial).expect_no_effects();
        assert_eq!(model.full_refresh.seq, seq);
    }

    #[test]
    fn a_repeated_state_message_does_not_draw_or_count() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        shell.broker.set_state("light.all_lights", "on", &[]);
        shell.run();
        let frames = shell.frames.len();
        let since_deep = shell.model.refresh.since_deep;

        // Home Assistant publishes the same state again.
        shell.take_log();
        shell.broker.set_state("light.all_lights", "on", &[]);
        shell.run();
        assert_eq!(shell.frames.len(), frames);
        assert_eq!(shell.model.refresh.since_deep, since_deep);
        assert!(!shell.take_log().contains(&"Render".into()));
    }

    #[test]
    fn a_light_full_refresh_counts_towards_the_deep_full_refresh() {
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        let since_deep = model.refresh.since_deep;
        let seq = model.full_refresh.seq;

        let _ = Dashboard.update(Event::NextPage, &mut model);
        assert_eq!(model.full_refresh.seq, seq + 1);
        assert_eq!(model.full_refresh.depth, Depth::Light);
        assert_eq!(model.refresh.since_deep, since_deep + 1);
    }

    #[test]
    fn a_deep_full_refresh_starts_the_count_again() {
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        for count in 0..10 {
            change_the_view(&mut model, count + 1);
            let _ = redraw(&mut model, Draw::Partial);
        }
        let _ = Dashboard.update(Event::NextPage, &mut model);
        assert!(model.refresh.since_deep >= 11);

        let _ = redraw(&mut model, Draw::Full(Depth::Deep));
        assert_eq!(model.full_refresh.depth, Depth::Deep);
        assert_eq!(model.refresh.since_deep, 0);
    }

    #[test]
    fn page_turns_alone_give_a_deep_full_refresh_after_the_limit() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        let first = shell.frames.len();
        let since_deep = shell.model.refresh.since_deep;

        let turns = RENDERS_PER_DEEP - since_deep + 1;
        for _ in 0..turns {
            shell.send(Event::NextPage);
        }
        let depths: alloc::vec::Vec<_> = shell.frames[first..]
            .iter()
            .map(|frame| frame.full)
            .collect();
        assert_eq!(depths.len(), turns as usize);
        let (last, lights) = depths.split_last().unwrap();
        assert!(
            lights.iter().all(|depth| *depth == Some(Depth::Light)),
            "{depths:?}"
        );
        assert_eq!(*last, Some(Depth::Deep));
        assert_eq!(shell.model.refresh.since_deep, 0);
    }

    #[test]
    fn the_start_is_a_deep_full_refresh() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        assert_eq!(shell.frames[0].full, Some(Depth::Deep));
    }

    #[test]
    fn the_timer_asks_for_a_deep_full_refresh_while_the_device_is_awake() {
        let app = Dashboard;
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        // `Connected` drew once, so a render occurred.
        assert!(model.refresh.since_deep > 0);
        let seq = model.full_refresh.seq;
        let id = model.refresh.timer;

        app.update(
            Event::Refresh(refresh::Event::FullRefreshDue(id)),
            &mut model,
        )
        .expect_render()
        .expect_full_refresh_timer();
        assert_eq!(model.full_refresh.seq, seq + 1);
        assert_eq!(model.full_refresh.depth, Depth::Deep);
        assert_eq!(model.refresh.since_deep, 0);

        // The old timer is stale.
        app.update(
            Event::Refresh(refresh::Event::FullRefreshDue(id)),
            &mut model,
        )
        .expect_no_effects();
    }

    #[test]
    fn the_timer_asks_for_a_deep_full_refresh_after_only_page_turns() {
        let app = Dashboard;
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        let _ = redraw(&mut model, Draw::Full(Depth::Deep));
        let id = model.refresh.timer;

        // A page turn is a light full refresh. It keeps the timer that runs.
        let _ = app.update(Event::NextPage, &mut model);
        assert_eq!(model.refresh.timer, id);
        let seq = model.full_refresh.seq;

        app.update(
            Event::Refresh(refresh::Event::FullRefreshDue(id)),
            &mut model,
        )
        .expect_render()
        .expect_full_refresh_timer();
        assert_eq!(model.full_refresh.seq, seq + 1);
        assert_eq!(model.full_refresh.depth, Depth::Deep);
    }

    #[test]
    fn the_timer_with_no_render_since_the_deep_full_refresh_only_starts_again() {
        let app = Dashboard;
        let (mut model, _wire) = drive_to_connected(PowerSource::Stand);
        let _ = redraw(&mut model, Draw::Full(Depth::Deep));
        assert_eq!(model.refresh.since_deep, 0);
        let seq = model.full_refresh.seq;
        let id = model.refresh.timer;

        app.update(
            Event::Refresh(refresh::Event::FullRefreshDue(id)),
            &mut model,
        )
        .expect_only_time_notify_after_with(|notify| {
            assert_eq!(Duration::from(notify.duration), FULL_REFRESH_INTERVAL);
        });
        assert_eq!(model.full_refresh.seq, seq);
        assert_ne!(model.refresh.timer, id);
    }

    #[test]
    fn the_timer_does_nothing_while_the_device_sleeps() {
        let app = Dashboard;
        let (mut model, _wire) = drive_to_connected(PowerSource::Battery);
        let id = model.refresh.timer;
        let _ = app.update(Event::SleepNow, &mut model);
        assert!(model.power.is_asleep());

        app.update(
            Event::Refresh(refresh::Event::FullRefreshDue(id)),
            &mut model,
        )
        .expect_no_effects();
    }

    #[test]
    fn on_the_stand_the_panel_has_a_deep_full_refresh_each_interval() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        // A state message draws with a partial refresh.
        shell.broker.set_state("light.all_lights", "on", &[]);
        shell.run();
        assert_eq!(shell.frames.last().unwrap().full, None);
        let frames = shell.frames.len();

        assert_eq!(full_refreshes_due(&mut shell), 1);
        assert_eq!(shell.frames.len(), frames + 1);
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Deep));

        // The deep full refresh started the next timer. With no render
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
            .filter(|event| matches!(event, Event::Refresh(refresh::Event::FullRefreshDue(_))))
            .count()
    }

    #[test]
    fn each_deep_full_refresh_starts_the_timer_again() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        shell.take_log();

        // The page turn comes at the limit, so it is a deep full refresh.
        shell.model.refresh.since_deep = RENDERS_PER_DEEP;
        shell.send(Event::NextPage);
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Deep));
        let log = shell.take_log();
        assert!(log.contains(&"TimeClear".into()), "{log:?}");
        // Only the new timer runs.
        assert_eq!(full_refreshes_due(&mut shell), 1);
    }

    #[test]
    fn a_light_full_refresh_keeps_the_timer_that_runs() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        shell.advance(FULL_REFRESH_INTERVAL / 2);
        shell.take_log();

        shell.send(Event::NextPage);
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Light));
        let log = shell.take_log();
        assert!(!log.contains(&"TimeClear".into()), "{log:?}");

        // The timer of the start expires at its time, and asks for a deep
        // full refresh.
        let events = shell.advance(FULL_REFRESH_INTERVAL / 2);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Refresh(refresh::Event::FullRefreshDue(_)))),
            "{events:?}"
        );
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Deep));
    }

    #[test]
    fn no_full_refresh_timer_runs_while_the_device_sleeps() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Battery));
        shell.advance(IDLE_TIMEOUT);
        assert!(shell.view().asleep);
        // The last image before sleep is a deep full refresh.
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Deep));
        assert_eq!(full_refreshes_due(&mut shell), 0);

        // The wake draws with a light full refresh and starts the timer
        // again.
        shell.take_log();
        shell.wake(Wake::Touch(None));
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Light));
        let timer = alloc::format!("TimeNotifyAfter {}ms", FULL_REFRESH_INTERVAL.as_millis());
        assert!(shell.take_log().contains(&timer));
    }

    #[test]
    fn many_state_messages_give_a_deep_full_refresh_after_the_limit() {
        let mut shell = Shell::new();
        shell.send(start(PowerSource::Stand));
        let first = shell.frames.len();

        // Each change of a visible tile draws once, with no extra draw for
        // `last_updated`.
        for index in 0..=RENDERS_PER_DEEP {
            let state = if index % 2 == 0 { "on" } else { "off" };
            shell.broker.set_state("light.all_lights", state, &[]);
            shell.run();
        }
        let full: alloc::vec::Vec<_> = shell.frames[first..]
            .iter()
            .filter_map(|frame| frame.full)
            .collect();
        assert_eq!(full, [Depth::Deep]);
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
        assert!(
            matches!(events[..], [Event::Refresh(refresh::Event::DrawDue(_))]),
            "{events:?}"
        );
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
        let _ = app.update(Event::SleepNow, &mut model);
        assert!(model.power.is_asleep());

        app.update(Event::Refresh(refresh::Event::DrawDue(id)), &mut model)
            .expect_no_effects();

        // The timer no longer waits, so the next change after the wake starts a
        // new timer.
        // A wake through an event draws, and the draw clears the draw timer.
        // Thus the test wakes the device with no draw.
        model.power.wake_with_no_draw();
        assert!(!model.power.is_asleep());
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

        // The idle timeout puts the device to sleep. The last image is a deep
        // full refresh with the change.
        shell.advance(IDLE_TIMEOUT.saturating_sub(before));
        assert!(shell.view().asleep);
        let frames = shell.frames.len();
        assert_eq!(shell.frames.last().unwrap().full, Some(Depth::Deep));

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

        app.update(Event::Refresh(refresh::Event::DrawDue(first)), &mut model)
            .expect_no_effects();
        app.update(Event::Refresh(refresh::Event::DrawDue(second)), &mut model)
            .expect_only_render();
        // A timer draws only once.
        app.update(Event::Refresh(refresh::Event::DrawDue(second)), &mut model)
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
        // Only the draw timer. A light full refresh keeps the full-refresh
        // timer that runs.
        assert_eq!(clears, 1, "{log:?}");

        let events = shell.advance(DRAW_DELAY);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Refresh(refresh::Event::DrawDue(_)))),
            "{events:?}"
        );
    }
}
