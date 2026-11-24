//! The pending feature: the panel shows a tap at once, before Home Assistant
//! confirms it.
//!
//! A service call changes the panel only when the new state comes back on
//! the state stream. That takes about 0.5 s while the device is awake, and
//! about 3 s after a wake. Thus a tap on a light tile, a − or + step of a
//! light or a thermostat, a scene or the All lights toggle gives the entity a
//! mark. The view shows a tile
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
//! - A step of a thermostat: the time of the last update is later than the
//!   time at the tap, and the target temperature is the target that the call
//!   asks for. The current temperature of a thermostat changes on its own,
//!   and each change moves `last_updated`, so the time alone does not confirm
//!   the call. Home Assistant also sends `last_updated` before the
//!   attributes, so the mark stays until the new target arrives.
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
//! a light at 100%, so that step shows no mark. The core does not know the
//! limits of a thermostat, so it sends each step. If Home Assistant keeps the
//! target at a limit, the mark shows until the timeout.
//!
//! # Steps that wait
//!
//! The mark of a step of a thermostat keeps the target that the call asks
//! for. If the user taps again before Home Assistant confirms the call, the
//! next step starts from that target, not from the target that the tile
//! shows. Refer to [`requested_target`].
//!
//! # Timers
//!
//! - [`PENDING_TIMEOUT`] (or [`TARGET_TIMEOUT`] for a step of a thermostat)
//!   after the core issued the service call, Home Assistant did not confirm
//!   it. The core removes the mark, and the tile
//!   shows the last state that the core has. If a newer call for the entity
//!   waits in the outbox, the mark stays for that call while its timeout of
//!   the outbox runs. If an old call waits with no timeout of the outbox
//!   (its mark ended at the queued timeout), the core removes the mark. The
//!   flow of the core does not reach that case now.
//! - [`SCENE_TIMEOUT`] after the core issued the call of a scene, the core
//!   removes its mark.
//! - [`QUEUED_TIMEOUT`] after the first call of a mark went into the
//!   outbox, the connection did not open. The core removes the mark, but the
//!   call stays in the outbox. If the connection opens later, the core sends
//!   it.
//!
//! A mark always has a timer. It has the timeout of the call that the core
//! issued last, the timeout of the outbox, or both. When a timer expires,
//! the mark stays only for its other timer, and only if that timer still
//! applies. The timeout of the outbox applies while a call waits. The
//! timeout of an issued call applies while no call waits. In all other
//! cases, the core removes the mark.
//!
//! The timeout of a call starts when the core issues the call: at once while
//! the connection is open, or when the outbox drains after the connect. Thus
//! the reconnect after a wake does not use the time of the timeout. The
//! core clears a timer when the mark no longer needs it. Each timer has an
//! id, and the core ignores the expiry of a timer that is not a timer of a
//! mark now.
//!
//! # Drop
//!
//! If the outbox drops a service call because it is full, the core removes
//! the mark. It keeps the mark if another call for the entity waits in the
//! outbox, or if a call for the entity went out. If the mark stays and no
//! call for the entity waits, the core clears the timeout of the outbox.
//!
//! # Sleep
//!
//! A sleep does not start while a mark is open and no service call waits in
//! the outbox. The sleep waits until each mark ends, and the end of the last
//! mark starts it. Refer to
//! [`sleep_if_settled`](crate::features::power::sleep_if_settled). After a
//! confirmation, the sleep starts when the draw timer expires. The last
//! image of the sleep thus shows the state that Home Assistant confirmed,
//! or the last state that the core has after a timeout. If a call still
//! waits in the outbox, the sleep does not wait for the marks. It drops the
//! calls and ends each mark, as the next paragraph tells.
//!
//! No mark stays through a sleep ([`end_for_sleep`]). A timer of the core
//! cannot wake the chip, so no timer may wait at the `Sleep` request. At the
//! disconnect step of a sleep, the core drops the outbox, so no call waits.
//! The core then removes each mark and clears its timers, also the mark of
//! a call that went out. The tile shows the last state that the core has,
//! so the last image of the sleep shows no mark. After the wake, the
//! retained messages show the true state.
//!
//! A tap that wakes the device gives a new mark. The mark shows during the
//! reconnect, and its timeout starts when the core issues the call.

use alloc::{string::String, vec::Vec};
use core::time::Duration;

use crate::{
    Cmd, Model,
    entities::EntityState,
    features::{
        power,
        refresh::{redraw, redraw_soon},
        timer::Running,
    },
    temperature::{self, Centi},
};
use jiff::Timestamp;

/// The internal events of the pending marks. The core makes them from its
/// timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The timer of a mark with this id expired. Refer to
    /// [`pending`](crate::features::pending).
    Timeout(u32),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Pending(event)
    }
}

/// Handles an internal event of the pending marks.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::Timeout(id) => timeout(id, model),
    }
}

/// The time after which the core removes the mark of a service call that
/// Home Assistant did not confirm.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

/// The time after which the core removes the mark of a step of a thermostat
/// that Home Assistant did not confirm.
///
/// A thermostat can take much longer than a light to report its new target.
/// A Nest thermostat reports it through the Google cloud, and on the board
/// that took from 1.3 s to 14 s. The time is less than
/// [`IDLE_TIMEOUT`](crate::features::power::IDLE_TIMEOUT), so the idle
/// timeout never ends the mark. A `SleepNow` waits for the end of the mark.
/// Refer to [`sleep_if_settled`](crate::features::power::sleep_if_settled).
pub const TARGET_TIMEOUT: Duration = Duration::from_secs(20);

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
    /// A step of a thermostat to this target temperature: a time of the last
    /// update after the time at the tap, and this target.
    Target(Centi),
    /// A scene: nothing confirms it.
    Scene,
}

/// The tap that gave a mark: how Home Assistant confirms the call, and the
/// state and the time of the entity at the tap.
#[derive(Debug)]
pub struct Tap {
    /// How Home Assistant confirms the call.
    pub confirm: Confirm,
    /// The state of the entity at the tap, or `None` if the core had no
    /// state.
    pub state: Option<String>,
    /// The exact `last_updated` time of the entity at the tap, or `None` if
    /// the core had no time.
    pub since: Option<Timestamp>,
}

/// Returns the tap for a service call for `entity_id`, with the state and
/// the time that the core has now. Make the tap before the core sends the
/// call.
#[must_use]
pub fn tap(model: &Model, entity_id: &str, confirm: Confirm) -> Tap {
    let entity = model.entities.get(entity_id);
    Tap {
        confirm,
        state: entity.and_then(|entity| entity.state.clone()),
        since: entity.and_then(|entity| entity.last_updated),
    }
}

/// The timers of a mark. A mark always has at least one timer.
#[derive(Debug)]
enum Timers {
    /// The core issued a call, and no other call for the entity waits in the
    /// outbox. The timer is the timeout of the call that the core issued
    /// last.
    ///
    /// In one corner case, an old call for the entity waits with no timeout
    /// of the outbox. A queued timeout removed the old mark, and its call
    /// stayed in the outbox. Then a new call went out while the old call
    /// waited. The flow of the core does not reach this case now, because
    /// the drain uses each free place in the session before a new call can.
    /// If the case occurs, the core removes the mark at the issued timeout.
    Issued(Running),
    /// Calls for the entity wait in the outbox, and the core issued none.
    /// The timer is the timeout of the outbox.
    Queued(Running),
    /// The core issued a call, and another call for the entity waits in the
    /// outbox.
    Both {
        /// The timeout of the call that the core issued last.
        issued: Running,
        /// The timeout of the outbox.
        queued: Running,
    },
}

impl Timers {
    /// Returns `true` if the timer `id` is one of the timers.
    const fn has(&self, id: u32) -> bool {
        match self {
            Self::Issued(timer) | Self::Queued(timer) => timer.id() == id,
            Self::Both { issued, queued } => issued.id() == id || queued.id() == id,
        }
    }

    /// Clears all the timers.
    fn clear(self) {
        match self {
            Self::Issued(timer) | Self::Queued(timer) => timer.clear(),
            Self::Both { issued, queued } => {
                issued.clear();
                queued.clear();
            }
        }
    }

    /// The core issued a call, with the new timeout `timer`. The new timeout
    /// replaces the timeout before it. If no other call for the entity
    /// waits (`waits` is `false`), the timeout of the outbox stops.
    fn issued(self, timer: Running, waits: bool) -> Self {
        let queued = match self {
            Self::Issued(old) => {
                old.clear();
                None
            }
            Self::Queued(queued) => Some(queued),
            Self::Both { issued, queued } => {
                issued.clear();
                Some(queued)
            }
        };
        match queued {
            Some(queued) if waits => Self::Both {
                issued: timer,
                queued,
            },
            Some(queued) => {
                queued.clear();
                Self::Issued(timer)
            }
            None => Self::Issued(timer),
        }
    }

    /// A call for the entity went into the outbox. If the timeout of the
    /// outbox does not run, start it. Returns the command of the new timer.
    fn queued(self, state: &mut State) -> (Cmd, Self) {
        match self {
            Self::Issued(issued) => {
                let (command, queued) = state.start(QUEUED_TIMEOUT);
                (command, Self::Both { issued, queued })
            }
            timers @ (Self::Queued(_) | Self::Both { .. }) => (Cmd::done(), timers),
        }
    }

    /// The outbox dropped calls. If a call for the entity still waits
    /// (`waits` is `true`), the timers stay. If not, the timeout of the
    /// outbox stops. Returns `None` if no timer stays.
    fn dropped(self, waits: bool) -> Option<Self> {
        match self {
            timers @ (Self::Queued(_) | Self::Both { .. }) if waits => Some(timers),
            timers @ Self::Issued(_) => Some(timers),
            Self::Queued(queued) => {
                queued.clear();
                None
            }
            Self::Both { issued, queued } => {
                queued.clear();
                Some(Self::Issued(issued))
            }
        }
    }

    /// The timer `id` expired. The timer of the outbox does not matter if no
    /// call waits, and the timeout of the issued call does not matter if a
    /// call waits. Returns the timers that stay, or `None` if no timer
    /// stays. The function clears the timers that do not stay.
    fn expired(self, id: u32, waits: bool) -> Option<Self> {
        match self {
            // The last timer expired.
            Self::Issued(_) | Self::Queued(_) => None,
            Self::Both { issued, queued } if issued.id() == id => {
                if waits {
                    Some(Self::Queued(queued))
                } else {
                    queued.clear();
                    None
                }
            }
            Self::Both { issued, queued: _ } => {
                if waits {
                    issued.clear();
                    None
                } else {
                    Some(Self::Issued(issued))
                }
            }
        }
    }
}

/// The mark of one entity.
#[derive(Debug)]
pub struct Mark {
    /// The entity of the service call.
    pub entity_id: &'static str,
    /// The newest tap on the entity.
    pub tap: Tap,
    /// The timers of the mark.
    timers: Timers,
}

impl Mark {
    /// Returns `true` if `entity` confirms the call. Refer to the module
    /// documentation.
    fn confirmed_by(&self, entity: Option<&EntityState>) -> bool {
        let tap = &self.tap;
        let state = entity.and_then(|entity| entity.state.as_deref());
        let updated = entity.and_then(|entity| entity.last_updated);
        let newer = match (tap.since, updated) {
            (Some(since), Some(updated)) => updated > since,
            (Some(_), None) => false,
            (None, _) => true,
        };
        match tap.confirm {
            Confirm::Toggle => {
                tap.state.is_some() && state.is_some() && state != tap.state.as_deref() && newer
            }
            Confirm::Step => tap.since.is_some() && newer,
            Confirm::Target(target) => {
                tap.since.is_some() && newer && entity.and_then(temperature::target) == Some(target)
            }
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

    /// Removes the mark of `entity_id` and returns it.
    fn take(&mut self, entity_id: &str) -> Option<Mark> {
        let index = self
            .marks
            .iter()
            .position(|mark| mark.entity_id == entity_id)?;
        Some(self.marks.swap_remove(index))
    }

    /// Starts a timer of `wait`, and returns its command and the timer.
    fn start(&mut self, wait: Duration) -> (Cmd, Running) {
        self.timer = self.timer.wrapping_add(1);
        let id = self.timer;
        Running::start(id, wait, Event::Timeout(id).into())
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

/// Returns the target of the step of the thermostat `entity_id` that waits.
///
/// The target is the target temperature that the newest step asks for, if
/// Home Assistant did not confirm that step yet. Returns `None` if the entity
/// has no mark of a step of a thermostat.
#[must_use]
pub fn requested_target(model: &Model, entity_id: &str) -> Option<Centi> {
    model
        .pending
        .marks
        .iter()
        .find(|mark| mark.entity_id == entity_id)
        .and_then(|mark| match mark.tap.confirm {
            Confirm::Target(target) => Some(target),
            Confirm::Toggle | Confirm::Step | Confirm::Scene => None,
        })
}

/// Gives the mark of `entity_id` the new tap `tap`, if the entity has a
/// mark. The timers of the mark stay.
///
/// The core uses this function for a call that it drops before the call
/// goes into the outbox or out. A new tap with no mark gives no mark there.
pub fn retap(entity_id: &str, tap: Tap, model: &mut Model) {
    if let Some(mark) = model.pending.get_mut(entity_id) {
        mark.tap = tap;
    }
}

/// A service call for `entity_id` went into the outbox. If no timeout of
/// the outbox runs for its mark, start [`QUEUED_TIMEOUT`].
///
/// `tap` is the tap that asks for the call, or `None` for a call with no
/// tap. A tap gives the entity a mark, or replaces the tap of its mark. The
/// timers of a mark stay.
pub fn queued(entity_id: &'static str, tap: Option<Tap>, model: &mut Model) -> Cmd {
    let state = &mut model.pending;
    let (command, mark) = match (state.take(entity_id), tap) {
        (
            Some(Mark {
                tap: old, timers, ..
            }),
            tap,
        ) => {
            let (command, timers) = timers.queued(state);
            (
                command,
                Mark {
                    entity_id,
                    tap: tap.unwrap_or(old),
                    timers,
                },
            )
        }
        (None, Some(tap)) => {
            let (command, timer) = state.start(QUEUED_TIMEOUT);
            (
                command,
                Mark {
                    entity_id,
                    tap,
                    timers: Timers::Queued(timer),
                },
            )
        }
        (None, None) => return Cmd::done(),
    };
    // Each path between `take` and `push` must put the mark back.
    debug_assert!(state.marks.iter().all(|mark| mark.entity_id != entity_id));
    state.marks.push(mark);
    command
}

/// The core issued the service call for `entity_id`: start the timeout of
/// its mark.
///
/// `tap` is the tap of a new call, or `None` for a call from the outbox. A
/// new tap gives the entity a mark, or replaces the tap of its mark.
///
/// The new timeout replaces the timeout before it. If no other call for the
/// entity waits in the outbox, the timeout of the outbox stops.
pub fn issued(entity_id: &'static str, tap: Option<Tap>, model: &mut Model) -> Cmd {
    let waits = model
        .connection
        .outbox
        .iter()
        .any(|call| call.entity_id == entity_id);
    let state = &mut model.pending;
    let (tap, timers) = match (state.take(entity_id), tap) {
        (
            Some(Mark {
                tap: old, timers, ..
            }),
            tap,
        ) => (tap.unwrap_or(old), Some(timers)),
        (None, Some(tap)) => (tap, None),
        (None, None) => return Cmd::done(),
    };
    let wait = match tap.confirm {
        Confirm::Scene => SCENE_TIMEOUT,
        Confirm::Target(_) => TARGET_TIMEOUT,
        Confirm::Toggle | Confirm::Step => PENDING_TIMEOUT,
    };
    let (command, timer) = state.start(wait);
    let timers = match timers {
        Some(timers) => timers.issued(timer, waits),
        None => Timers::Issued(timer),
    };
    // Each path between `take` and `push` must put the mark back.
    debug_assert!(state.marks.iter().all(|mark| mark.entity_id != entity_id));
    state.marks.push(Mark {
        entity_id,
        tap,
        timers,
    });
    command
}

/// The outbox dropped one or more service calls. Returns `true` if the core
/// removed a mark.
///
/// This removal does not start a sleep that waits for the marks. Only a new
/// call can make the outbox drop a call, and in that wait no call waits.
/// The tap that asks for a new call cancels the sleep that waits first.
///
/// The core removes each mark that has no timeout of an issued call and no
/// call in the outbox. A mark that stays with no call in the outbox loses
/// its timeout of the outbox.
pub fn forget_dropped(model: &mut Model) -> bool {
    let outbox = &model.connection.outbox;
    let marks = &mut model.pending.marks;
    let before = marks.len();
    *marks = core::mem::take(marks)
        .into_iter()
        .filter_map(
            |Mark {
                 entity_id,
                 tap,
                 timers,
             }| {
                let waits = outbox.iter().any(|call| call.entity_id == entity_id);
                timers.dropped(waits).map(|timers| Mark {
                    entity_id,
                    tap,
                    timers,
                })
            },
        )
        .collect();
    marks.len() != before
}

/// Removes the mark of `entity_id` if Home Assistant confirmed its service
/// call, after a state-stream message for the entity.
///
/// Refer to the module documentation for the rules. The draw waits for the
/// other messages of the change, as for any change from the state stream.
/// Home Assistant sends `last_updated` before the attributes, so a draw at
/// once can show the old brightness. If a sleep waits for the marks, the
/// expiry of the draw timer starts it.
pub fn confirm(entity_id: &str, model: &mut Model) -> Cmd {
    let entity = model.entities.get(entity_id);
    let marks = &mut model.pending.marks;
    let Some(index) = marks
        .iter()
        .position(|mark| mark.entity_id == entity_id && mark.confirmed_by(entity))
    else {
        return Cmd::done();
    };
    marks.swap_remove(index).timers.clear();
    redraw_soon(model).and(power::sleep_if_settled(model))
}

/// The device starts to sleep, and the outbox is empty: remove each mark and
/// clear its timers. Returns `true` if the core removed a mark. Refer to
/// the [module documentation](self).
///
/// The caller drops the outbox first. The device sleeps, so the core does
/// not draw. The last image of the sleep draws the tiles with no mark.
pub fn end_for_sleep(model: &mut Model) -> bool {
    debug_assert!(
        model.connection.outbox.is_empty(),
        "the sleep drops the outbox first"
    );
    let marks = core::mem::take(&mut model.pending.marks);
    let ended = !marks.is_empty();
    for mark in marks {
        mark.timers.clear();
    }
    ended
}

/// The timer `id` of a mark expired. The core ignores a timer that is not a
/// timer of a mark now.
///
/// - The timeout of an issued call: if a newer call for the entity waits in
///   the outbox and its timeout of the outbox runs, the mark stays for that
///   call. Otherwise the core removes the mark.
/// - The timeout of the outbox: if no call for the entity waits in the
///   outbox and the timeout of an issued call runs, the mark stays for that
///   call. Otherwise the core removes the mark. A call that waits stays in
///   the outbox.
///
/// The tile then shows the last state that the core has. While the device
/// sleeps, the core removes the mark but does not draw. The wake draws the
/// page. If a sleep waits for the marks, and this mark was the last one,
/// the sleep starts.
pub fn timeout(id: u32, model: &mut Model) -> Cmd {
    let outbox = &model.connection.outbox;
    let state = &mut model.pending;
    let Some(index) = state.marks.iter().position(|mark| mark.timers.has(id)) else {
        return Cmd::done();
    };
    let Mark {
        entity_id,
        tap,
        timers,
    } = state.marks.swap_remove(index);
    let waits = outbox.iter().any(|call| call.entity_id == entity_id);
    if let Some(timers) = timers.expired(id, waits) {
        state.marks.push(Mark {
            entity_id,
            tap,
            timers,
        });
        return Cmd::done();
    }
    if model.power.is_asleep() {
        return Cmd::done();
    }
    redraw(model, false).and(power::sleep_if_settled(model))
}

#[cfg(test)]
mod tests;
