//! The power feature: the power policy.
//!
//! - On the stand, the device stays awake and connected.
//! - On battery, the device connects when it wakes. After [`IDLE_TIMEOUT`]
//!   without a touch, or at [`crate::Event::SleepNow`], it sleeps in steps. Each
//!   step waits for the answer of the shell: stop a scan for a better
//!   access point if one runs, publish `asleep` and disconnect, then turn
//!   the radio off, then draw the last image with the time of the last
//!   state message and sleep until a touch.
//! - The charger cannot wake the chip. So each `Sleep` request also has a
//!   timer of [`CHECK_PERIOD`]. At the timer wake, the shell reads the
//!   charger, and the core sends a new `Sleep` request at once. If the shell
//!   sees the stand, it sends `PowerSourceChanged` first, and the device
//!   wakes.
//! - After a wake with no cause ([`Wake::Other`]), the device stays awake
//!   for [`STRAY_HOLD`], then sends a new `Sleep` request. In that time, a
//!   tap or the stand wakes the device.
//! - Once an hour ([`REPORT_PERIOD`]), the device reports to Home
//!   Assistant while it stays asleep for the user ([`report`]). The core
//!   counts the checks of the sleep, and the check with the number
//!   [`REPORT_CHECKS`] starts the report. The radio turns on, the device
//!   connects with the status `reporting`, and the device publish queue
//!   sends the state message and the lines of the log. When the queue is
//!   settled, or at [`REPORT_LIMIT`], the steps of a new sleep run. The
//!   last step draws only if the image changed ([`radio_off`]). A tap or
//!   the stand during a report wakes the device as from a sleep.
//! - If the front light is still on at the sleep, one more step turns it
//!   off first. Usually the light timer turned the light of a tap off
//!   already. The light is on at the sleep after a tap on no control, or if
//!   Home Assistant turned it on. Refer to
//!   [`LIGHT_TIMEOUT`](crate::features::light::LIGHT_TIMEOUT) and
//!   [`light`](crate::features::light).
//! - A sleep does not start while a pending mark shows. Refer to
//!   [`crate::features::pending`]. At the end of the idle time, marks can be
//!   open while no service call waits in the outbox. The sleep then waits
//!   until Home Assistant confirms each mark or its timeout ends it. The
//!   mark timeouts are shorter than [`IDLE_TIMEOUT`]. Thus usually only
//!   [`crate::Event::SleepNow`] soon after a tap must wait. The end of the
//!   last mark starts the sleep ([`sleep_if_settled`]), so the wait needs
//!   no timer of its own. The last image then shows the confirmed state. A
//!   tap that starts the idle timer again cancels the sleep that waits. A
//!   move to the stand also cancels it.
//! - The core clears the idle timer on a move to the stand, at a sleep and
//!   at [`crate::Event::SleepNow`]. Each idle timer has an id. If the expiry of an
//!   older timer still arrives, the core ignores it.
//! - Each sleep has an id. A tap or a move to the stand wakes the device
//!   during the steps, and the core then ignores the answers of the steps and
//!   of `Sleep`.
//!
//! The state is one phase: on the stand, awake on battery with the idle
//! timer, awake on battery while a sleep waits for the marks, or asleep
//! with the id of the sleep and where the sleep is ([`Rest`]). A report is
//! a place of a sleep, because the device stays asleep for the user. The power source comes from the phase. Power does not keep
//! the step before the first `Sleep` request. Each step waits in the light,
//! Wi-Fi or connection feature, and [`sleep_step`] reads it from there.
//!
//! # Transitions while asleep
//!
//! "Sleep" is a `Sleep` request with a touch wake and a timer of
//! [`CHECK_PERIOD`], with no `Render` and no line, unless the row tells
//! otherwise. `n` is the number of wakes with no cause, `k` is the number
//! of checks ([`Checks`]), and `c` is a control. A new sleep starts with
//! `n` and `k` at 0.
//!
//! | From | Input | To | Effects |
//! | --- | --- | --- | --- |
//! | `Steps { Draw }` | the radio is off | `Sleeping { 0, 0 }` | `Render` (full), Sleep |
//! | `Steps { IfChanged }` | the radio is off, and the image changed | `Sleeping { 0, 0 }` | `Render` (partial, or full after the last partial), Sleep |
//! | `Steps { IfChanged }` | the radio is off, and only the time or nothing changed | `Sleeping { 0, 0 }` | Sleep |
//! | `Sleeping` | `Wake::Touch(c)` | `Awake` | wake, connect, act on `c` |
//! | `Sleeping { n, k }` | `Wake::Timer` (the check), before the last check | `Sleeping { n, k + 1 }` | Sleep |
//! | `Sleeping { n, k }` | `Wake::Timer`, the check with the number [`REPORT_CHECKS`] | `Reporting` | the limit timer, connect, a Debug line, the Info line with `n` if `n > 1` |
//! | `Sleeping { n, k }` | `Wake::Other` | `Held { n + 1, k }` | a timer of [`STRAY_HOLD`], an Info line for the first one |
//! | `Sleeping` | `Wake::PowerSource` | `Sleeping` (the same `k`) | a warning, Sleep |
//! | `Held { n, k }` | the hold timer expires | `Sleeping { n, k }` | Sleep |
//! | `Held` | `Tapped(c)` | `Awake` | clear the hold timer, wake, connect, act on `c` |
//! | `Held` or `Sleeping` | the stand | `Stand` | clear the hold timer, connect |
//! | `Reporting` | the session is up | `Reporting` | publish `reporting`, subscribe, then the device publish queue |
//! | `Reporting` | the subscribe ends | `Reporting` | PINGREQ (the replay fence), no scan |
//! | `Reporting` | a failed try to connect | `Reporting` | the usual retry with Debug lines, no count of reconnects or of joins for a restart |
//! | `Reporting` | the queue is settled and the PINGRESP came | `Steps { IfChanged }` of a new sleep | clear the limit timer, a Debug line, the steps |
//! | `Reporting` | the limit timer expires | `Steps { IfChanged }` of a new sleep | an Info line with what was not done, the steps |
//! | `Reporting` | `Tapped(c)` | `Awake` | clear the limit timer, wake, publish `online` on the session, act on `c` |
//! | `Reporting` | the stand | `Stand` | clear the limit timer, publish `online` on the session |
//! | any | `Start` | `Awake` or `Stand` | clear the hold timer or the limit timer |
//!
//! A wake (touch, tap, stand or `Start`) after more than one wake with no
//! cause writes one Info line with the count. A report writes that line
//! too, so a wake from a report writes none. The core ignores the answer or
//! the timer of an older sleep, and an answer in `Steps`, `Held` or
//! `Reporting`. `BatteryChanged`, `PowerRead` and `SleepNow` do not change
//! the sleep. In a report, the core ignores a light command and the Sleep
//! now and Full refresh buttons of Home Assistant. The Restart button
//! restarts the chip.
//!
//! # The wake reason
//!
//! The core keeps what woke the device last ([`WakeReason`]) and publishes
//! it in the state message. `Start` sets `Boot`. A touch wake, or a tap in
//! the steps of a sleep or in a hold, sets `Touch` ([`wake_up`]). A move to
//! the stand while the device is asleep or in a report sets `Stand`. The
//! checks of [`CHECK_PERIOD`], the reports and the wakes with no cause keep
//! the device asleep, so they do not change it.
//!
//! # Lines of the log
//!
//! At the start of each step, the core writes a `Debug` line through the
//! log capability ([`crate::capabilities::log`]), with the area `power`.
//! The line comes before the effects of the step. The core does not keep a
//! `Debug` line, so a usual sleep sends no line to Home Assistant. A sleep
//! that waits for the pending marks also writes a `Debug` line. A report
//! writes `Debug` lines, also for each try to connect that fails. Only
//! unusual decisions write an Info or Warn line: a stalled sleep, a wake
//! with no cause, a power source wake on battery, and a report that stops
//! at its limit. Thus a report that fails each hour keeps one line in the
//! ring, and the ring holds about two days of such reports.

use alloc::{format, string::String, vec};
use core::{mem, num::NonZeroU32, time::Duration};

use crate::{
    Cmd, Model,
    capabilities::{
        log::Area,
        power::{Power, PowerSource, Reading, Wake, WakeSource},
    },
    features::{
        connection::{self, Link},
        device, light, logs,
        refresh::{image_changed, redraw},
        rooms,
        timer::{Running, Slot},
        wifi,
    },
    view::rounded_level,
};

/// The internal events of the power feature. The core makes them from its
/// timers and from the answers to its requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The idle timer with this id expired.
    IdleTimeout(u32),
    /// The front light is off for the sleep with this id. Refer to
    /// [`light_off`].
    LightOffForSleep(u32),
    /// The stream of the MQTT connection closed for the sleep with this id,
    /// or no stream was open. Refer to [`connection::go_offline`].
    Offline(u32),
    /// The hold timer of the sleep with this id expired. Refer to
    /// [`STRAY_HOLD`].
    HoldEnded(u32),
    /// The `Sleep` request of the sleep with this id ended.
    Woke {
        /// The id of the sleep. The core ignores the answer of an older
        /// sleep.
        sleep: u32,
        /// What ended the sleep.
        wake: Wake,
    },
    /// The device publish queue of the report of the sleep with this id is
    /// settled. Refer to [`reported`].
    Reported(u32),
    /// The limit timer of the report of the sleep with this id expired.
    /// Refer to [`REPORT_LIMIT`].
    ReportLimit(u32),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Power(event)
    }
}

/// Handles an internal event of the power feature.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::IdleTimeout(id) => idle_timeout(id, model),
        Event::LightOffForSleep(id) => light_off(id, model),
        Event::Offline(id) => offline(id, model),
        Event::HoldEnded(id) => hold_ended(id, model),
        Event::Woke { sleep, wake } => woke(sleep, wake, model),
        Event::Reported(id) => reported(id, model),
        Event::ReportLimit(id) => report_limit(id, model),
    }
}

/// On battery, the time without a touch after which the device sleeps.
///
/// To end the idle time early, a shell sends
/// [`Event::SleepNow`](crate::Event::SleepNow).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// While asleep, the time between two checks of the power source.
///
/// Each `Sleep` request has a timer of this period, because the charger
/// cannot wake the chip. A timer of the core cannot wake the chip either,
/// because the sleep stops the timers of the shell.
pub const CHECK_PERIOD: Duration = Duration::from_secs(60);

/// After a wake with no cause, the time that the device stays awake before
/// it sleeps again.
pub const STRAY_HOLD: Duration = Duration::from_secs(3);

/// While asleep on battery, the time between two reports to Home
/// Assistant. Refer to [`report`].
///
/// The core counts the checks of [`CHECK_PERIOD`], so the period must be a
/// whole number of checks. The clock of the chip drifts in light sleep, and
/// each check and each wake with no cause adds a little time. Thus the real
/// period is a minute or two longer.
pub const REPORT_PERIOD: Duration = Duration::from_mins(60);

/// The number of checks in one [`REPORT_PERIOD`]. The check with this
/// number reports.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the assert makes sure that the count is in the range of u8"
)]
pub const REPORT_CHECKS: u8 = {
    let period = REPORT_PERIOD.as_secs();
    let check = CHECK_PERIOD.as_secs();
    assert!(
        period.is_multiple_of(check),
        "the period is a whole number of checks"
    );
    let count = period / check;
    assert!(count > 0 && count <= u8::MAX as u64, "the count is in u8");
    count as u8
};

/// The longest time of a report. At this time, the report stops, and the
/// device sleeps again. Refer to [`report_limit`].
pub const REPORT_LIMIT: Duration = Duration::from_secs(30);

/// A reading with no values, before the shell reads the diagnostics.
#[cfg(test)]
const NO_READING: Reading = Reading {
    charge: None,
    voltage_mv: None,
    current_ma: None,
    tenth_celsius: None,
};

/// The power source and the sleep state.
#[derive(Debug, Default)]
pub struct State {
    /// The exact charge of the battery in percent, or `None` if the shell
    /// did not read it yet.
    pub battery: Option<u8>,
    /// The last reading of the power diagnostics. Each value is `None`
    /// until the shell reads it.
    pub reading: Reading,
    /// The power phase. It also gives the power source.
    phase: Phase,
    /// The last id that the core took for an idle timer. Each new idle timer
    /// takes the next id. The `Slot` of the phase decides if an expiry is
    /// stale.
    pub idle: u32,
    /// The id of the newest sleep. The core takes a new id for each sleep,
    /// and ignores the answers of an older one.
    pub sleep: u32,
    /// What woke the device last. Refer to [`WakeReason`].
    wake: WakeReason,
}

/// What woke the device last. The core publishes it in the state message.
/// Refer to [`device`].
///
/// Only a wake of the device changes it. The check of the power source
/// after [`CHECK_PERIOD`] and a wake with no cause keep the device asleep,
/// so they do not change it. A move to the stand while the device is awake
/// does not change it either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WakeReason {
    /// The chip started (`Start`), and the device did not sleep since.
    #[default]
    Boot,
    /// A touch woke the device: a touch wake of the `Sleep` request, or a
    /// tap in the steps of a sleep or after a wake with no cause.
    Touch,
    /// A move to the stand woke the device.
    Stand,
}

/// The power phase. Each phase holds only the data that is valid in it.
#[derive(Debug, Default)]
enum Phase {
    /// The device is on the stand. It stays awake, and no idle timer runs.
    ///
    /// A move off the stand makes the phase `Awake` and starts the idle
    /// timer.
    #[default]
    Stand,
    /// The device is on battery and awake.
    ///
    /// The idle timer runs, except before `Start` after it expired. The end
    /// of the idle time makes the phase `Asleep` ([`sleep`]), or
    /// `SleepWaits` while pending marks are open ([`idle_ended`]). A move to
    /// the stand makes the phase `Stand`. Each change out of this phase
    /// clears the idle timer ([`State::enter`]).
    Awake {
        /// The idle timer.
        idle: Slot,
        /// The core waits one more idle period, because service calls wait
        /// in the outbox. The next end of the idle time sleeps.
        extended: bool,
    },
    /// The device is on battery and awake, and a sleep waits for the end of
    /// each pending mark. No idle timer runs, and no service call waits in
    /// the outbox.
    ///
    /// The end of the last mark starts the sleep ([`sleep_if_settled`]). A
    /// tap that starts the idle timer again makes the phase `Awake`
    /// ([`restart_idle_timer`]). A move to the stand makes the phase
    /// `Stand`.
    SleepWaits,
    /// The device is on battery, in the sleep `id`. Refer to [`Rest`] for
    /// where the sleep is, and to [`SleepStep`] for the step.
    ///
    /// A tap, a touch wake from the `Sleep` request or `Start` makes the
    /// phase `Awake`. A move to the stand makes the phase `Stand`. The core
    /// then ignores the answers and the timers of the sleep `id`.
    Asleep {
        /// The id of the sleep.
        id: u32,
        /// Where the sleep is.
        rest: Rest,
    },
}

/// Where a sleep is. Refer to the table in the [module docs](self).
#[derive(Debug)]
enum Rest {
    /// The steps before the first `Sleep` request run: light off, stop the
    /// scan, disconnect, radio off. [`sleep_step`] reads the step from the
    /// other features.
    Steps {
        /// What the last step draws before the first `Sleep` request.
        last: LastImage,
    },
    /// A `Sleep` request waits for its answer.
    Sleeping {
        /// The number of wakes with no cause in this sleep.
        strays: u32,
        /// The number of checks in this sleep.
        checks: Checks,
    },
    /// The device woke with no cause. It stays awake until the hold timer
    /// expires, then it sends a new `Sleep` request. No `Sleep` request
    /// waits.
    Held {
        /// The hold timer. Its id is the id of the sleep.
        hold: Running,
        /// The number of wakes with no cause in this sleep, this one
        /// included.
        strays: NonZeroU32,
        /// The number of checks in this sleep.
        checks: Checks,
    },
    /// The hourly report: the radio is on, and the device publishes to Home
    /// Assistant. No `Sleep` request waits. Refer to [`report`].
    ///
    /// The report ends the sleep, so it keeps no count of wakes with no
    /// cause and no count of checks.
    Reporting {
        /// The limit timer of [`REPORT_LIMIT`]. Its id is the id of the
        /// sleep.
        limit: Running,
        /// The tries to connect that failed in this report, or `None`.
        failures: Option<Failures>,
    },
}

impl Rest {
    /// Returns the number of wakes with no cause in the sleep.
    const fn strays(&self) -> u32 {
        match self {
            Self::Steps { .. } | Self::Reporting { .. } => 0,
            Self::Sleeping { strays, .. } => *strays,
            Self::Held { strays, .. } => strays.get(),
        }
    }
}

/// What the steps of a sleep draw before the first `Sleep` request. Refer
/// to [`radio_off`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastImage {
    /// A full refresh with the last image: a sleep after the idle time.
    Draw,
    /// The sleep after a report: a partial `Render` only if the view, with
    /// the time left out, is not the image on the panel
    /// ([`image_changed`]). Otherwise the panel keeps its image.
    IfChanged,
}

/// The number of checks in a sleep. It is always less than
/// [`REPORT_CHECKS`], because the check with that number reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Checks(u8);

/// The result of one more check. Refer to [`Checks::tick`].
enum Tick {
    /// The sleep continues with this count.
    Again(Checks),
    /// The check is the check with the number [`REPORT_CHECKS`].
    Report,
}

impl Checks {
    /// Counts one more check. Returns `Report` at the check with the number
    /// [`REPORT_CHECKS`].
    const fn tick(self) -> Tick {
        let next = self.0 + 1;
        if next >= REPORT_CHECKS {
            Tick::Report
        } else {
            Tick::Again(Self(next))
        }
    }
}

/// The tries to connect that failed in a report. Refer to [`report_failed`].
#[derive(Debug)]
struct Failures {
    /// The number of tries that failed.
    tries: NonZeroU32,
    /// Why the last try failed.
    last: String,
}

impl State {
    /// Returns the power source. The device is on battery while it is
    /// awake on battery or asleep.
    #[must_use]
    pub const fn source(&self) -> PowerSource {
        match self.phase {
            Phase::Stand => PowerSource::Stand,
            Phase::Awake { .. } | Phase::SleepWaits | Phase::Asleep { .. } => PowerSource::Battery,
        }
    }

    /// Returns what woke the device last.
    #[must_use]
    pub const fn wake(&self) -> WakeReason {
        self.wake
    }

    /// Returns `true` if the device is awake on battery, and a sleep waits
    /// for the end of each pending mark.
    #[must_use]
    pub const fn sleep_waits(&self) -> bool {
        matches!(self.phase, Phase::SleepWaits)
    }

    /// Returns `true` if the device is asleep, or in the steps before the
    /// sleep, or in a report. The device then does not draw, a tap wakes it,
    /// and the core keeps the readings.
    #[must_use]
    pub const fn is_asleep(&self) -> bool {
        matches!(self.phase, Phase::Asleep { .. })
    }

    /// Returns the id of the sleep whose report runs, or `None` if no report
    /// runs. Refer to [`report`].
    #[must_use]
    pub const fn reporting(&self) -> Option<u32> {
        match self.phase {
            Phase::Asleep {
                id,
                rest: Rest::Reporting { .. },
            } => Some(id),
            _ => None,
        }
    }

    /// Returns `true` if a report runs. Refer to [`report`].
    #[must_use]
    pub const fn is_reporting(&self) -> bool {
        self.reporting().is_some()
    }

    /// Returns `true` if the device is in the steps of the sleep `id`. The
    /// core ignores the answers of the steps of an older sleep, and of a
    /// sleep that a wake ended.
    const fn in_steps(&self, id: u32) -> bool {
        matches!(self.phase, Phase::Asleep { id: current, rest: Rest::Steps { .. } } if current == id)
    }

    /// Returns the state of a device on battery in the steps of the sleep
    /// `sleep`, for tests that start in the middle of a sleep.
    #[cfg(test)]
    pub(crate) const fn asleep_in(sleep: u32) -> Self {
        Self {
            battery: None,
            reading: NO_READING,
            phase: Phase::Asleep {
                id: sleep,
                rest: Rest::Steps {
                    last: LastImage::Draw,
                },
            },
            idle: 0,
            sleep,
            wake: WakeReason::Boot,
        }
    }

    /// Returns the state of a device on battery whose `Sleep` request of
    /// the sleep `sleep` waits, for tests of the wakes.
    #[cfg(test)]
    pub(crate) const fn sleeping_in(sleep: u32) -> Self {
        Self {
            battery: None,
            reading: NO_READING,
            phase: Phase::Asleep {
                id: sleep,
                rest: Rest::Sleeping {
                    strays: 0,
                    checks: Checks(0),
                },
            },
            idle: 0,
            sleep,
            wake: WakeReason::Boot,
        }
    }

    /// Wakes the device on battery with no idle timer, for tests that must
    /// not draw at the wake.
    #[cfg(test)]
    pub(crate) fn wake_with_no_draw(&mut self) {
        self.enter(awake_on(PowerSource::Battery));
    }

    /// Changes the phase to `next`. If the old phase is `Awake`, the core
    /// clears its idle timer. If the old phase holds the device awake after
    /// a wake with no cause, the core clears the hold timer. If the old
    /// phase is a report, the core clears the limit timer.
    fn enter(&mut self, next: Phase) {
        match mem::replace(&mut self.phase, next) {
            Phase::Awake { mut idle, .. } => idle.stop(),
            Phase::Asleep {
                rest: Rest::Held { hold, .. },
                ..
            } => hold.clear(),
            Phase::Asleep {
                rest: Rest::Reporting { limit, .. },
                ..
            } => limit.clear(),
            Phase::Stand | Phase::SleepWaits | Phase::Asleep { .. } => {}
        }
    }

    /// Changes the phase to `next`, which is awake. Returns the Info line
    /// with the number of wakes with no cause in the sleep that ends, if
    /// there was more than one.
    fn wake_to(&mut self, next: Phase) -> Cmd {
        let summary = match &self.phase {
            Phase::Asleep { id, rest } if rest.strays() > 1 => logs::info(
                Area::Power,
                format!("sleep {id} had {} wakes with no cause", rest.strays()),
            ),
            _ => Cmd::done(),
        };
        self.enter(next);
        summary
    }
}

/// A step of a sleep. Refer to [`sleep_step`].
///
/// Power does not keep the step. Each step waits in another feature, and
/// [`sleep_step`] reads the step from there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepStep {
    /// The `FrontLight` request turns the front light off. The answer starts
    /// the next step ([`light_off`]).
    LightOff,
    /// A `StopScan` request waits in the Wi-Fi feature, and the DISCONNECT
    /// packet waits for its answer. The answer starts the next step
    /// ([`scan_stopped`]).
    StopScan,
    /// The connection publishes `asleep` and disconnects. The close of the
    /// stream starts the next step ([`offline`]).
    Disconnect,
    /// An `Off` request waits in the Wi-Fi feature. The answer starts the
    /// next step ([`radio_off`]).
    RadioOff,
    /// The `Sleep` request waits. Only a wake ends this step ([`woke`]).
    Sleep,
    /// The device woke with no cause. It stays awake until the hold timer
    /// expires, then it sends a new `Sleep` request ([`hold_ended`]).
    Held,
    /// The chip restarts, or a restart waits for the close of the
    /// connection, so the radio does not turn off. No step of the sleep
    /// waits, and the core stays asleep until a wake or `Start`.
    Stalled,
}

/// Returns the step of the sleep, or `None` while the device is awake or
/// in a report. A report is not a step of a sleep: it ends the sleep, and
/// its end starts the steps of a new sleep.
///
/// After the first `Sleep` request, the step is `Sleep` while a request
/// waits, and `Held` while the device stays awake after a wake with no
/// cause. Before it, the core keeps no copy of the step. The function reads
/// the step from the feature in which it waits, in this order:
///
/// 1. If the light is on, the step is `LightOff`. This occurs only in
///    [`sleep`], before the `FrontLight` request.
/// 2. If the Wi-Fi phase is `TurningOff` for this sleep, the step is
///    `RadioOff`.
/// 3. If the Wi-Fi phase is `StoppingScan` for this sleep, the step is
///    `StopScan`.
/// 4. If the stream closes for this sleep, the step is `Disconnect`.
/// 5. If the stream closes for a restart, or the core asked for a restart
///    of the chip, the step is `Stalled`.
/// 6. If the radio is off and no stream is open, the step is `Sleep`.
/// 7. Else, the step is `LightOff`.
///
/// The light-off step has no state to read, because it is a command chain.
/// The light feature sets the level to 0 when it sends the `FrontLight`
/// request. Thus the function gives `LightOff` when no other step matches.
/// While the chip restarts, the light-off step reads as `Stalled`. With no
/// stream, the disconnect step sends [`Event::Offline`] at once. Until the
/// core handles that event, the function gives `LightOff`, or `Sleep` if the
/// radio is already off. The shell never sees that state.
#[must_use]
pub fn sleep_step(model: &Model) -> Option<SleepStep> {
    let Phase::Asleep { id, ref rest } = model.power.phase else {
        return None;
    };
    match rest {
        Rest::Steps { .. } => {}
        Rest::Sleeping { .. } => return Some(SleepStep::Sleep),
        Rest::Held { .. } => return Some(SleepStep::Held),
        Rest::Reporting { .. } => return None,
    }
    if light::is_on(model) {
        return Some(SleepStep::LightOff);
    }
    let step = match model.wifi.phase {
        wifi::Phase::TurningOff { sleep, .. } if sleep == id => SleepStep::RadioOff,
        wifi::Phase::StoppingScan { sleep, .. } if sleep == id => SleepStep::StopScan,
        _ if model.connection.sleep_waiter() == Some(id) => SleepStep::Disconnect,
        _ if model.connection.restart_waits() => SleepStep::Stalled,
        wifi::Phase::Restarting => SleepStep::Stalled,
        wifi::Phase::Off if !model.connection.has_stream() => SleepStep::Sleep,
        _ => SleepStep::LightOff,
    };
    Some(step)
}

/// Writes the line of the log for the start of `step` of the sleep `id`.
/// The line comes before the effects of the step. A usual step writes a
/// `Debug` line. Only a stalled sleep writes a warning.
fn step_started(step: SleepStep, id: u32) -> Cmd {
    let line = |message: &str| format!("sleep {id} {message}");
    match step {
        SleepStep::LightOff => logs::debug(Area::Power, line("turns the front light off")),
        SleepStep::StopScan => logs::debug(Area::Power, line("waits for the end of the scan")),
        SleepStep::Disconnect => logs::debug(Area::Power, line("closes the connection")),
        SleepStep::RadioOff => logs::debug(Area::Power, line("turns the radio off")),
        SleepStep::Sleep => logs::debug(Area::Power, line("draws the last image and sleeps")),
        SleepStep::Held => Cmd::done(),
        SleepStep::Stalled => logs::warn(
            Area::Power,
            line("stops before the radio turns off, because the chip restarts"),
        ),
    }
}

/// The phase for a device that wakes, or starts, on `source`. On battery,
/// no idle timer runs yet.
const fn awake_on(source: PowerSource) -> Phase {
    match source {
        PowerSource::Stand => Phase::Stand,
        PowerSource::Battery => Phase::Awake {
            idle: Slot::new(),
            extended: false,
        },
    }
}

/// Sets the power phase for `Start` on `source`. The device is awake.
///
/// The core clears the idle timer or the hold timer that runs, and ignores
/// the answers of a sleep that `Start` ended. The wake reason is
/// [`WakeReason::Boot`]. Returns the line with the number of wakes with no
/// cause in that sleep, if there was more than one. The caller then starts
/// the idle timer with [`restart_idle_timer`].
pub fn reset(source: PowerSource, model: &mut Model) -> Cmd {
    model.power.wake = WakeReason::Boot;
    model.power.wake_to(awake_on(source))
}

/// The `Sleep` request of the sleep `id` ended with `wake`. Refer to the
/// table in the [module docs](self).
///
/// - A touch wakes the device, which connects again. The touch also acts
///   as a tap on its control.
/// - The timer is the check of the power source. The device stays asleep
///   and sends a new `Sleep` request. The check with the number
///   [`REPORT_CHECKS`] in the sleep starts a report instead ([`report`]).
/// - After a wake with no cause, the device stays awake for
///   [`STRAY_HOLD`]. Only the first one in a sleep writes a line.
/// - A change of the power source comes first as `PowerSourceChanged`, and
///   a move to the stand wakes the device. So `Wake::PowerSource` while the
///   device sleeps on battery is not expected. The core writes a warning
///   and sends a new `Sleep` request.
///
/// The core ignores the answer if no `Sleep` request of the sleep `id`
/// waits.
pub fn woke(id: u32, wake: Wake, model: &mut Model) -> Cmd {
    let Phase::Asleep {
        id: current,
        rest: Rest::Sleeping { strays, checks },
    } = model.power.phase
    else {
        return Cmd::done();
    };
    if current != id {
        return Cmd::done();
    }
    match wake {
        Wake::Touch(control) => rooms::tapped(control, model),
        Wake::Timer => match checks.tick() {
            Tick::Again(checks) => {
                model.power.phase = Phase::Asleep {
                    id,
                    rest: Rest::Sleeping { strays, checks },
                };
                sleep_request(id)
            }
            Tick::Report => report(id, strays, model),
        },
        Wake::Other => {
            let (timer, hold) = Running::start(id, STRAY_HOLD, Event::HoldEnded(id).into());
            let strays = NonZeroU32::MIN.saturating_add(strays);
            model.power.phase = Phase::Asleep {
                id,
                rest: Rest::Held {
                    hold,
                    strays,
                    checks,
                },
            };
            let line = if strays == NonZeroU32::MIN {
                logs::info(
                    Area::Power,
                    format!(
                        "sleep {id} woke with no cause, so it sleeps again after {} s",
                        STRAY_HOLD.as_secs()
                    ),
                )
            } else {
                Cmd::done()
            };
            line.and(timer)
        }
        Wake::PowerSource => logs::warn(
            Area::Power,
            format!("sleep {id} got a power source wake on battery, so it sleeps again"),
        )
        .and(sleep_request(id)),
    }
}

/// The hold timer of the sleep `id` expired: send a new `Sleep` request.
/// The core ignores a timer of an older sleep, and a timer that a wake
/// ended.
pub fn hold_ended(id: u32, model: &mut Model) -> Cmd {
    let Phase::Asleep {
        id: current,
        rest: Rest::Held {
            ref hold,
            strays,
            checks,
        },
    } = model.power.phase
    else {
        return Cmd::done();
    };
    if current != id || hold.id() != id {
        return Cmd::done();
    }
    // The timer completed, so the core does not clear it.
    model.power.phase = Phase::Asleep {
        id,
        rest: Rest::Sleeping {
            strays: strays.get(),
            checks,
        },
    };
    sleep_request(id)
}

/// The check with the number [`REPORT_CHECKS`] in the sleep `id` arrived:
/// start the report to Home Assistant. `strays` is the number of wakes with
/// no cause in the sleep.
///
/// The radio turns on, and the core connects as for a wake. The status is
/// `reporting`, not `online`. The device publish queue publishes the
/// discovery message if it is due, the state of the light, the state
/// message and the lines of the log. The device stays asleep for the user:
/// it does not draw, it does not turn the light on, and it does not scan
/// for a better access point. A tap or the stand wakes it.
///
/// The limit timer of [`REPORT_LIMIT`] starts. The report ends when the
/// queue is settled ([`reported`]), or at the limit ([`report_limit`]).
/// If the sleep had more than one wake with no cause, the summary line goes
/// with this report.
fn report(id: u32, strays: u32, model: &mut Model) -> Cmd {
    let (timer, limit) = Running::start(id, REPORT_LIMIT, Event::ReportLimit(id).into());
    // The `Sleep` request ended, so no timer of the sleep runs.
    model.power.phase = Phase::Asleep {
        id,
        rest: Rest::Reporting {
            limit,
            failures: None,
        },
    };
    let summary = if strays > 1 {
        logs::info(
            Area::Power,
            format!("sleep {id} had {strays} wakes with no cause"),
        )
    } else {
        Cmd::done()
    };
    connection::reset_backoff(model);
    summary
        .and(logs::debug(
            Area::Power,
            format!("sleep {id} reports to Home Assistant"),
        ))
        .and(timer)
        .and(connection::go_online(model))
}

/// A try to connect in the report failed because of `reason`. The core
/// counts the try for the line at the limit ([`report_limit`]). Outside a
/// report, the function does nothing.
///
/// The connection calls this function when it waits to connect again. In a
/// report, the lines of each try are `Debug` lines, so a report that fails
/// each hour writes only its line at the limit to the ring.
pub fn report_failed(reason: &str, model: &mut Model) {
    let Phase::Asleep {
        rest: Rest::Reporting { failures, .. },
        ..
    } = &mut model.power.phase
    else {
        return;
    };
    match failures {
        Some(Failures { tries, last }) => {
            *tries = tries.saturating_add(1);
            *last = reason.into();
        }
        None => {
            *failures = Some(Failures {
                tries: NonZeroU32::MIN,
                last: reason.into(),
            });
        }
    }
}

/// The device publish queue of the report of the sleep `id` is settled:
/// end the report, and start the steps of a new sleep. The core clears the
/// limit timer. The steps draw the image only if it changed
/// ([`LastImage::IfChanged`]).
///
/// The core ignores the event if no report of the sleep `id` runs. The
/// connection can send it more than once.
fn reported(id: u32, model: &mut Model) -> Cmd {
    if model.power.reporting() != Some(id) {
        return Cmd::done();
    }
    logs::debug(Area::Power, format!("the report of sleep {id} is complete"))
        .and(sleep(LastImage::IfChanged, model))
}

/// The limit timer of the report of the sleep `id` expired: stop the
/// report, and start the steps of a new sleep. The steps draw the image only
/// if it changed ([`LastImage::IfChanged`]).
///
/// The core writes one Info line with what was not done. If tries to
/// connect failed, the line gives their number and the last reason. The
/// ring keeps each line that got no PUBACK, and the next report or wake
/// sends it. The core ignores a timer of an older sleep, and a timer that a
/// wake ended.
fn report_limit(id: u32, model: &mut Model) -> Cmd {
    let matches = matches!(
        &model.power.phase,
        Phase::Asleep { id: current, rest: Rest::Reporting { limit, .. } }
            if *current == id && limit.id() == id
    );
    if !matches {
        return Cmd::done();
    }
    // The timer completed, so the core does not clear it.
    let old = mem::replace(
        &mut model.power.phase,
        Phase::Asleep {
            id,
            rest: Rest::Steps {
                last: LastImage::IfChanged,
            },
        },
    );
    let Phase::Asleep {
        rest: Rest::Reporting { failures, .. },
        ..
    } = old
    else {
        return Cmd::done();
    };
    let left = match failures {
        Some(Failures { tries, last }) => {
            let tries = match tries.get() {
                1 => String::from("1 try failed"),
                count => format!("{count} tries failed"),
            };
            format!("{tries}, the last because {last}")
        }
        None if model.connection.link() == Link::Up => {
            String::from("the publishes or the retained messages did not end")
        }
        None => String::from("no connection to the broker"),
    };
    logs::info(
        Area::Power,
        format!(
            "the report of sleep {id} stopped after {} s: {left}",
            REPORT_LIMIT.as_secs()
        ),
    )
    .and(sleep(LastImage::IfChanged, model))
}

/// Asks the shell to sleep until a touch, or until the check of the power
/// source after [`CHECK_PERIOD`]. The answer is [`Event::Woke`] for the
/// sleep `id`.
fn sleep_request(id: u32) -> Cmd {
    let check = WakeSource::Timer {
        seconds: u32::try_from(CHECK_PERIOD.as_secs()).unwrap_or(u32::MAX),
    };
    Power::sleep(vec![WakeSource::Touch, check])
        .then_send(move |wake| Event::Woke { sleep: id, wake }.into())
}

/// The stream of the MQTT connection of the sleep `id` closed: turn the
/// radio off. Refer to [`wifi::off`].
pub fn offline(id: u32, model: &mut Model) -> Cmd {
    if !model.power.in_steps(id) {
        return Cmd::done();
    }
    // After the core asked for a restart, `wifi::off` does nothing.
    if model.wifi.phase == wifi::Phase::Restarting {
        return step_started(SleepStep::Stalled, id);
    }
    step_started(SleepStep::RadioOff, id).and(wifi::off(id, model))
}

/// The radio of the sleep `id` is off: draw the last image and send the
/// first `Sleep` request. The Wi-Fi feature calls this function. Refer to
/// [`wifi::radio_off`].
///
/// After the idle time, the last image is a full refresh. After a report,
/// the core draws only if the view, with the time left out, is not the
/// image on the panel ([`image_changed`]). The draw is then a partial
/// refresh, or a full refresh after
/// [`PARTIALS_PER_FULL`](crate::features::refresh::PARTIALS_PER_FULL)
/// partial refreshes. With no change, the panel keeps its image and the
/// core sends no `Render`.
pub fn radio_off(id: u32, model: &mut Model) -> Cmd {
    let Phase::Asleep {
        id: current,
        rest: Rest::Steps { last },
    } = model.power.phase
    else {
        return Cmd::done();
    };
    if current != id {
        return Cmd::done();
    }
    model.power.phase = Phase::Asleep {
        id,
        rest: Rest::Sleeping {
            strays: 0,
            checks: Checks::default(),
        },
    };
    let image = match last {
        LastImage::Draw => step_started(SleepStep::Sleep, id).and(redraw(model, true)),
        LastImage::IfChanged if image_changed(model) => logs::debug(
            Area::Power,
            format!("sleep {id} draws the changes and sleeps"),
        )
        .and(redraw(model, false)),
        LastImage::IfChanged => logs::debug(
            Area::Power,
            format!("sleep {id} keeps the image and sleeps"),
        ),
    };
    image.and(sleep_request(id))
}

/// Wakes the device from a sleep, or from the steps before it, after a
/// touch, and connects again. The wake reason is [`WakeReason::Touch`].
///
/// The caller makes sure that the device is asleep. If the device stays
/// awake after a wake with no cause, the core clears the hold timer.
pub fn wake_up(model: &mut Model) -> Cmd {
    debug_assert!(
        model.power.is_asleep(),
        "only a device that sleeps can wake"
    );
    model.power.wake = WakeReason::Touch;
    let summary = model.power.wake_to(awake_on(PowerSource::Battery));
    connection::reset_backoff(model);
    summary
        .and(redraw(model, true))
        .and(connection::go_online(model))
        .and(restart_idle_timer(model))
}

/// The device moved to the stand or off the stand.
///
/// If the device was asleep, the move to the stand wakes it, and the wake
/// reason is [`WakeReason::Stand`].
///
/// The core publishes the new power source in the state message. On the
/// stand, [`connection::go_online`] publishes it, or the next connection
/// does. Refer to [`device::changed`].
pub fn source_changed(source: PowerSource, model: &mut Model) -> Cmd {
    if source == model.power.source() {
        return Cmd::done();
    }
    match source {
        PowerSource::Stand => {
            let was_asleep = model.power.is_asleep();
            if was_asleep {
                model.power.wake = WakeReason::Stand;
            }
            // The device stays awake on the stand, so it needs no idle
            // timer. `enter` clears the idle timer or the hold timer that
            // runs.
            let summary = model.power.wake_to(Phase::Stand);
            connection::reset_backoff(model);
            summary
                .and(light::source_changed(model))
                .and(redraw(model, was_asleep))
                .and(connection::go_online(model))
        }
        PowerSource::Battery => {
            model.power.enter(awake_on(PowerSource::Battery));
            light::source_changed(model)
                .and(redraw(model, false))
                .and(restart_idle_timer(model))
                .and(device::changed(model))
        }
    }
}

/// The charge of the battery changed. The core keeps the exact level and
/// draws the level of the view.
///
/// The view gives the level rounded to the nearest
/// [`BATTERY_STEP`](crate::view::BATTERY_STEP). If the rounded level does
/// not change, the core does not draw. Thus each change of 1% does not use
/// a partial refresh.
///
/// The level is a fact, not a touch, so it does not wake the device and it
/// does not start the idle timer again. While the device sleeps, the core
/// keeps the level but does not draw, as for a change of the dry run. The
/// wake draws the level.
///
/// The core publishes each change of the exact level in the state message,
/// while the device is awake. A move of more than
/// [`BATTERY_FLAP`](device::BATTERY_FLAP) goes at once, and a smaller move
/// goes at most once in
/// [`STATE_MIN_INTERVAL`](device::STATE_MIN_INTERVAL). Refer to
/// [`device::changed`]. A reading with the same level does nothing. While
/// the device sleeps, the next connect publishes the level.
pub fn battery_changed(level: Option<u8>, model: &mut Model) -> Cmd {
    let before = model.power.battery;
    model.power.battery = level;
    if level == before || model.power.is_asleep() {
        return Cmd::done();
    }
    let publish = device::changed(model);
    if level.map(rounded_level) == before.map(rounded_level) {
        return publish;
    }
    redraw(model, false).and(publish)
}

/// The shell read the power diagnostics. The core keeps the reading.
///
/// The view does not show the reading, so the core does not draw. The
/// reading is a fact, not a touch, so it does not wake the device and it
/// does not start the idle timer again. While the device sleeps, the core
/// only keeps the reading, and the next connection publishes it.
///
/// While the device is awake, the core publishes the reading in the state
/// message. Refer to [`device::changed`]. A change of the phase of the
/// charge that starts or ends a charge goes at once. A change of only the
/// other values goes at most once in
/// [`STATE_MIN_INTERVAL`](device::STATE_MIN_INTERVAL).
pub fn read(reading: Reading, model: &mut Model) -> Cmd {
    let before = mem::replace(&mut model.power.reading, reading);
    if reading == before || model.power.is_asleep() {
        return Cmd::done();
    }
    device::changed(model)
}

/// Clears the idle timer that runs, and starts a new one if the device is
/// awake on battery. A new idle period does not wait for the outbox.
///
/// If a sleep waits for the marks, the core cancels that sleep. The device
/// then stays awake for a full idle period, as after each tap.
pub fn restart_idle_timer(model: &mut Model) -> Cmd {
    let state = &mut model.power;
    state.idle = state.idle.wrapping_add(1);
    let id = state.idle;
    if matches!(state.phase, Phase::SleepWaits) {
        state.enter(awake_on(PowerSource::Battery));
    }
    let Phase::Awake { idle, extended } = &mut state.phase else {
        return Cmd::done();
    };
    *extended = false;
    idle.start(id, IDLE_TIMEOUT, Event::IdleTimeout(id).into())
}

/// The idle timer with the id `id` expired. If it is the newest idle timer,
/// the idle time ends ([`idle_ended`]).
pub fn idle_timeout(id: u32, model: &mut Model) -> Cmd {
    let Phase::Awake { idle, .. } = &mut model.power.phase else {
        return Cmd::done();
    };
    if !idle.expired(id) {
        return Cmd::done();
    }
    idle_ended(model).1
}

/// What the end of the idle time did. Refer to [`sleep_now`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleEnd {
    /// The steps of the sleep started.
    Sleeps,
    /// The sleep waits for the end of each pending mark
    /// ([`Phase::SleepWaits`]).
    WaitsForMarks,
    /// Service calls wait in the outbox, so the device stays awake for one
    /// more idle period.
    WaitsForOutbox,
    /// Nothing changed: before `Start`, on the stand, while a sleep waits
    /// for the marks, or while the device sleeps.
    Nothing,
}

/// The user asked to sleep now. The idle time ends at once, as if the idle
/// timer expired ([`idle_ended`]). Returns what the end of the idle time
/// did, so the caller can tell it in the log.
///
/// The core clears the idle timer that runs, also if the event does
/// nothing. Before `Start`, the device then stays `Awake` with no idle
/// timer. If a sleep already waits for the marks, the event does nothing.
pub fn sleep_now(model: &mut Model) -> (IdleEnd, Cmd) {
    if let Phase::Awake { idle, .. } = &mut model.power.phase {
        idle.stop();
    }
    idle_ended(model)
}

/// The idle time ended: sleep.
///
/// Before `Start`, on the stand, while a sleep waits, or while the device
/// sleeps, the core does nothing. Otherwise:
///
/// 1. If service calls wait in the outbox, the core first waits one more
///    idle period for the connection. At the next end of the idle time,
///    the sleep starts and drops the calls that still wait.
/// 2. If no call waits but pending marks are open, the sleep waits for the
///    end of each mark ([`Phase::SleepWaits`]). With no call in the outbox,
///    each mark has only the timeout of its issued call. Thus the wait is at
///    most [`TARGET_TIMEOUT`](crate::features::pending::TARGET_TIMEOUT),
///    and [`DRAW_DELAY`](crate::features::refresh::DRAW_DELAY) after a
///    confirmation.
/// 3. Else, the sleep starts at once.
fn idle_ended(model: &mut Model) -> (IdleEnd, Cmd) {
    if !model.startup.is_started() {
        return (IdleEnd::Nothing, Cmd::done());
    }
    let Phase::Awake { extended, .. } = model.power.phase else {
        return (IdleEnd::Nothing, Cmd::done());
    };
    if !model.connection.outbox.is_empty() {
        if extended {
            return (IdleEnd::Sleeps, sleep(LastImage::Draw, model));
        }
        let timer = restart_idle_timer(model);
        if let Phase::Awake { extended, .. } = &mut model.power.phase {
            *extended = true;
        }
        return (IdleEnd::WaitsForOutbox, timer);
    }
    let open = model.pending.marks.len();
    if open > 0 {
        model.power.enter(Phase::SleepWaits);
        let marks = if open == 1 { "mark" } else { "marks" };
        let line = logs::debug(
            Area::Power,
            format!("the sleep waits for {open} pending {marks}"),
        );
        return (IdleEnd::WaitsForMarks, line);
    }
    (IdleEnd::Sleeps, sleep(LastImage::Draw, model))
}

/// A pending mark ended, or the draw timer expired. If a sleep waits for
/// the marks, and no mark is open and no draw timer waits, the sleep
/// starts.
///
/// After Home Assistant confirms a mark, the draw timer waits for the
/// other messages of the change. Refer to
/// [`redraw_soon`](crate::features::refresh::redraw_soon). The sleep then
/// starts at the expiry of that timer, so the last image shows the whole
/// change. A full refresh clears that timer, so the full refresh starts the
/// sleep instead. The pending and refresh features call this function.
pub fn sleep_if_settled(model: &mut Model) -> Cmd {
    if !matches!(model.power.phase, Phase::SleepWaits)
        || !model.pending.marks.is_empty()
        || model.refresh.draw_waits()
    {
        return Cmd::done();
    }
    sleep(LastImage::Draw, model)
}

/// Starts the steps of a new sleep. If the front light is on, the first
/// step turns it off, and the answer to `FrontLight` starts the next step
/// ([`light_off`]). Otherwise the core starts that step at once. `last`
/// tells what the last step draws ([`radio_off`]).
fn sleep(last: LastImage, model: &mut Model) -> Cmd {
    model.power.sleep = model.power.sleep.wrapping_add(1);
    let id = model.power.sleep;
    model.power.enter(Phase::Asleep {
        id,
        rest: Rest::Steps { last },
    });
    if light::is_on(model) {
        return step_started(SleepStep::LightOff, id)
            .and(light::off(model).then(Cmd::event(Event::LightOffForSleep(id).into())));
    }
    disconnect(id, model)
}

/// The scan for the sleep `id` stopped: publish `asleep` and disconnect.
/// The Wi-Fi feature calls this function. Refer to [`wifi::stop_scan`].
///
/// The core ignores this answer if a tap or a move to the stand woke the
/// device first.
pub fn scan_stopped(id: u32, model: &mut Model) -> Cmd {
    if !model.power.in_steps(id) {
        return Cmd::done();
    }
    disconnect(id, model)
}

/// The front light of the sleep `id` is off: publish `asleep` and
/// disconnect.
///
/// The core ignores this answer if a tap or a move to the stand woke the
/// device first.
pub fn light_off(id: u32, model: &mut Model) -> Cmd {
    if !model.power.in_steps(id) {
        return Cmd::done();
    }
    disconnect(id, model)
}

/// Publishes `asleep` and disconnects. The close of the stream starts the
/// next step ([`offline`]). With no stream, the next step starts at once.
/// Refer to [`connection::go_offline`].
///
/// If a scan for a better access point runs, or its `StopScan` request
/// still waits, the core first waits for the end of the scan. The answer
/// starts this step again ([`scan_stopped`]). A scan can drop the link, so
/// the DISCONNECT packet waits for the end of the scan.
///
/// The core drops the service calls that wait in the outbox, and the view
/// shows how many. The last image shows the time of the last state message.
///
/// If a restart waits for the close of the connection, the restart wins.
/// The step is `Stalled`, and the core writes its warning. The calls in the
/// outbox, the pending marks and the count of dropped calls do not change,
/// because the restart clears them. A wake before the restart then finds
/// them unchanged.
fn disconnect(id: u32, model: &mut Model) -> Cmd {
    if let Some(stop) = wifi::stop_scan(id, model) {
        return step_started(SleepStep::StopScan, id).and(stop);
    }
    let step = if model.connection.restart_waits() {
        SleepStep::Stalled
    } else {
        model.unsent += model.connection.outbox.len();
        SleepStep::Disconnect
    };
    step_started(step, id).and(connection::go_offline(id, model))
}

#[cfg(test)]
mod tests;
