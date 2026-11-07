//! The power feature: the power policy.
//!
//! - On the stand, the device stays awake and connected.
//! - On battery, the device connects when it wakes. After [`IDLE_TIMEOUT`]
//!   without a touch, it disconnects, turns Wi-Fi off, draws the last image
//!   with the time of the last state message, and sleeps until a touch.

use alloc::vec;
use core::time::Duration;

use crux_time::clock::Time;

use crate::{
    Cmd, Event, Model,
    capabilities::power::{Power, PowerSource, WakeSource},
    features::{connection, refresh::redraw},
};

/// On battery, the time without a touch after which the device sleeps.
///
/// No other timer of the core waits for this time, so a shell can find the
/// idle timer by its duration, for example to end the idle time early.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The power source and the sleep state.
#[derive(Debug, Default)]
pub struct State {
    /// The power source.
    pub source: PowerSource,
    /// The device is asleep, or goes to sleep now.
    pub asleep: bool,
    /// The id of the newest idle timer.
    pub idle: u32,
    /// The core waits one more idle period, because service calls wait in
    /// the outbox.
    pub extended: bool,
}

/// The device woke from a light sleep: connect again.
pub fn woke(model: &mut Model) -> Cmd {
    if !model.power.asleep {
        return restart_idle_timer(model);
    }
    model.power.asleep = false;
    connection::restart(model);
    redraw(model, true)
        .and(connection::go_online(model))
        .and(restart_idle_timer(model))
}

/// The device moved to the stand or off the stand.
pub fn source_changed(source: PowerSource, model: &mut Model) -> Cmd {
    if source == model.power.source {
        return Cmd::done();
    }
    model.power.source = source;
    match source {
        PowerSource::Stand => {
            let was_asleep = model.power.asleep;
            model.power.asleep = false;
            // The core ignores the idle timer that runs now.
            model.power.idle = model.power.idle.wrapping_add(1);
            connection::restart(model);
            redraw(model, was_asleep).and(connection::go_online(model))
        }
        PowerSource::Battery => redraw(model, false).and(restart_idle_timer(model)),
    }
}

/// Starts the idle timer on battery. The core then ignores the timer that
/// ran before.
pub fn restart_idle_timer(model: &mut Model) -> Cmd {
    model.power.idle = model.power.idle.wrapping_add(1);
    model.power.extended = false;
    if model.power.source != PowerSource::Battery || model.power.asleep {
        return Cmd::done();
    }
    let id = model.power.idle;
    let (timer, _handle) = Time::notify_after(IDLE_TIMEOUT);
    timer.then_send(move |_| Event::IdleTimeout(id))
}

/// The idle timer expired: sleep.
///
/// If service calls wait in the outbox, the core first waits one more idle
/// period for the connection.
pub fn idle_timeout(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.idle || model.power.source != PowerSource::Battery || model.power.asleep {
        return Cmd::done();
    }
    if !model.connection.outbox.is_empty() && !model.power.extended {
        let timer = restart_idle_timer(model);
        model.power.extended = true;
        return timer;
    }
    sleep(model)
}

/// Disconnects, turns Wi-Fi off, draws the last image and sleeps.
///
/// The core drops the service calls that wait in the outbox, and the view
/// shows how many. The last image shows the time of the last state message.
fn sleep(model: &mut Model) -> Cmd {
    model.power.asleep = true;
    model.unsent += model.connection.outbox.len();
    connection::go_offline(model)
        .then(redraw(model, true))
        .then(Power::sleep(vec![WakeSource::Touch]))
}
