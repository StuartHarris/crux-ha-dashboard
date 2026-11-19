//! The decisions of the task that runs a `Sleep` job: when the chip can go
//! into light sleep, and which wake is a real wake.
//!
//! The core owns the steps before the sleep (disconnect, then Wi-Fi off,
//! then the last image and `Sleep`), and it handles the wake. The firmware
//! only does the job. Light sleep keeps the RAM, so the core and its model
//! stay. A wake with no cause in the plan is a detail of the firmware: the
//! chip sleeps again after [`WAKE_HOLD_MS`], and the core gets no answer.
//!
//! The charger cannot wake the chip. So while a `Sleep` job waits, the RTC
//! timer also wakes the chip each [`CHECK_SECONDS`] for a check of the power
//! source ([`After::CheckPower`]). The task reads only the charger, with no
//! Wi-Fi. If the power monitor sees a new source, the shell sends
//! `PowerSourceChanged` and answers `Sleep` with `Wake::PowerSource`.
//! Otherwise the chip sleeps again.

use shared::capabilities::power::{Sleep, Wake, WakeSource};

/// The time after a wake that was not real before the chip can sleep again,
/// in milliseconds.
///
/// In this time, the task reads the touch controller and the power source.
/// A change of the power source needs a second read after 1 s.
pub const WAKE_HOLD_MS: u64 = 3000;
/// The time after a tap before the chip can sleep again, in milliseconds.
/// The shell gets the tap in this time, and it ends the `Sleep` request.
pub const TAP_HOLD_MS: u64 = 2000;
/// The time in milliseconds that one [`Blocker`] can stop a sleep before the
/// task writes a warning about it.
pub const BLOCK_WARNING_MS: u64 = 5000;
/// The longest sleep in seconds while a `Sleep` job waits. The chip then
/// wakes to check the power source.
pub const CHECK_SECONDS: u32 = 60;
/// The time in milliseconds that a timer wake can come before the deadline
/// of the core and still be the timer of the core.
const DEADLINE_SLACK_MS: u64 = 50;

/// The wake sources of one sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// A touch on the panel wakes the chip.
    pub touch: bool,
    /// The RTC timer wakes the chip after this number of seconds.
    pub timer_seconds: Option<u32>,
}

impl Plan {
    /// Makes the plan for the `Sleep` operation of the core. If the
    /// operation has more than one timer, the shortest timer wins.
    #[must_use]
    pub fn new(sleep: &Sleep) -> Self {
        let mut plan = Self {
            touch: false,
            timer_seconds: None,
        };
        for source in &sleep.wake_on {
            match source {
                WakeSource::Touch => plan.touch = true,
                WakeSource::Timer { seconds } => {
                    plan.timer_seconds = Some(
                        plan.timer_seconds
                            .map_or(*seconds, |shortest| shortest.min(*seconds)),
                    );
                }
            }
        }
        plan
    }

    /// Returns the plan if the hardware can do it now, or `None`. A touch
    /// wake needs a touch controller that started, because the INT pin
    /// floats before the start. Without it, the device stays awake, so that
    /// the touch poll can start the controller and a tap can wake the device.
    #[must_use]
    pub const fn armed(self, touch_ready: bool) -> Option<Self> {
        if self.touch && !touch_ready {
            None
        } else {
            Some(self)
        }
    }
}

/// What the hardware reports at the end of a light sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The GT911 INT pin went low.
    Touch,
    /// The RTC timer expired.
    Timer,
    /// Another cause, or the hardware rejected the sleep.
    Other,
}

/// What the task does after a light sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// A touch: read the point and send it as a tap, which answers `Sleep`.
    /// If the task cannot read the point, it answers `Wake::Touch(None)`.
    Touch,
    /// A real wake: answer `Sleep` with this cause.
    Answer(Wake),
    /// The timer of the power check: read the charger, then sleep again
    /// unless the source changed.
    CheckPower,
    /// A wake that was not real: sleep again after [`WAKE_HOLD_MS`].
    SleepAgain,
}

/// A thing that stops a sleep now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// A view model waits to be drawn. It can be the last image before the
    /// sleep.
    Render,
    /// The GT911 holds INT low, so a touch wake would end the sleep at once.
    Touch,
}

/// The state of the device that decides if the chip can sleep now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Readiness {
    /// A view model waits to be drawn.
    pub render_pending: bool,
    /// The GT911 holds INT low.
    pub touch_pending: bool,
}

impl Readiness {
    /// Returns the first thing that stops a sleep with `plan`, or `None` if
    /// the chip can sleep. INT stops only a sleep with a touch wake.
    #[must_use]
    pub const fn blocker(&self, plan: Plan) -> Option<Blocker> {
        if self.render_pending {
            Some(Blocker::Render)
        } else if plan.touch && self.touch_pending {
            Some(Blocker::Touch)
        } else {
            None
        }
    }
}

/// Decides when the task can sleep again, and when a blocker gets a warning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sleeper {
    /// The chip does not sleep before this time, in milliseconds since the
    /// start.
    hold_until_ms: u64,
    /// The thing that stops the sleep, the time from which it stops it, and
    /// `true` after the warning about it.
    blocked: Option<(Blocker, u64, bool)>,
    /// The time of the timer wake of the core, if the job has one.
    deadline_ms: Option<u64>,
    /// The task wrote the warning that no wake source is ready for this
    /// job.
    warned_not_ready: bool,
}

impl Sleeper {
    /// A new `Sleep` job with `plan` starts at the time `now_ms`.
    pub fn start(&mut self, now_ms: u64, plan: Plan) {
        self.deadline_ms = plan
            .timer_seconds
            .map(|seconds| now_ms.saturating_add(u64::from(seconds) * 1000));
        self.warned_not_ready = false;
    }

    /// Returns the seconds to the next timer wake at the time `now_ms`: the
    /// time to the deadline of the core, at most [`CHECK_SECONDS`].
    #[must_use]
    pub fn timer_seconds(&self, now_ms: u64) -> u32 {
        self.deadline_ms.map_or(CHECK_SECONDS, |deadline| {
            let seconds = deadline.saturating_sub(now_ms).div_ceil(1000).max(1);
            u32::try_from(seconds).map_or(CHECK_SECONDS, |seconds| seconds.min(CHECK_SECONDS))
        })
    }

    /// Returns what the task does after a sleep that ended at the time
    /// `now_ms` with `cause`.
    ///
    /// A touch wake is real also if the task reads no tap after it, because
    /// the finger can lift before the read. A timer wake is real only at the
    /// deadline of the core. Before it, the wake is the power check.
    #[must_use]
    pub fn after(&self, now_ms: u64, cause: Cause) -> After {
        let at_deadline = self
            .deadline_ms
            .is_some_and(|deadline| now_ms.saturating_add(DEADLINE_SLACK_MS) >= deadline);
        match cause {
            Cause::Touch => After::Touch,
            Cause::Timer if at_deadline => After::Answer(Wake::Timer),
            Cause::Timer => After::CheckPower,
            Cause::Other => After::SleepAgain,
        }
    }

    /// No wake source is ready at the time `now_ms`, so the chip does not
    /// sleep. Holds the sleep for [`WAKE_HOLD_MS`], and returns `true` once
    /// for each job, for one warning.
    pub fn not_ready(&mut self, now_ms: u64) -> bool {
        self.woke(now_ms);
        !core::mem::replace(&mut self.warned_not_ready, true)
    }
    /// Returns `true` if the hold after the last wake or tap ended at the
    /// time `now_ms`.
    #[must_use]
    pub const fn can_sleep(&self, now_ms: u64) -> bool {
        now_ms >= self.hold_until_ms
    }

    /// The task sent a tap at the time `now_ms`.
    pub fn tapped(&mut self, now_ms: u64) {
        self.hold(now_ms.saturating_add(TAP_HOLD_MS));
    }

    /// The chip woke at the time `now_ms` from a wake that was not real.
    pub fn woke(&mut self, now_ms: u64) {
        self.hold(now_ms.saturating_add(WAKE_HOLD_MS));
    }

    /// `blocker` stopped a sleep at the time `now_ms`. Returns `true` once,
    /// when the same blocker stopped the sleeps for [`BLOCK_WARNING_MS`].
    /// The caller then writes a warning, and can try to remove the blocker.
    pub fn blocked(&mut self, now_ms: u64, blocker: Blocker) -> bool {
        let (_, since, warned) = match &mut self.blocked {
            Some(state) if state.0 == blocker => state,
            other => other.insert((blocker, now_ms, false)),
        };
        if *warned || now_ms.saturating_sub(*since) < BLOCK_WARNING_MS {
            return false;
        }
        *warned = true;
        true
    }

    /// No blocker stopped the sleep.
    pub const fn unblocked(&mut self) {
        self.blocked = None;
    }

    fn hold(&mut self, until_ms: u64) {
        self.hold_until_ms = self.hold_until_ms.max(until_ms);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use shared::capabilities::power::{Sleep, Wake, WakeSource};

    use super::{
        After, BLOCK_WARNING_MS, Blocker, CHECK_SECONDS, Cause, Plan, Readiness, Sleeper,
        TAP_HOLD_MS, WAKE_HOLD_MS,
    };

    const TOUCH: Plan = Plan {
        touch: true,
        timer_seconds: None,
    };

    #[test]
    fn the_plan_has_the_wake_sources_of_the_core() {
        assert_eq!(
            Plan::new(&Sleep {
                wake_on: vec![WakeSource::Touch]
            }),
            TOUCH
        );
        assert_eq!(
            Plan::new(&Sleep {
                wake_on: vec![
                    WakeSource::Timer { seconds: 600 },
                    WakeSource::Touch,
                    WakeSource::Timer { seconds: 60 },
                ]
            }),
            Plan {
                touch: true,
                timer_seconds: Some(60)
            }
        );
        assert_eq!(
            Plan::new(&Sleep { wake_on: vec![] }),
            Plan {
                touch: false,
                timer_seconds: None
            }
        );
    }

    #[test]
    fn a_touch_wake_needs_a_touch_controller_that_started() {
        assert_eq!(TOUCH.armed(true), Some(TOUCH));
        assert_eq!(TOUCH.armed(false), None);
        let timer = Plan {
            touch: false,
            timer_seconds: Some(5),
        };
        assert_eq!(timer.armed(false), Some(timer));
    }

    #[test]
    fn with_no_timer_of_the_core_each_timer_wake_is_a_power_check() {
        let mut sleeper = Sleeper::default();
        sleeper.start(0, TOUCH);
        assert_eq!(sleeper.timer_seconds(0), CHECK_SECONDS);
        assert_eq!(sleeper.after(60_000, Cause::Timer), After::CheckPower);
        assert_eq!(sleeper.after(60_000, Cause::Touch), After::Touch);
        // A rejected sleep, or a wake with no known cause: sleep again.
        assert_eq!(sleeper.after(60_000, Cause::Other), After::SleepAgain);
    }

    #[test]
    fn the_timer_of_the_core_is_a_real_wake_at_its_deadline() {
        let mut sleeper = Sleeper::default();
        let plan = Plan {
            touch: true,
            timer_seconds: Some(90),
        };
        sleeper.start(1000, plan);
        // The first wake is the power check at 60 s.
        assert_eq!(sleeper.timer_seconds(1000), CHECK_SECONDS);
        assert_eq!(sleeper.after(61_000, Cause::Timer), After::CheckPower);
        // Then the chip sleeps to the deadline of the core.
        assert_eq!(sleeper.timer_seconds(61_000), 30);
        assert_eq!(
            sleeper.after(91_000, Cause::Timer),
            After::Answer(Wake::Timer)
        );
        // A wake a little before the deadline is the timer of the core.
        assert_eq!(
            sleeper.after(90_980, Cause::Timer),
            After::Answer(Wake::Timer)
        );
        // After the deadline, the next sleep is at least 1 s.
        assert_eq!(sleeper.timer_seconds(95_000), 1);
    }

    #[test]
    fn the_warning_that_no_wake_source_is_ready_comes_once_for_each_job() {
        let mut sleeper = Sleeper::default();
        sleeper.start(0, TOUCH);
        assert!(sleeper.not_ready(0));
        assert!(!sleeper.can_sleep(WAKE_HOLD_MS - 1));
        assert!(!sleeper.not_ready(WAKE_HOLD_MS));
        assert!(!sleeper.not_ready(2 * WAKE_HOLD_MS));
        // A new job warns again.
        sleeper.start(10_000, TOUCH);
        assert!(sleeper.not_ready(10_000));
    }

    #[test]
    fn the_chip_sleeps_only_when_nothing_blocks_it() {
        let quiet = Readiness::default();
        assert_eq!(quiet.blocker(TOUCH), None);

        let render = Readiness {
            render_pending: true,
            touch_pending: true,
        };
        assert_eq!(render.blocker(TOUCH), Some(Blocker::Render));

        let touch = Readiness {
            touch_pending: true,
            ..quiet
        };
        assert_eq!(touch.blocker(TOUCH), Some(Blocker::Touch));
        // INT does not stop a sleep with only a timer wake.
        let timer = Plan {
            touch: false,
            timer_seconds: Some(60),
        };
        assert_eq!(touch.blocker(timer), None);
    }

    #[test]
    fn the_chip_waits_after_a_wake_that_was_not_real() {
        let mut sleeper = Sleeper::default();
        assert!(sleeper.can_sleep(0));

        sleeper.woke(10_000);
        assert!(!sleeper.can_sleep(10_000 + WAKE_HOLD_MS - 1));
        assert!(sleeper.can_sleep(10_000 + WAKE_HOLD_MS));
    }

    #[test]
    fn a_tap_holds_the_sleep_and_does_not_shorten_a_hold() {
        let mut sleeper = Sleeper::default();
        sleeper.tapped(1000);
        assert!(!sleeper.can_sleep(1000 + TAP_HOLD_MS - 1));
        assert!(sleeper.can_sleep(1000 + TAP_HOLD_MS));

        sleeper.woke(5000);
        sleeper.tapped(5000);
        assert!(!sleeper.can_sleep(5000 + TAP_HOLD_MS));
        assert!(sleeper.can_sleep(5000 + WAKE_HOLD_MS));
    }

    #[test]
    fn a_long_block_gives_one_warning() {
        let mut sleeper = Sleeper::default();
        assert!(!sleeper.blocked(0, Blocker::Touch));
        assert!(!sleeper.blocked(BLOCK_WARNING_MS - 1, Blocker::Touch));
        assert!(sleeper.blocked(BLOCK_WARNING_MS, Blocker::Touch));
        assert!(!sleeper.blocked(BLOCK_WARNING_MS + 30, Blocker::Touch));

        // Another blocker starts its own time.
        assert!(!sleeper.blocked(BLOCK_WARNING_MS + 60, Blocker::Render));
        assert!(sleeper.blocked(2 * BLOCK_WARNING_MS + 60, Blocker::Render));

        // After a pass with no blocker, the time starts again.
        sleeper.unblocked();
        assert!(!sleeper.blocked(3 * BLOCK_WARNING_MS, Blocker::Touch));
        assert!(sleeper.blocked(4 * BLOCK_WARNING_MS, Blocker::Touch));
    }
}
