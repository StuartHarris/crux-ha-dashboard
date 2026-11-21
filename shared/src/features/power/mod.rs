//! The power feature: the power policy.
//!
//! - On the stand, the device stays awake and connected.
//! - On battery, the device connects when it wakes. After [`IDLE_TIMEOUT`]
//!   without a touch, or at [`crate::Event::SleepNow`], it sleeps in steps. Each
//!   step waits for the answer of the shell: stop a scan for a better
//!   access point if one runs, publish `asleep` and disconnect, then turn
//!   the radio off, then draw the last image with the time of the last
//!   state message and sleep until a touch.
//! - If the front light is still on at the sleep, one more step turns it
//!   off first. Usually the light timer turned it off already. Refer to
//!   [`LIGHT_TIMEOUT`](crate::features::light::LIGHT_TIMEOUT).
//! - The core clears the idle timer on a move to the stand, at a sleep and
//!   at [`crate::Event::SleepNow`]. Each idle timer has an id. If the expiry of an
//!   older timer still arrives, the core ignores it.
//! - Each sleep has an id. A tap or a move to the stand wakes the device
//!   during the steps, and the core then ignores the answers of the steps and
//!   of `Sleep`.
//!
//! The state is one phase: on the stand, awake on battery with the idle
//! timer, or asleep with the id of the sleep. The power source comes from
//! the phase. Power does not keep the step of a sleep. Each step waits in
//! the light, Wi-Fi or connection feature, and [`sleep_step`] reads it from
//! there.
//!
//! At the start of each step, the core writes a line through the log
//! capability ([`crate::capabilities::log`]), with the area `power`. The
//! line comes before the effects of the step.

use alloc::{format, vec};
use core::{mem, time::Duration};

use crate::{
    Cmd, Model,
    capabilities::{
        log::{self, Area},
        power::{Power, PowerSource, Wake, WakeSource},
    },
    features::{connection, light, refresh::redraw, rooms, timer::Slot, wifi},
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
    /// The `Sleep` request of the sleep with this id ended.
    Woke {
        /// The id of the sleep. The core ignores the answer of an older
        /// sleep.
        sleep: u32,
        /// What ended the sleep.
        wake: Wake,
    },
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
        Event::Woke { sleep, wake } => woke(sleep, wake, model),
    }
}

/// On battery, the time without a touch after which the device sleeps.
///
/// To end the idle time early, a shell sends
/// [`Event::SleepNow`](crate::Event::SleepNow).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The power source and the sleep state.
#[derive(Debug, Default)]
pub struct State {
    /// The exact charge of the battery in percent, or `None` if the shell
    /// did not read it yet.
    pub battery: Option<u8>,
    /// The power phase. It also gives the power source.
    phase: Phase,
    /// The id of the newest idle timer. The core takes a new id for each
    /// idle timer, and ignores the expiry of an older one.
    pub idle: u32,
    /// The id of the newest sleep. The core takes a new id for each sleep,
    /// and ignores the answers of an older one.
    pub sleep: u32,
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
    /// of the idle time makes the phase `Asleep` ([`sleep`]). A move to the
    /// stand makes the phase `Stand`. Each change out of this phase clears
    /// the idle timer ([`State::enter`]).
    Awake {
        /// The idle timer.
        idle: Slot,
        /// The core waits one more idle period, because service calls wait
        /// in the outbox. The next end of the idle time sleeps.
        extended: bool,
    },
    /// The device is on battery, in the steps of the sleep `id`, or its
    /// `Sleep` request waits. Refer to [`SleepStep`] for the step.
    ///
    /// A tap, a wake from the `Sleep` request or `Start` makes the phase
    /// `Awake`. A move to the stand makes the phase `Stand`. The core then
    /// ignores the answers of the steps of the sleep `id`.
    Asleep {
        /// The id of the sleep.
        id: u32,
    },
}

impl State {
    /// Returns the power source. The device is on battery while it is
    /// awake on battery or asleep.
    #[must_use]
    pub const fn source(&self) -> PowerSource {
        match self.phase {
            Phase::Stand => PowerSource::Stand,
            Phase::Awake { .. } | Phase::Asleep { .. } => PowerSource::Battery,
        }
    }

    /// Returns `true` if the device is asleep, or in the steps before the
    /// sleep.
    #[must_use]
    pub const fn is_asleep(&self) -> bool {
        matches!(self.phase, Phase::Asleep { .. })
    }

    /// Returns `true` if the device is in the sleep `id`. The core ignores
    /// the answers of the steps of an older sleep, and of a sleep that a
    /// wake ended.
    const fn sleeping(&self, id: u32) -> bool {
        matches!(self.phase, Phase::Asleep { id: current } if current == id)
    }

    /// Returns the state of a device on battery in the steps of the sleep
    /// `sleep`, for tests that start in the middle of a sleep.
    #[cfg(test)]
    pub(crate) const fn asleep_in(sleep: u32) -> Self {
        Self {
            battery: None,
            phase: Phase::Asleep { id: sleep },
            idle: 0,
            sleep,
        }
    }

    /// Wakes the device on battery with no idle timer, for tests that must
    /// not draw at the wake.
    #[cfg(test)]
    pub(crate) fn wake_with_no_draw(&mut self) {
        self.enter(awake_on(PowerSource::Battery));
    }

    /// Changes the phase to `next`. If the old phase is `Awake`, the core
    /// clears its idle timer.
    fn enter(&mut self, next: Phase) {
        if let Phase::Awake { mut idle, .. } = mem::replace(&mut self.phase, next) {
            idle.stop();
        }
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
    /// The core asked the shell to restart the chip, so the radio does not
    /// turn off. No step waits, and the core stays asleep until a wake or
    /// `Start`.
    Stalled,
}

/// Returns the step of the sleep, or `None` while the device is awake.
///
/// The core keeps no copy of the step. The function reads the step from the
/// feature in which it waits, in this order:
///
/// 1. If the light is on, the step is `LightOff`. This occurs only in
///    [`sleep`], before the `FrontLight` request.
/// 2. If the Wi-Fi phase is `TurningOff` for this sleep, the step is
///    `RadioOff`.
/// 3. If the Wi-Fi phase is `StoppingScan` for this sleep, the step is
///    `StopScan`.
/// 4. If the stream closes for this sleep, the step is `Disconnect`.
/// 5. If the core asked for a restart of the chip, the step is `Stalled`.
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
    let Phase::Asleep { id } = model.power.phase else {
        return None;
    };
    if light::is_on(model) {
        return Some(SleepStep::LightOff);
    }
    let step = match model.wifi.phase {
        wifi::Phase::TurningOff { sleep, .. } if sleep == id => SleepStep::RadioOff,
        wifi::Phase::StoppingScan { sleep, .. } if sleep == id => SleepStep::StopScan,
        _ if model.connection.sleep_waiter() == Some(id) => SleepStep::Disconnect,
        wifi::Phase::Restarting => SleepStep::Stalled,
        wifi::Phase::Off if !model.connection.has_stream() => SleepStep::Sleep,
        _ => SleepStep::LightOff,
    };
    Some(step)
}

/// Writes the line of the log for the start of `step` of the sleep `id`.
/// The line comes before the effects of the step.
fn step_started(step: SleepStep, id: u32) -> Cmd {
    let line = |message: &str| format!("sleep {id} {message}");
    match step {
        SleepStep::LightOff => log::info(Area::Power, line("turns the front light off")),
        SleepStep::StopScan => log::info(Area::Power, line("waits for the end of the scan")),
        SleepStep::Disconnect => log::info(Area::Power, line("closes the connection")),
        SleepStep::RadioOff => log::info(Area::Power, line("turns the radio off")),
        SleepStep::Sleep => log::info(Area::Power, line("draws the last image and sleeps")),
        SleepStep::Stalled => log::warn(
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

/// Sets the power phase for `Start` on `source`. The device is awake, and
/// the core clears the idle timer that runs. The core ignores the answers
/// of a sleep that `Start` ended.
///
/// The caller then starts the idle timer with [`restart_idle_timer`].
pub fn reset(source: PowerSource, model: &mut Model) {
    model.power.enter(awake_on(source));
}

/// The `Sleep` request of the sleep `id` ended with `wake`.
///
/// If the device still sleeps for that request, it wakes and connects again.
/// A touch also acts as a tap on its control. A change of the power source
/// comes first as `PowerSourceChanged`, and a move to the stand wakes the
/// device. Thus the device sleeps only on battery, and the core ignores
/// `Wake::PowerSource`.
pub fn woke(id: u32, wake: Wake, model: &mut Model) -> Cmd {
    if !model.power.sleeping(id) {
        return Cmd::done();
    }
    match wake {
        Wake::Touch(control) => rooms::tapped(control, model),
        Wake::Timer => wake_up(model),
        Wake::PowerSource => Cmd::done(),
    }
}

/// The stream of the MQTT connection of the sleep `id` closed: turn the
/// radio off. Refer to [`wifi::off`].
pub fn offline(id: u32, model: &mut Model) -> Cmd {
    if !model.power.sleeping(id) {
        return Cmd::done();
    }
    // After the core asked for a restart, `wifi::off` does nothing.
    if model.wifi.phase == wifi::Phase::Restarting {
        return step_started(SleepStep::Stalled, id);
    }
    step_started(SleepStep::RadioOff, id).and(wifi::off(id, model))
}

/// The radio of the sleep `id` is off: draw the last image and sleep. The
/// Wi-Fi feature calls this function. Refer to [`wifi::radio_off`].
pub fn radio_off(id: u32, model: &mut Model) -> Cmd {
    if !model.power.sleeping(id) {
        return Cmd::done();
    }
    step_started(SleepStep::Sleep, id)
        .and(redraw(model, true))
        .and(
            Power::sleep(vec![WakeSource::Touch])
                .then_send(move |wake| Event::Woke { sleep: id, wake }.into()),
        )
}

/// Wakes the device from a sleep, or from the steps before it, and connects
/// again. The caller makes sure that the device is asleep.
pub fn wake_up(model: &mut Model) -> Cmd {
    debug_assert!(
        model.power.is_asleep(),
        "only a device that sleeps can wake"
    );
    model.power.enter(awake_on(PowerSource::Battery));
    connection::restart(model);
    redraw(model, true)
        .and(connection::go_online(model))
        .and(restart_idle_timer(model))
}

/// The device moved to the stand or off the stand.
pub fn source_changed(source: PowerSource, model: &mut Model) -> Cmd {
    if source == model.power.source() {
        return Cmd::done();
    }
    match source {
        PowerSource::Stand => {
            let was_asleep = model.power.is_asleep();
            // The device stays awake on the stand, so it needs no idle
            // timer.
            model.power.idle = model.power.idle.wrapping_add(1);
            model.power.enter(Phase::Stand);
            connection::restart(model);
            light::source_changed(model)
                .and(redraw(model, was_asleep))
                .and(connection::go_online(model))
        }
        PowerSource::Battery => {
            model.power.enter(awake_on(PowerSource::Battery));
            light::source_changed(model)
                .and(redraw(model, false))
                .and(restart_idle_timer(model))
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
pub fn battery_changed(level: Option<u8>, model: &mut Model) -> Cmd {
    let shown = model.power.battery.map(rounded_level);
    model.power.battery = level;
    if level.map(rounded_level) == shown {
        return Cmd::done();
    }
    if model.power.is_asleep() {
        return Cmd::done();
    }
    redraw(model, false)
}

/// Clears the idle timer that runs, and starts a new one if the device is
/// awake on battery. A new idle period does not wait for the outbox.
pub fn restart_idle_timer(model: &mut Model) -> Cmd {
    let state = &mut model.power;
    state.idle = state.idle.wrapping_add(1);
    let id = state.idle;
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
    idle_ended(model)
}

/// The user asked to sleep now. The idle time ends at once, as if the idle
/// timer expired ([`idle_ended`]).
///
/// The core clears the idle timer that runs, also if the event does
/// nothing. Before `Start`, the device then stays `Awake` with no idle
/// timer.
pub fn sleep_now(model: &mut Model) -> Cmd {
    model.power.idle = model.power.idle.wrapping_add(1);
    if let Phase::Awake { idle, .. } = &mut model.power.phase {
        idle.stop();
    }
    idle_ended(model)
}

/// The idle time ended: sleep.
///
/// Before `Start`, on the stand, or while the device sleeps, the core does
/// nothing. If service calls wait in the outbox, the core first waits one
/// more idle period for the connection. The next end of the idle time then
/// sleeps.
fn idle_ended(model: &mut Model) -> Cmd {
    if !model.startup.is_started() {
        return Cmd::done();
    }
    let Phase::Awake { extended, .. } = model.power.phase else {
        return Cmd::done();
    };
    if !model.connection.outbox.is_empty() && !extended {
        let timer = restart_idle_timer(model);
        if let Phase::Awake { extended, .. } = &mut model.power.phase {
            *extended = true;
        }
        return timer;
    }
    sleep(model)
}

/// Starts the steps of a sleep. If the front light is on, the first step
/// turns it off, and the answer to `FrontLight` starts the next step
/// ([`light_off`]). Otherwise the core starts that step at once.
fn sleep(model: &mut Model) -> Cmd {
    model.power.sleep = model.power.sleep.wrapping_add(1);
    let id = model.power.sleep;
    model.power.enter(Phase::Asleep { id });
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
    if !model.power.sleeping(id) {
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
    if !model.power.sleeping(id) {
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
fn disconnect(id: u32, model: &mut Model) -> Cmd {
    if let Some(stop) = wifi::stop_scan(id, model) {
        return step_started(SleepStep::StopScan, id).and(stop);
    }
    model.unsent += model.connection.outbox.len();
    step_started(SleepStep::Disconnect, id).and(connection::go_offline(id, model))
}

#[cfg(test)]
mod tests;
