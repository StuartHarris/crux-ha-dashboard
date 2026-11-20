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
