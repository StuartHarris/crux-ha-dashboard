//! The power feature: the power policy.
//!
//! - On the stand, the device stays awake and connected.
//! - On battery, the device connects when it wakes. After [`IDLE_TIMEOUT`]
//!   without a touch, or at [`Event::SleepNow`], it sleeps in steps. Each
//!   step waits for the answer of the shell: stop a scan for a better
//!   access point if one runs, publish `asleep` and disconnect, then turn
//!   the radio off, then draw the last image with the time of the last
//!   state message and sleep until a touch.
//! - If the front light is still on at the sleep, one more step turns it
//!   off first. Usually the light timer turned it off already. Refer to
//!   [`LIGHT_TIMEOUT`](crate::features::light::LIGHT_TIMEOUT).
//! - The core clears the idle timer on a move to the stand, at a sleep and
//!   at [`Event::SleepNow`]. Each idle timer has an id. If the expiry of an
//!   older timer still arrives, the core ignores it.
//! - Each sleep has an id. A tap or a move to the stand wakes the device
//!   during the steps, and the core then ignores the answers of the steps and
//!   of `Sleep`.

use alloc::vec;
use core::time::Duration;

use crate::{
    Cmd, Event, Model,
    capabilities::power::{Power, PowerSource, Wake, WakeSource},
    features::{connection, light, refresh::redraw, rooms, timer::Slot, wifi},
    view::rounded_level,
};

/// On battery, the time without a touch after which the device sleeps.
///
/// To end the idle time early, a shell sends
/// [`Event::SleepNow`](crate::Event::SleepNow).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The power source and the sleep state.
#[derive(Debug, Default)]
pub struct State {
    /// The power source.
    pub source: PowerSource,
    /// The exact charge of the battery in percent, or `None` if the shell
    /// did not read it yet.
    pub battery: Option<u8>,
    /// The device is asleep, or goes to sleep now.
    pub asleep: bool,
    /// The id of the newest idle timer.
    pub idle: u32,
    /// The idle timer that runs.
    pub(crate) idle_timer: Slot,
    /// The core waits one more idle period, because service calls wait in
    /// the outbox.
    pub extended: bool,
    /// The id of the newest sleep. The core ignores the answers of an older
    /// sleep.
    pub sleep: u32,
}

/// The `Sleep` request of the sleep `id` ended with `wake`.
///
/// If the device still sleeps for that request, it wakes and connects again.
/// A touch also acts as a tap on its control. A change of the power source
/// comes first as `PowerSourceChanged`, so `Wake::PowerSource` wakes the
/// device only if the source is now `Stand`. On battery, the core ignores it.
pub fn woke(id: u32, wake: Wake, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    match wake {
        Wake::Touch(control) => rooms::tapped(control, model),
        Wake::PowerSource if model.power.source != PowerSource::Stand => Cmd::done(),
        Wake::Timer | Wake::PowerSource => wake_up(model),
    }
}

/// The stream of the MQTT connection of the sleep `id` closed: turn the
/// radio off. Refer to [`wifi::off`].
pub fn offline(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    wifi::off(id, model)
}

/// The radio of the sleep `id` is off: draw the last image and sleep. The
/// Wi-Fi feature calls this function. Refer to [`wifi::radio_off`].
pub fn radio_off(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    redraw(model, true).and(
        Power::sleep(vec![WakeSource::Touch])
            .then_send(move |wake| Event::Woke { sleep: id, wake }),
    )
}

/// Wakes the device from a sleep, or from the steps before it, and connects
/// again.
pub fn wake_up(model: &mut Model) -> Cmd {
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
            // The device stays awake on the stand, so it needs no idle
            // timer.
            model.power.idle = model.power.idle.wrapping_add(1);
            model.power.idle_timer.stop();
            connection::restart(model);
            light::source_changed(model)
                .and(redraw(model, was_asleep))
                .and(connection::go_online(model))
        }
        PowerSource::Battery => light::source_changed(model)
            .and(redraw(model, false))
            .and(restart_idle_timer(model)),
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
    if model.power.asleep {
        return Cmd::done();
    }
    redraw(model, false)
}

/// Clears the idle timer that runs, and starts a new one on battery.
pub fn restart_idle_timer(model: &mut Model) -> Cmd {
    let state = &mut model.power;
    state.idle = state.idle.wrapping_add(1);
    state.extended = false;
    state.idle_timer.stop();
    if state.source != PowerSource::Battery || state.asleep {
        return Cmd::done();
    }
    let id = state.idle;
    state
        .idle_timer
        .start(id, IDLE_TIMEOUT, Event::IdleTimeout(id))
}

/// The idle timer with the id `id` expired. If it is the newest idle timer,
/// the idle time ends ([`idle_ended`]).
pub fn idle_timeout(id: u32, model: &mut Model) -> Cmd {
    if !model.power.idle_timer.expired(id) {
        return Cmd::done();
    }
    idle_ended(model)
}

/// The user asked to sleep now. The idle time ends at once, as if the idle
/// timer expired ([`idle_ended`]).
///
/// The core clears the idle timer that runs, also if the event does
/// nothing.
pub fn sleep_now(model: &mut Model) -> Cmd {
    model.power.idle = model.power.idle.wrapping_add(1);
    model.power.idle_timer.stop();
    idle_ended(model)
}

/// The idle time ended: sleep.
///
/// Before `Start` (with no settings), on the stand, or while the device
/// sleeps, the core does nothing. If service calls wait in the outbox, the
/// core first waits one more idle period for the connection. The next end
/// of the idle time then sleeps.
fn idle_ended(model: &mut Model) -> Cmd {
    if model.settings.is_none() || model.power.source != PowerSource::Battery || model.power.asleep
    {
        return Cmd::done();
    }
    if !model.connection.outbox.is_empty() && !model.power.extended {
        let timer = restart_idle_timer(model);
        model.power.extended = true;
        return timer;
    }
    sleep(model)
}

/// Starts the steps of a sleep. If the front light is on, the first step
/// turns it off, and the answer to `FrontLight` starts the next step
/// ([`light_off`]). Otherwise the core starts that step at once.
fn sleep(model: &mut Model) -> Cmd {
    model.power.asleep = true;
    model.power.idle_timer.stop();
    model.power.sleep = model.power.sleep.wrapping_add(1);
    let id = model.power.sleep;
    if light::is_on(model) {
        return light::off(model).then(Cmd::event(Event::LightOffForSleep(id)));
    }
    disconnect(id, model)
}

/// The scan for the sleep `id` stopped: publish `asleep` and disconnect.
/// The Wi-Fi feature calls this function. Refer to [`wifi::stop_scan`].
///
/// The core ignores this answer if a tap or a move to the stand woke the
/// device first.
pub fn scan_stopped(id: u32, model: &mut Model) -> Cmd {
    if id != model.power.sleep || !model.power.asleep {
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
    if id != model.power.sleep || !model.power.asleep {
        return Cmd::done();
    }
    disconnect(id, model)
}

/// Publishes `asleep` and disconnects. The answer to the `Close` of the
/// stream starts the next step ([`offline`]). Refer to
/// [`connection::go_offline`].
///
/// If a scan for a better access point runs, the core first stops it, and
/// the answer starts this step again ([`scan_stopped`]). A scan can drop
/// the link, so the DISCONNECT packet waits for the end of the scan.
///
/// The core drops the service calls that wait in the outbox, and the view
/// shows how many. The last image shows the time of the last state message.
fn disconnect(id: u32, model: &mut Model) -> Cmd {
    if let Some(stop) = wifi::stop_scan(id, model) {
        return stop;
    }
    model.unsent += model.connection.outbox.len();
    connection::go_offline(id, model)
}

#[cfg(test)]
mod tests;
