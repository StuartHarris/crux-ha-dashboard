//! The decisions of the task that runs a `Sleep` job: when the chip can go
//! into light sleep.
//!
//! The core owns the schedule. It sends each `Sleep` request with its wake
//! sources: a touch, and a timer for the check of the power source. The
//! firmware arms these sources, sleeps, and answers each wake at once:
//! `Wake::Touch` (with the tap), `Wake::Key` (a press of the touch key),
//! `Wake::Timer` after it read the charger, or `Wake::Other`. The core then decides what to do next. Light sleep keeps
//! the RAM, so the core and its model stay.
//!
//! The firmware keeps only driver guards: the chip does not sleep while a
//! view model waits to be drawn, while the GT911 holds INT low, while an
//! event that the display task sent waits for the core task ([`InFlight`]),
//! or while a new power source waits for its second read ([`Blocker`]).
//! After a tap or a `Down` edge of the touch key, the chip waits
//! [`TAP_HOLD_MS`].

use core::sync::atomic::{AtomicU32, Ordering};

use shared::capabilities::power::{Sleep, WakeSource};

/// The time after a tap before the chip can sleep again, in milliseconds.
///
/// The display task sends a tap to the core task, and then tries to sleep
/// in the same pass of its loop, before the core task handles the tap. The
/// core cannot see a tap that waits in the queue, so this firmware guard
/// keeps the chip awake until the core task handled it. Then the shell
/// answers the `Sleep` request, or the core wakes from the tap.
pub const TAP_HOLD_MS: u64 = 2000;
/// The time in milliseconds that one [`Blocker`] can stop a sleep before the
/// task writes a warning about it.
pub const BLOCK_WARNING_MS: u64 = 5000;

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

/// A thing that stops a sleep now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// A view model waits to be drawn. It can be the last image before the
    /// sleep.
    Render,
    /// The GT911 holds INT low, so a touch wake would end the sleep at once.
    Touch,
    /// An event that the display task sent waits for the core task. The
    /// core can end the `Sleep` request at that event, for example on the
    /// stand. Refer to [`InFlight`].
    CoreBusy,
    /// A new power source waits for its second read. The read comes about
    /// 1 s later, and a sleep would stop it.
    PowerConfirm,
}

/// The state of the device that decides if the chip can sleep now.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate blocker that the task reads"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Readiness {
    /// A view model waits to be drawn.
    pub render_pending: bool,
    /// The GT911 holds INT low.
    pub touch_pending: bool,
    /// An event that the display task sent waits for the core task.
    pub core_busy: bool,
    /// A new power source waits for its second read.
    pub power_pending: bool,
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
        } else if self.core_busy {
            Some(Blocker::CoreBusy)
        } else if self.power_pending {
            Some(Blocker::PowerConfirm)
        } else {
            None
        }
    }
}

/// The number of events that the display task sent to the core task, and
/// that the core task did not handle yet.
///
/// The display task sends a change of the power source, and then tries to
/// sleep in the same pass of its loop. Without this count, the chip could
/// sleep with the event in the queue, and the core would see the stand only
/// at the next wake. The handshake is exact:
///
/// 1. The display task calls [`InFlight::sent`] before it pushes the event.
///    Thus the count is never 0 while the event waits.
/// 2. The core task calls [`InFlight::handled`] after `Shell::handle` for
///    the event returns, and after it signals the end of the `Sleep` job.
///    Thus the display task sees the end of the job before the count goes
///    to 0.
/// 3. While the count is above 0, each sleep stops at [`Blocker::CoreBusy`].
#[derive(Debug, Default)]
pub struct InFlight(AtomicU32);

impl InFlight {
    /// Makes a count of 0.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// The display task sends one more event.
    pub fn sent(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    /// The core task handled one event. The count does not go below 0.
    pub fn handled(&self) {
        let mut count = self.0.load(Ordering::SeqCst);
        while count > 0 {
            match self
                .0
                .compare_exchange_weak(count, count - 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(now) => count = now,
            }
        }
    }

    /// Returns `true` if an event waits for the core task.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.0.load(Ordering::SeqCst) > 0
    }
}

/// Decides when the task can sleep again after a tap, and when a blocker
/// gets a warning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sleeper {
    /// The chip does not sleep before this time, in milliseconds since the
    /// start.
    hold_until_ms: u64,
    /// The thing that stops the sleep, the time from which it stops it, and
    /// `true` after the warning about it.
    blocked: Option<(Blocker, u64, bool)>,
    /// The task wrote the warning that no wake source is ready for this
    /// job.
    warned_not_ready: bool,
}

impl Sleeper {
    /// A new `Sleep` job starts.
    pub const fn start(&mut self) {
        self.warned_not_ready = false;
    }

    /// No wake source is ready, so the chip does not sleep. The task tries
    /// again on its next pass. Returns `true` once for each job, for one
    /// warning.
    pub const fn not_ready(&mut self) -> bool {
        !core::mem::replace(&mut self.warned_not_ready, true)
    }

    /// Returns `true` if the hold after the last tap ended at the time
    /// `now_ms`.
    #[must_use]
    pub const fn can_sleep(&self, now_ms: u64) -> bool {
        now_ms >= self.hold_until_ms
    }

    /// The task sent a tap at the time `now_ms`. Refer to [`TAP_HOLD_MS`].
    pub fn tapped(&mut self, now_ms: u64) {
        self.hold_until_ms = self.hold_until_ms.max(now_ms.saturating_add(TAP_HOLD_MS));
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
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use shared::capabilities::power::{Sleep, WakeSource};

    use super::{BLOCK_WARNING_MS, Blocker, InFlight, Plan, Readiness, Sleeper, TAP_HOLD_MS};

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
    fn the_chip_sleeps_only_when_nothing_blocks_it() {
        let quiet = Readiness::default();
        assert_eq!(quiet.blocker(TOUCH), None);

        let render = Readiness {
            render_pending: true,
            touch_pending: true,
            core_busy: true,
            power_pending: true,
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
    fn a_tap_holds_the_sleep_for_the_tap_guard() {
        let mut sleeper = Sleeper::default();
        sleeper.tapped(1000);
        assert!(!sleeper.can_sleep(1000 + TAP_HOLD_MS - 1));
        assert!(sleeper.can_sleep(1000 + TAP_HOLD_MS));
    }

    #[test]
    fn a_power_source_that_waits_for_its_second_read_stops_each_sleep() {
        let power = Readiness {
            power_pending: true,
            ..Readiness::default()
        };
        let timer = Plan {
            touch: false,
            timer_seconds: Some(60),
        };
        for plan in [TOUCH, timer] {
            assert_eq!(power.blocker(plan), Some(Blocker::PowerConfirm));
        }
        // INT comes first, because its remedy is in the firmware.
        let both = Readiness {
            touch_pending: true,
            ..power
        };
        assert_eq!(both.blocker(TOUCH), Some(Blocker::Touch));
    }

    #[test]
    fn the_warning_that_no_wake_source_is_ready_comes_once_for_each_job_with_no_hold() {
        let mut sleeper = Sleeper::default();
        sleeper.start();
        assert!(sleeper.not_ready());
        assert!(!sleeper.not_ready());
        // The task tries again on each pass, so no hold starts.
        assert!(sleeper.can_sleep(0));
        // A new job warns again.
        sleeper.start();
        assert!(sleeper.not_ready());
    }

    #[test]
    fn a_plan_with_no_timer_arms_no_timer_wake() {
        let plan = Plan::new(&Sleep {
            wake_on: vec![WakeSource::Touch],
        });
        assert_eq!(plan.timer_seconds, None);
        let plan = Plan::new(&Sleep {
            wake_on: vec![WakeSource::Touch, WakeSource::Timer { seconds: 60 }],
        });
        assert_eq!(plan.timer_seconds, Some(60));
    }

    #[test]
    fn an_event_that_waits_for_the_core_stops_each_sleep() {
        let busy = Readiness {
            core_busy: true,
            ..Readiness::default()
        };
        let timer = Plan {
            touch: false,
            timer_seconds: Some(60),
        };
        for plan in [TOUCH, timer] {
            assert_eq!(busy.blocker(plan), Some(Blocker::CoreBusy));
        }
    }

    #[test]
    fn the_count_of_events_in_flight_goes_to_0_only_when_each_one_is_handled() {
        let in_flight = InFlight::new();
        assert!(!in_flight.is_busy());
        in_flight.sent();
        in_flight.sent();
        in_flight.handled();
        assert!(in_flight.is_busy());
        in_flight.handled();
        assert!(!in_flight.is_busy());
        // A second answer for one event does not go below 0.
        in_flight.handled();
        in_flight.sent();
        assert!(in_flight.is_busy());
    }

    #[test]
    fn the_confirming_read_of_the_stand_does_not_sleep_before_the_core_handled_it() {
        use shared::{Event, PowerSource};

        use crate::power::PowerMonitor;

        let in_flight = InFlight::new();
        let mut monitor = PowerMonitor::new(PowerSource::Battery);
        // The check after a timer wake: the first read of the stand waits for
        // its second read.
        assert_eq!(monitor.update(Some(PowerSource::Stand), None), []);
        let readiness = |monitor: &PowerMonitor, in_flight: &InFlight| Readiness {
            core_busy: in_flight.is_busy(),
            power_pending: monitor.is_pending(),
            ..Readiness::default()
        };
        assert_eq!(
            readiness(&monitor, &in_flight).blocker(TOUCH),
            Some(Blocker::PowerConfirm)
        );

        // The second read confirms the stand. The display task counts the
        // event, then sends it. The poll of the sleep in the same pass sees
        // no pending source, but the event waits, so the chip stays awake.
        for event in monitor.update(Some(PowerSource::Stand), None) {
            assert_eq!(event, Event::PowerSourceChanged(PowerSource::Stand));
            in_flight.sent();
        }
        assert!(!monitor.is_pending());
        assert_eq!(
            readiness(&monitor, &in_flight).blocker(TOUCH),
            Some(Blocker::CoreBusy)
        );

        // The core task handled the event.
        in_flight.handled();
        assert_eq!(readiness(&monitor, &in_flight).blocker(TOUCH), None);
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
