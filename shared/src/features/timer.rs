//! Timers that the core can clear.
//!
//! [`notify_after`] starts a timer that sends its event only when the timer
//! completes. If the core clears the timer first, the timer sends nothing.
//! The shell then gets a `TimeClear` request for the timer, if it got the
//! `TimeNotifyAfter` request.
//!
//! A feature keeps the timer that runs as one value, [`Running`], in a
//! [`Slot`]. The slot holds the id of the timer and its handle, so the
//! feature cannot hold an id without a timer or a timer without an id. A new
//! timer in the slot clears the timer before it.
//!
//! A clear cannot stop an expiry that the shell already sent. Thus each
//! timer still has an id, and the feature ignores an expiry with an id that
//! is not the id of the timer in its slot.

use core::time::Duration;

use crux_time::{TimerHandle, TimerOutcome, clock::Time};

use crate::{Cmd, Event};

/// Starts a timer of `after`. The command sends `event` when the timer
/// completes, and sends nothing if the core clears the timer first.
///
/// To clear the timer, call [`TimerHandle::clear`] on the handle. If the
/// command already sent `TimeNotifyAfter`, it then sends `TimeClear` and ends
/// when the shell answers. If the command did not start yet, it ends with
/// no effects.
pub fn notify_after(after: Duration, event: Event) -> (Cmd, TimerHandle) {
    let (timer, handle) = Time::notify_after(after);
    let command = Cmd::new(move |ctx| async move {
        if let TimerOutcome::Completed(_) = timer.into_future(ctx.clone()).await {
            ctx.send_event(event);
        }
    });
    (command, handle)
}

/// A timer that runs: its id and its handle.
#[derive(Debug)]
pub struct Running {
    id: u32,
    handle: TimerHandle,
}

impl Running {
    /// Starts the timer `id` of `after`, which sends `event` when it
    /// completes. Refer to [`notify_after`].
    pub fn start(id: u32, after: Duration, event: Event) -> (Cmd, Self) {
        let (command, handle) = notify_after(after, event);
        (command, Self { id, handle })
    }

    /// Returns the id of the timer.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// Clears the timer.
    pub fn clear(self) {
        self.handle.clear();
    }
}

/// The timer of a feature that runs, or no timer.
#[derive(Debug, Default)]
pub struct Slot(Option<Running>);

impl Slot {
    /// Starts the timer `id` of `after`, which sends `event` when it
    /// completes. Clears the timer that runs now, if there is one.
    pub fn start(&mut self, id: u32, after: Duration, event: Event) -> Cmd {
        let (command, running) = Running::start(id, after, event);
        self.stop();
        self.0 = Some(running);
        command
    }

    /// Clears the timer that runs, if there is one.
    pub fn stop(&mut self) {
        if let Some(running) = self.0.take() {
            running.clear();
        }
    }

    /// Returns `true` if the timer `id` runs.
    #[must_use]
    pub fn is(&self, id: u32) -> bool {
        self.0.as_ref().is_some_and(|running| running.id == id)
    }

    /// Returns `true` if a timer runs.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.0.is_some()
    }

    /// The timer `id` expired. If it is the timer that runs, the slot
    /// forgets it and the function returns `true`. Otherwise, the expiry is
    /// stale, and the function returns `false`.
    pub fn expired(&mut self, id: u32) -> bool {
        if !self.is(id) {
            return false;
        }
        self.0 = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::features::light;
    use core::time::Duration;

    use crux_core::Command;

    use super::{Slot, notify_after};
    use crate::{Effect, Event, app::EffectTestExt};

    const AFTER: Duration = Duration::from_secs(5);

    #[test]
    fn a_timer_that_completes_sends_its_event() {
        let (mut command, _handle) = notify_after(AFTER, Event::Light(light::Event::Timeout(7)));

        let event = command
            .resolve_time_notify_after(|notify| {
                assert_eq!(Duration::from(notify.duration), AFTER);
                notify.id
            })
            .expect_one_event();

        assert!(
            matches!(event, Event::Light(light::Event::Timeout(7))),
            "{event:?}"
        );
        assert!(command.is_done());
    }

    #[test]
    fn a_cleared_timer_asks_the_shell_to_clear_it_and_sends_no_event() {
        let (mut command, handle) = notify_after(AFTER, Event::Light(light::Event::Timeout(7)));
        let notify = command.expect_one_effect().expect_time_notify_after();
        let id = notify.operation.id;

        handle.clear();

        command
            .resolve_time_clear(|clear| {
                assert_eq!(clear.id, id);
                clear.id
            })
            .expect_no_events();
        assert!(command.is_done());
    }

    #[test]
    fn a_timer_cleared_before_it_starts_sends_nothing() {
        let (mut command, handle) = notify_after(AFTER, Event::Light(light::Event::Timeout(7)));

        handle.clear();

        command.expect_no_effect_or_events();
        assert!(command.is_done());
    }

    #[test]
    fn a_new_timer_in_a_slot_clears_the_old_timer() {
        let mut slot = Slot::default();
        let mut old: Command<Effect, Event> =
            slot.start(1, AFTER, Event::Light(light::Event::Timeout(1)));
        let id = old
            .expect_one_effect()
            .expect_time_notify_after()
            .operation
            .id;

        let mut new = slot.start(2, AFTER, Event::Light(light::Event::Timeout(2)));

        old.resolve_time_clear(|clear| {
            assert_eq!(clear.id, id);
            clear.id
        })
        .expect_no_events();
        assert!(old.is_done());
        assert!(slot.is(2));
        assert!(!slot.is(1));
        let event = new
            .resolve_time_notify_after(|notify| notify.id)
            .expect_one_event();
        assert!(
            matches!(event, Event::Light(light::Event::Timeout(2))),
            "{event:?}"
        );
    }

    #[test]
    fn a_slot_takes_only_the_expiry_of_its_timer() {
        let mut slot = Slot::default();
        let _command = slot.start(1, AFTER, Event::Light(light::Event::Timeout(1)));

        assert!(!slot.expired(2));
        assert!(slot.is_running());
        assert!(slot.expired(1));
        assert!(!slot.is_running());
        assert!(!slot.expired(1));
    }
}
