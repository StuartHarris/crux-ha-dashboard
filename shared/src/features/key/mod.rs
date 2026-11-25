//! The key feature: the touch key on the front of the device.
//!
//! The key is part of the touch controller. The shell sends each edge of
//! the key as [`Event::Key`](crate::Event::Key) with a [`KeyEdge`]. The
//! core decides what a press means:
//!
//! - A short press shows the index page ([`Route::Home`]). On the index
//!   page, the press shows nothing new and does not draw.
//! - A long press asks for a deep full refresh, with the rules of the Full
//!   refresh button of Home Assistant ([`device::full_refresh`]). A press
//!   is long when the key stays down for [`LONG_PRESS`]. The refresh starts
//!   at that time, not when the key goes up.
//!
//! Each press is a touch, as a tap is. The `Down` edge turns the front
//! light on and starts the idle timer again. Refer to [`light::touched`]
//! and [`power::restart_idle_timer`].
//!
//! A press that overlaps a full refresh is never long. The firmware does
//! not read the key during a refresh of the panel, and a refresh can take
//! longer than [`LONG_PRESS`]. Thus the `Up` edge can arrive late, and the
//! core cannot measure the press. Two rules follow:
//!
//! - A press that wakes the device is short, and it acts at once. The core
//!   sets the route to the index page before the wake draws, so the wake
//!   draws once. The press starts no long-press timer. A press that wakes
//!   the device comes as `Down` while the device sleeps, or as the answer
//!   `Wake::Key` to a `Sleep` request.
//! - If a full refresh starts while the long-press timer runs, the core
//!   clears the timer ([`full_refresh_started`]). The `Up` edge then shows
//!   the index page, also if it arrives after [`LONG_PRESS`].
//!
//! A tap on the panel while the key is down cancels the press ([`tapped`]).
//! The tap acts, and the `Up` edge does nothing.
//!
//! A sleep ends the press that is in progress ([`sleep_started`]). Thus no
//! long-press timer waits at a `Sleep` request, and the `Up` edge after the
//! sleep does nothing.

use core::time::Duration;

use crate::{
    Cmd, KeyEdge, Model,
    capabilities::log::Area,
    features::{
        device::{self, Asker},
        light, logs, power, rooms,
        timer::Running,
    },
    route::Route,
};

/// The time that the key must stay down for a long press.
pub const LONG_PRESS: Duration = Duration::from_secs(1);

/// The internal events of the key feature. The core makes them from its
/// timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The long-press timer with this id expired.
    LongPress(u32),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::KeyTimer(event)
    }
}

/// Handles an internal event of the key feature.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::LongPress(id) => long_press(id, model),
    }
}

/// Where the key is in a press.
///
/// | From | Input | To | Effects |
/// | --- | --- | --- | --- |
/// | `Idle` | `Down`, the device is awake | `Held { t }` | light on, idle timer again, start `t` |
/// | any | `Down`, the device sleeps | `Acted` | clear `t` of `Held`, route to the index page, wake (one light full refresh), light on |
/// | `Held`, `ShortOnly`, `Acted`, `Cancelled` | `Down`, the device is awake | unchanged | nothing (a repeated `Down`) |
/// | `Idle` | `Up` | `Idle` | nothing (an `Up` with no `Down`) |
/// | `Held { t }` | `Up` | `Idle` | clear `t`, show the index page |
/// | `ShortOnly` | `Up` | `Idle` | show the index page |
/// | `Acted` | `Up` | `Idle` | nothing |
/// | `Held { t }` | `LongPress(t.id)` | `Acted` | a deep full refresh, with the rules of the Full refresh button |
/// | `Held { t }` | `LongPress(other id)` | `Held { t }` | nothing (stale) |
/// | `Idle`, `ShortOnly`, `Acted`, `Cancelled` | `LongPress(any)` | unchanged | nothing (stale) |
/// | `Held { t }` | a full refresh starts | `ShortOnly` | clear `t` |
/// | `Held { t }`, `ShortOnly` | a tap on the panel | `Cancelled` | clear `t` of `Held`, Debug line |
/// | `Cancelled` | `Up` | `Idle` | nothing |
/// | `Held { t }`, `ShortOnly` | a sleep starts | `Idle` | clear `t` of `Held`, Debug line |
/// | any | `Start` | `Idle` | clear `t` of `Held` |
///
/// A sleep starts only while the device is awake. In the steps of a sleep,
/// the press is thus `Idle`, `Acted` or `Cancelled`. A `Down` while the device sleeps
/// always starts a new press. If the shell lost an `Up` edge, the key
/// still wakes the device.
///
/// The long press moves to `Acted` before it asks for its full refresh, so
/// its own refresh does not make the press `ShortOnly`.
#[derive(Debug, Default)]
pub enum Press {
    /// The key is up.
    #[default]
    Idle,
    /// The key is down, and the long-press timer runs.
    Held {
        /// The long-press timer.
        timer: Running,
    },
    /// The key is down, and a full refresh started while the long-press
    /// timer ran. The press cannot be long. The `Up` edge shows the index
    /// page.
    ShortOnly,
    /// The key is down, and the press already acted: the long press asked
    /// for a full refresh, or the press woke the device. The `Up` edge does
    /// nothing.
    Acted,
    /// The key is down, and a tap on the panel cancelled the press. The
    /// tap acts, and the `Up` edge does nothing. A finger on the key while
    /// the other hand taps the panel is usually a grip on the device, not a
    /// press of the key.
    Cancelled,
}

/// The state of the key.
#[derive(Debug, Default)]
pub struct State {
    /// Where the key is in a press.
    press: Press,
    /// The last id that the core took for a long-press timer. Each new
    /// timer takes the next id.
    timer: u32,
}

impl State {
    /// Returns where the key is in a press.
    #[must_use]
    pub const fn press(&self) -> &Press {
        &self.press
    }
}

/// The shell saw an edge of the key. Refer to [`Press`].
pub fn edge(edge: KeyEdge, model: &mut Model) -> Cmd {
    match edge {
        KeyEdge::Down => down(model),
        KeyEdge::Up => up(model),
    }
}

/// The key went down.
///
/// If the device sleeps, the press wakes it and acts at once as a short
/// press ([`woke`]). While the device is awake, only a press from `Idle`
/// acts. It turns the front light on, starts the idle timer again, and
/// starts the long-press timer.
fn down(model: &mut Model) -> Cmd {
    if model.power.is_asleep() {
        return woke(model);
    }
    if !matches!(model.key.press, Press::Idle) {
        return Cmd::done();
    }
    model.key.timer = model.key.timer.wrapping_add(1);
    let id = model.key.timer;
    let (start, timer) = Running::start(id, LONG_PRESS, Event::LongPress(id).into());
    model.key.press = Press::Held { timer };
    // The light goes on first, then the idle timer starts again. One pump
    // publishes the state of the light.
    light::touched(model)
        .and(power::restart_idle_timer(model))
        .and(start)
        .and(device::changed(model))
}

/// The key woke the device. The press is short: the core sets the route to
/// the index page before the wake draws, so the wake draws once. The press
/// is `Acted`, so its `Up` edge does nothing.
///
/// The wake starts the idle timer again. Refer to [`power::wake_up`].
fn woke(model: &mut Model) -> Cmd {
    reset(model);
    model.key.press = Press::Acted;
    if model.pages.iter().any(|page| page.is_at(&Route::Home)) {
        model.route = Route::Home;
    }
    // A tap that wakes the device turns the light on after the wake, as
    // `rooms::tapped` does.
    power::wake_up(model)
        .and(light::touched(model))
        .and(device::changed(model))
}

/// The key went up. If the press was short, the core shows the index
/// page.
fn up(model: &mut Model) -> Cmd {
    match core::mem::take(&mut model.key.press) {
        Press::Held { timer } => {
            timer.clear();
            rooms::navigate(Route::Home, model)
        }
        Press::ShortOnly => rooms::navigate(Route::Home, model),
        Press::Idle | Press::Acted | Press::Cancelled => Cmd::done(),
    }
}

/// The long-press timer with the id `id` expired. If the key is still
/// down, the core asks for a full refresh.
fn long_press(id: u32, model: &mut Model) -> Cmd {
    match &model.key.press {
        Press::Held { timer } if timer.id() == id => {
            // `Acted` first, so the refresh does not make the press
            // `ShortOnly`.
            model.key.press = Press::Acted;
            device::full_refresh(Asker::Key, model)
        }
        Press::Idle | Press::Held { .. } | Press::ShortOnly | Press::Acted | Press::Cancelled => {
            Cmd::done()
        }
    }
}

/// A full refresh starts: the press can only be short.
///
/// If the long-press timer runs, the core clears it. The `Up` edge can
/// arrive late after the refresh, so the core cannot measure the press.
/// The refresh feature calls this function each time it asks for a full
/// refresh.
pub fn full_refresh_started(model: &mut Model) {
    if let Press::Held { .. } = model.key.press {
        reset(model);
        model.key.press = Press::ShortOnly;
    }
}

/// The panel got a tap: a press of the key that did not act ends.
///
/// The core clears the long-press timer, and the `Up` edge does nothing.
/// The rooms feature calls this function before the tap acts.
pub fn tapped(model: &mut Model) -> Cmd {
    if let Press::Held { .. } | Press::ShortOnly = model.key.press {
        reset(model);
        model.key.press = Press::Cancelled;
        return logs::debug(
            Area::Device,
            "a tap on the panel while the key is down cancels the press",
        );
    }
    Cmd::done()
}

/// A sleep starts: a press that did not act ends with no action.
///
/// If the long-press timer runs, the core clears it. The power feature
/// calls this function before the steps of the sleep.
pub fn sleep_started(model: &mut Model) -> Cmd {
    if let Press::Held { .. } | Press::ShortOnly = model.key.press {
        reset(model);
        return logs::debug(
            Area::Device,
            "the sleep starts while the key is down, so the press does nothing",
        );
    }
    Cmd::done()
}

/// Forgets the press, and clears the long-press timer if it runs. The core
/// calls this function at `Start`.
pub fn reset(model: &mut Model) {
    if let Press::Held { timer } = core::mem::take(&mut model.key.press) {
        timer.clear();
    }
}

#[cfg(test)]
mod tests;
