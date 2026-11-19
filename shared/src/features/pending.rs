//! The pending feature: the panel shows a tap at once, before Home Assistant
//! confirms it.
//!
//! A service call changes the panel only when the new state comes back on
//! the state stream. That takes about 0.5 s while the device is awake, and
//! about 3 s after a wake. Thus a tap on a light tile, a − or + step, a scene
//! or the All lights toggle gives the entity a mark. The view shows a tile
//! or a scene with a mark as [`TileState::Pending`](crate::TileState) or
//! [`SceneView::pending`](crate::SceneView). The view does not show a state
//! that Home Assistant did not confirm.
//!
//! # Confirm
//!
//! At the tap, the mark keeps the state of the entity and the exact time of
//! its last update (`last_updated`). A state-stream message for the entity
//! confirms the call as [`Confirm`] tells:
//!
//! - A toggle (a tap on a tile or on the All lights toggle): the state is
//!   different from the state at the tap. If the core had a time at the tap,
//!   the time of the last update must also be later than that time.
//! - A step: the time of the last update is later than the time at the tap.
//!   The state of a light that is on usually stays `on`, so only the time
//!   shows the change.
//! - A scene: no message. A scene has no state that confirms it.
//!
//! If the core had no state (for a toggle) or no time (for a step) at the
//! tap, no message confirms the call. An example is a tap after a start,
//! before the retained messages arrive. The first retained message can be
//! older than the call, so the core cannot use it. Only the timeout then
//! removes the mark.
//!
//! After a wake, the core sends the outbox before the subscribe, so the old
//! retained messages arrive after the service call. They have the state and
//! the time at the tap, so the mark stays.
//!
//! A call that changes nothing shows the mark until the timeout. An example
//! is All lights on when all the lights are on. The core sends no + step for
//! a light at 100%, so that step shows no mark.
//!
//! # Timers
//!
//! - [`PENDING_TIMEOUT`] after the core issued the service call, Home
//!   Assistant did not confirm it. The core removes the mark, and the tile
//!   shows the last state that the core has. If a newer call for the entity
//!   waits in the outbox, the mark stays for that call.
//! - [`SCENE_TIMEOUT`] after the core issued the call of a scene, the core
//!   removes its mark.
//! - [`QUEUED_TIMEOUT`] after the first call of a mark went into the
//!   outbox, the connection did not open. The core removes the mark, but the
//!   call stays in the outbox. If the connection opens later, the core sends
//!   it.
//!
//! The timeout of a call starts when the core issues the call: at once while
//! the connection is open, or when the outbox drains after the connect. Thus
//! the reconnect after a wake does not use the time of the timeout. Each
//! timer has an id, and the core ignores a timer that is not a timer of a
//! mark now.
//!
//! # Drop
//!
//! If the outbox drops a service call, because it is full or the device goes
//! to sleep, the core removes the mark. It keeps the mark if another call for
//! the entity waits in the outbox, or if a call for the entity went out.
//!
//! A mark stays through a sleep. A tap that wakes the device shows the mark
//! during the reconnect.

use alloc::{string::String, vec::Vec};
use core::time::Duration;

use chrono::{DateTime, Utc};
use crux_time::{TimerHandle, clock::Time};

use crate::{
    Cmd, Event, Model,
    features::refresh::{redraw, redraw_soon},
};

/// The time after which the core removes the mark of a service call that
/// Home Assistant did not confirm.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

/// The time after which the core removes the mark of a scene. A scene has no
/// state that confirms it.
pub const SCENE_TIMEOUT: Duration = Duration::from_secs(2);

/// The time after which the core removes the mark of a service call that
/// still waits in the outbox. The call stays in the outbox.
pub const QUEUED_TIMEOUT: Duration = Duration::from_secs(30);

/// How Home Assistant confirms a service call. Refer to the module
/// documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// A toggle: a new state, after the time at the tap.
    Toggle,
    /// A brightness step: a time of the last update after the time at the
    /// tap.
    Step,
    /// A scene: nothing confirms it.
    Scene,
}

/// A timer of a mark: its id and its handle.
#[derive(Debug)]
struct Timer {
    id: u32,
    handle: TimerHandle,
}

impl Timer {
    /// Clears the timer.
    fn clear(self) {
        self.handle.clear();
    }
}

/// The mark of one entity.
#[derive(Debug)]
pub struct Mark {
    /// The entity of the service call.
    pub entity_id: &'static str,
    /// How Home Assistant confirms the call.
    pub confirm: Confirm,
    /// The state of the entity at the tap, or `None` if the core had no
    /// state.
    pub state: Option<String>,
    /// The exact `last_updated` time of the entity at the tap, or `None` if
    /// the core had no time.
    pub since: Option<DateTime<Utc>>,
    /// The timeout of the call that the core issued last, or `None` if no
    /// timeout runs.
    timeout: Option<Timer>,
    /// The timeout of the calls that wait in the outbox, or `None` if no
    /// timeout runs.
    queued: Option<Timer>,
}

impl Mark {
    /// Returns `true` if the timer `id` is a timer of the mark.
    fn has_timer(&self, id: u32) -> bool {
        [&self.timeout, &self.queued]
            .into_iter()
            .flatten()
            .any(|timer| timer.id == id)
    }

    /// Clears the timers of the mark.
    fn clear_timers(&mut self) {
        for timer in [self.timeout.take(), self.queued.take()]
            .into_iter()
            .flatten()
        {
            timer.clear();
        }
    }

    /// Returns `true` if the entity, with `state` and the exact time
    /// `updated`, confirms the call. Refer to the module documentation.
    fn confirmed_by(&self, state: Option<&str>, updated: Option<DateTime<Utc>>) -> bool {
        let newer = match (self.since, updated) {
            (Some(since), Some(updated)) => updated > since,
            (Some(_), None) => false,
            (None, _) => true,
        };
        match self.confirm {
            Confirm::Toggle => {
                self.state.is_some() && state.is_some() && state != self.state.as_deref() && newer
            }
            Confirm::Step => self.since.is_some() && newer,
            Confirm::Scene => false,
        }
    }
}

/// The marks of the entities.
#[derive(Debug, Default)]
pub struct State {
    /// The marks, at most one for each entity.
    pub marks: Vec<Mark>,
    /// The id of the newest timer.
    pub timer: u32,
}

impl State {
    /// Returns the mark of `entity_id`.
    fn get_mut(&mut self, entity_id: &str) -> Option<&mut Mark> {
        self.marks
            .iter_mut()
            .find(|mark| mark.entity_id == entity_id)
    }

    /// Starts a timer of `wait`, and returns its command and the timer.
    fn start(&mut self, wait: Duration) -> (Cmd, Timer) {
        self.timer = self.timer.wrapping_add(1);
        let id = self.timer;
        let (timer, handle) = Time::notify_after(wait);
        let command = timer.then_send(move |_| Event::PendingTimeout(id));
        (command, Timer { id, handle })
    }
}

/// Returns `true` if the entity `entity_id` has a mark.
#[must_use]
pub fn is_pending(model: &Model, entity_id: &str) -> bool {
    model
        .pending
        .marks
        .iter()
        .any(|mark| mark.entity_id == entity_id)
}

/// Gives the entity `entity_id` a mark, before the core sends its service
/// call. The mark keeps the state and the time that the core has now.
///
/// If the entity already has a mark, the core keeps its timers, and the
/// new tap gives the mark its state, its time and `confirm`.
pub fn mark(model: &mut Model, entity_id: &'static str, confirm: Confirm) {
    let entity = model.entities.get(entity_id);
    let state = entity.and_then(|entity| entity.state.clone());
    let since = entity.and_then(|entity| entity.last_updated_exact);
    if let Some(mark) = model.pending.get_mut(entity_id) {
        mark.confirm = confirm;
        mark.state = state;
        mark.since = since;
        return;
    }
    model.pending.marks.push(Mark {
        entity_id,
        confirm,
        state,
        since,
        timeout: None,
        queued: None,
    });
}

/// A service call for `entity_id` went into the outbox. If no timeout of
/// the outbox runs for its mark, start [`QUEUED_TIMEOUT`].
pub fn queued(entity_id: &str, model: &mut Model) -> Cmd {
    let state = &mut model.pending;
    if state
        .get_mut(entity_id)
        .is_none_or(|mark| mark.queued.is_some())
    {
        return Cmd::done();
    }
    let (command, timer) = state.start(QUEUED_TIMEOUT);
    if let Some(mark) = state.get_mut(entity_id) {
        mark.queued = Some(timer);
    }
    command
}

/// The core issued the service call for `entity_id`: start the timeout of
/// its mark.
///
/// The new timeout replaces the timeout before it. If no other call for the
/// entity waits in the outbox, the timeout of the outbox stops.
pub fn issued(entity_id: &str, model: &mut Model) -> Cmd {
    let waits = model
        .connection
        .outbox
        .iter()
        .any(|call| call.entity_id == entity_id);
    let state = &mut model.pending;
    let Some(confirm) = state.get_mut(entity_id).map(|mark| mark.confirm) else {
        return Cmd::done();
    };
    let wait = if confirm == Confirm::Scene {
        SCENE_TIMEOUT
    } else {
        PENDING_TIMEOUT
    };
    let (command, timer) = state.start(wait);
    if let Some(mark) = state.get_mut(entity_id) {
        if let Some(old) = mark.timeout.replace(timer) {
            old.clear();
        }
        if !waits && let Some(queued) = mark.queued.take() {
            queued.clear();
        }
    }
    command
}

/// The outbox dropped one or more service calls. Removes each mark that has
/// no timeout of an issued call and no call in the outbox. Returns `true` if
/// the core removed a mark.
pub fn forget_dropped(model: &mut Model) -> bool {
    let outbox = &model.connection.outbox;
    let marks = &mut model.pending.marks;
    let before = marks.len();
    marks.retain_mut(|mark| {
        let keep =
            mark.timeout.is_some() || outbox.iter().any(|call| call.entity_id == mark.entity_id);
        if !keep {
            mark.clear_timers();
        }
        keep
    });
    marks.len() != before
}

/// Removes the mark of `entity_id` if Home Assistant confirmed its service
/// call, after a state-stream message for the entity.
///
/// Refer to the module documentation for the rules. The draw waits for the
/// other messages of the change, as for any change from the state stream.
/// Home Assistant sends `last_updated` before the attributes, so a draw at
/// once can show the old brightness.
pub fn confirm(entity_id: &str, model: &mut Model) -> Cmd {
    let entity = model.entities.get(entity_id);
    let state = entity.and_then(|entity| entity.state.as_deref());
    let updated = entity.and_then(|entity| entity.last_updated_exact);
    let marks = &mut model.pending.marks;
    let Some(index) = marks
        .iter()
        .position(|mark| mark.entity_id == entity_id && mark.confirmed_by(state, updated))
    else {
        return Cmd::done();
    };
    marks.swap_remove(index).clear_timers();
    redraw_soon(model)
}

/// The timer `id` of a mark expired. The core ignores a timer that is not a
/// timer of a mark now.
///
/// - The timeout of an issued call: if a newer call for the entity waits in
///   the outbox, the mark stays for that call. Otherwise the core removes the
///   mark.
/// - The timeout of the outbox: if a call for the entity still waits in the
///   outbox, the core removes the mark. The call stays in the outbox.
///
/// The tile then shows the last state that the core has. While the device
/// sleeps, the core removes the mark but does not draw. The wake draws the
/// page.
pub fn timeout(id: u32, model: &mut Model) -> Cmd {
    let outbox = &model.connection.outbox;
    let marks = &mut model.pending.marks;
    let Some(index) = marks.iter().position(|mark| mark.has_timer(id)) else {
        return Cmd::done();
    };
    let mark = &mut marks[index];
    let waits = outbox.iter().any(|call| call.entity_id == mark.entity_id);
    if mark.timeout.as_ref().is_some_and(|timer| timer.id == id) {
        mark.timeout = None;
        if waits {
            return Cmd::done();
        }
    } else {
        mark.queued = None;
        if !waits {
            return Cmd::done();
        }
    }
    marks.swap_remove(index).clear_timers();
    if model.power.asleep {
        return Cmd::done();
    }
    redraw(model, false)
}
