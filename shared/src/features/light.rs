//! The light feature: the policy of the front light.
//!
//! - Each touch turns the front light on, also a touch on no control. The
//!   level comes from the power source: [`STAND_LEVEL`] on the stand, and
//!   the lower [`BATTERY_LEVEL`] on battery.
//! - The light goes off [`LIGHT_TIMEOUT`] after the last touch. If the light
//!   is still on when the device sleeps, it goes off before the other steps
//!   of the sleep.
//! - If the power source changes while the light is on, the light changes to
//!   the level of the new source.
//! - Each light timer has an id. A touch clears the light timer that runs
//!   and starts a new one. If the expiry of an older timer still arrives,
//!   the core ignores it.
//!
//! The shell only sets the level that the core gives in `FrontLight`.

use core::time::Duration;

use crate::{
    Cmd, Event, Model,
    capabilities::{light::Light, power::PowerSource},
    features::timer::Slot,
};

/// The level of the front light on the stand, in percent.
pub const STAND_LEVEL: u8 = 60;

/// The level of the front light on battery, in percent. It is lower than
/// [`STAND_LEVEL`], so the light uses less of the battery.
pub const BATTERY_LEVEL: u8 = 25;

/// The time without a touch after which the front light goes off.
///
/// This time is shorter than
/// [`IDLE_TIMEOUT`](crate::features::power::IDLE_TIMEOUT). Thus, on battery,
/// the light is usually off before the device sleeps. The light is on at
/// the sleep only after a touch on no control, because that touch does not
/// start the idle timer again.
pub const LIGHT_TIMEOUT: Duration = Duration::from_secs(20);

/// The state of the front light.
#[derive(Debug, Default)]
pub struct State {
    /// The level that the core last sent to the shell, in percent. The
    /// value 0 means that the light is off.
    pub level: u8,
    /// The id of the newest light timer.
    pub timer: u32,
    /// The light timer that runs.
    running: Slot,
}

/// Returns the level of the front light for the power source `source`.
#[must_use]
pub const fn level_for(source: PowerSource) -> u8 {
    match source {
        PowerSource::Stand => STAND_LEVEL,
        PowerSource::Battery => BATTERY_LEVEL,
    }
}

/// Returns `true` if the front light is on.
#[must_use]
pub const fn is_on(model: &Model) -> bool {
    model.light.level > 0
}

/// The user touched the panel: turns the light on at the level of the power
/// source, and starts the light timer again.
///
/// If the light is already on at that level, the core sends no `FrontLight`.
/// The core then only starts a new timer.
pub fn touched(model: &mut Model) -> Cmd {
    model.light.timer = model.light.timer.wrapping_add(1);
    let id = model.light.timer;
    let timer = model
        .light
        .running
        .start(id, LIGHT_TIMEOUT, Event::LightTimeout(id));
    set(level_for(model.power.source), model).and(timer)
}

/// The light timer with the id `id` expired. If it is the newest timer,
/// turns the light off.
pub fn timeout(id: u32, model: &mut Model) -> Cmd {
    if !model.light.running.expired(id) {
        return Cmd::done();
    }
    set(0, model)
}

/// Turns the light off, and clears the light timer that runs.
///
/// If the light is already off, the command has no effect.
pub fn off(model: &mut Model) -> Cmd {
    model.light.timer = model.light.timer.wrapping_add(1);
    model.light.running.stop();
    set(0, model)
}

/// Turns the light off after a start. The core does not know the state of
/// the light after a reset, so it always sends `FrontLight`. The core
/// clears the light timer that runs.
pub fn reset(model: &mut Model) -> Cmd {
    model.light.timer = model.light.timer.wrapping_add(1);
    model.light.running.stop();
    model.light.level = 0;
    Light::set(0).build()
}

/// The power source changed. If the light is on, sets the level of the new
/// source.
pub fn source_changed(model: &mut Model) -> Cmd {
    if !is_on(model) {
        return Cmd::done();
    }
    set(level_for(model.power.source), model)
}

/// Sets the light to `level`. If the light already has that level, the
/// command has no effect.
fn set(level: u8, model: &mut Model) -> Cmd {
    if model.light.level == level {
        return Cmd::done();
    }
    model.light.level = level;
    Light::set(level).build()
}
