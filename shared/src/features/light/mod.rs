//! The light feature: the policy of the front light.
//!
//! - A touch turns the front light on, also a touch on no control. The level
//!   is the tap level of the power source. Home Assistant sets the tap level
//!   of each source with a number entity. Until Home Assistant sets it, the
//!   tap level is [`DEFAULT_STAND_LEVEL`] on the stand and the lower
//!   [`DEFAULT_BATTERY_LEVEL`] on battery. At the tap level 0, a touch does
//!   not turn the light on.
//! - The light of a touch goes off [`LIGHT_TIMEOUT`] after the last touch.
//! - Home Assistant can turn the light on and off with the light entity of
//!   the device. A light that Home Assistant turned on stays on: a touch and
//!   the light timer do not change it. Only Home Assistant, a sleep or
//!   `Start` turns it off.
//! - A `light.turn_on` without brightness, for example from an automation, a
//!   scene or the toggle, turns a tap light into a held light. The light
//!   then stays on until Home Assistant turns it off, and on the stand the
//!   device does not sleep.
//! - If the light is still on when the device sleeps, it goes off before the
//!   other steps of the sleep.
//! - Each light timer has an id. A touch clears the light timer that runs
//!   and starts a new one. If the expiry of an older timer still arrives,
//!   the core ignores it.
//! - The core publishes the state of the light to Home Assistant once on
//!   each connection, after each change, and after each command of Home
//!   Assistant. Refer to [`Sync`].
//!
//! The shell only sets the level that the core gives in `FrontLight`.
//!
//! # State table
//!
//! `L(s)` is the lit level of a tap at the power source `s`, from the tap
//! levels ([`TapLevel`]). `None` is the tap level 0. `s` is the power source
//! now. The core sends `FrontLight` only if the level changes. "pub" means
//! that the light publisher sees the change. "cmd" means that the core
//! publishes the state again for a command of Home Assistant, also if the
//! light did not change ([`Sync`]). Each line of the log has the area `device`.
//!
//! ## Tap and timer
//!
//! | From | Input | To | Effects |
//! | --- | --- | --- | --- |
//! | `Off` | tap, `L(s) = Some(l)` | `Tap { l, t' }` | `FrontLight l`, start `t'`, pub |
//! | `Off` | tap, `L(s) = None` | `Off` | nothing (no timer) |
//! | `Tap { l, t }` | tap, `L(s) = Some(l)` | `Tap { l, t' }` | clear `t`, start `t'` |
//! | `Tap { l, t }` | tap, `L(s)` is another level or `None` | as the two rows above | not reachable, because a change of the level or of the source updates `Tap` at once |
//! | `Held { l }` | tap | `Held { l }` | nothing. The tap still acts on its control and on the idle timer. |
//! | `Tap { l, t }` | `Timeout(t.id)` | `Off` | `FrontLight 0`, pub |
//! | `Tap { l, t }` | `Timeout(other id)` | unchanged | nothing (stale) |
//! | `Off`, `Held` | `Timeout(any)` | unchanged | nothing (stale) |
//!
//! ## Commands of Home Assistant on `set/light` (live, link up, awake)
//!
//! | From | Input | To | Effects |
//! | --- | --- | --- | --- |
//! | any | ON with brightness `b` (1 to 100) | `Held { b }` | clear the timer of a `Tap`, `FrontLight b`, Debug line, cmd |
//! | `Off` | ON, no brightness | `Held { L(s) }`, or the default of `s` if `L(s) = None` | `FrontLight`, Debug line, cmd |
//! | `Tap { l, t }` | ON, no brightness | `Held { l }` | clear `t`, Debug line, cmd |
//! | `Held { l }` | ON, no brightness | `Held { l }` | Debug line, cmd |
//! | any | ON with brightness 0 | as OFF | |
//! | `Tap`, `Held` | OFF | `Off` | clear the timer of a `Tap`, `FrontLight 0`, Debug line, cmd |
//! | `Off` | OFF | `Off` | Debug line, cmd (so Home Assistant gets a state) |
//!
//! ## Commands that the core ignores
//!
//! | From | Input | To | Effects |
//! | --- | --- | --- | --- |
//! | any | a retained `set/light` | unchanged | Info line, no publish |
//! | any | a payload that does not parse, or brightness more than 100 | unchanged | Warn line, no publish |
//! | any, power `Asleep` (the first steps of a sleep, link still up) | a command | unchanged | Debug line. The sleep wins. |
//! | any, a restart waits | a command | unchanged | Debug line |
//!
//! ## Tap levels on `set/tap_level_stand` and `set/tap_level_battery` (retained or live)
//!
//! | From | Input | To | Effects |
//! | --- | --- | --- | --- |
//! | any | the same tap level as now (the retained message at each connect) | unchanged | nothing |
//! | `Tap { l, t }` | a new tap level of `s`, `L(s) = Some(m)` | `Tap { m, t }` | `FrontLight m`, the timer continues, Debug line, pub |
//! | `Tap { l, t }` | a new tap level of `s`, `L(s) = None` | `Off` | clear `t`, `FrontLight 0`, Debug line, pub |
//! | `Tap` | a new tap level of the other source | `Tap` | Debug line |
//! | `Off`, `Held` | a new tap level of any source | unchanged | Debug line |
//! | any | an empty payload | as `Default` | as the rows above |
//! | any | a payload that does not parse, or more than 100 | unchanged | Warn line |
//!
//! ## Power, sleep, start and connection
//!
//! | From | Input | To | Effects |
//! | --- | --- | --- | --- |
//! | `Tap { l, t }` | source change, `L(new) = Some(m)` | `Tap { m, t }` | `FrontLight m` if `m` is not `l`, the timer continues, pub |
//! | `Tap { l, t }` | source change, `L(new) = None` | `Off` | clear `t`, `FrontLight 0`, pub |
//! | `Held { l }` | source change | `Held { l }` | nothing |
//! | `Off` | source change | `Off` | nothing |
//! | `Tap`, `Held` | a sleep starts (the step `LightOff`) | `Off` | clear the timer of a `Tap`, `FrontLight 0`. No publish, because the queue is closed. |
//! | `Off` | a sleep starts | `Off` | nothing |
//! | any | `Start` | `Off`, tap levels `Default` | clear the timer, `FrontLight 0` always |
//! | any | a restart waits | unchanged | the chip reset turns the light off, then `Start` |
//! | any | a new session | unchanged | [`Sync::Due`]: the queue publishes the state |
//! | any | a live birth message of Home Assistant | unchanged | [`Sync::again`]: the queue publishes the state again |
//! | `Held` on battery | the idle timer expires | `Off` (through the sleep) | as a sleep |
//!
//! An `Event::Commanded` acts in the update after the message, outside
//! [`connection::pump`](crate::features::connection::pump). A tap does not
//! pump in [`touched`]. [`rooms::tapped`](crate::features::rooms::tapped)
//! pumps once after the action of the tap. Thus the service call of a tap
//! takes the next write, and the state of the light goes in the write
//! after it. If `touched` pumped, the state of the light would take the
//! write, and the service call would wait for its `Sent`.

mod wire;

use alloc::format;
use core::{num::NonZeroU8, time::Duration};

pub(crate) use wire::{acked, command, in_flight, never, next, taken, tap_level};

use crate::{
    Cmd, Model,
    capabilities::{light::Light as FrontLight, log::Area, power::PowerSource},
    features::{device, logs, timer::Running},
    mqtt::PacketId,
};

/// The internal events of the front light. The core makes them from its
/// timer and from the messages of Home Assistant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The light timer with this id expired. Refer to
    /// [`crate::features::light`].
    Timeout(u32),
    /// A live command of Home Assistant on `set/light`.
    Commanded(Command),
    /// A message on `set/tap_level_stand` or `set/tap_level_battery`,
    /// retained or live.
    TapLevelSet {
        /// The power source of the tap level.
        source: PowerSource,
        /// The new tap level.
        level: TapLevel,
    },
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Light(event)
    }
}

/// Handles an internal event of the front light.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::Timeout(id) => timeout(id, model),
        Event::Commanded(command) => commanded(command, model),
        Event::TapLevelSet { source, level } => tap_level_set(source, level, model),
    }
}

/// A level of the light that is on, in percent, from 1 to 100.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level(NonZeroU8);

impl Level {
    /// Returns the level of `percent`, or `None` for 0 or a value more
    /// than 100.
    #[must_use]
    pub const fn new(percent: u8) -> Option<Self> {
        if percent > 100 {
            return None;
        }
        match NonZeroU8::new(percent) {
            Some(level) => Some(Self(level)),
            None => None,
        }
    }

    /// Returns the level in percent.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0.get()
    }
}

/// A tap level in percent, from 0 to 100. The value 0 means that a tap does
/// not turn the light on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Percent(u8);

impl Percent {
    /// Returns the tap level of `percent`, or `None` for a value more than
    /// 100.
    #[must_use]
    pub const fn new(percent: u8) -> Option<Self> {
        if percent > 100 {
            None
        } else {
            Some(Self(percent))
        }
    }

    /// Returns the tap level in percent.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// The tap level of one power source, from the retained number of Home
/// Assistant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TapLevel {
    /// No retained value, or an empty one. The level is the default of the
    /// source ([`default_level`]).
    #[default]
    Default,
    /// The level that Home Assistant set.
    Set(Percent),
}

/// A command of Home Assistant on `set/light`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Turn the light on, at the brightness if the command has one.
    On(Option<Level>),
    /// Turn the light off.
    Off,
}

/// The level of a tap on the stand until Home Assistant sets a tap level.
pub const DEFAULT_STAND_LEVEL: Level = Level::new(60).expect("60 is a lit level");

/// The level of a tap on battery until Home Assistant sets a tap level. It
/// is lower than [`DEFAULT_STAND_LEVEL`], so the light uses less of the
/// battery.
pub const DEFAULT_BATTERY_LEVEL: Level = Level::new(25).expect("25 is a lit level");

/// The time without a touch after which the light of a touch goes off.
///
/// This time is shorter than
/// [`IDLE_TIMEOUT`](crate::features::power::IDLE_TIMEOUT). Thus, on battery,
/// the light of a touch is usually off before the device sleeps. The light
/// is on at the sleep after a touch on no control, because that touch does
/// not start the idle timer again, or if Home Assistant turned it on.
pub const LIGHT_TIMEOUT: Duration = Duration::from_secs(20);

/// The state of the front light.
#[derive(Debug, Default)]
pub struct State {
    /// The last id that the core took for a light timer. Each new light timer
    /// takes the next id. The `Running` timer in `light` decides if an
    /// expiry is stale.
    pub timer: u32,
    /// The light, as the core last sent it to the shell.
    light: Light,
    /// The tap level of each power source.
    taps: TapLevels,
}

impl State {
    /// Returns the level that the core last sent to the shell, in percent.
    /// The value 0 means that the light is off.
    #[must_use]
    pub fn level(&self) -> u8 {
        self.light.level().map_or(0, Level::get)
    }

    /// Returns the level of the light, or `None` if it is off.
    #[must_use]
    pub const fn lit(&self) -> Option<Level> {
        self.light.level()
    }

    /// Returns `true` if Home Assistant turned the light on, and only Home
    /// Assistant, a sleep or `Start` turns it off.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        matches!(self.light, Light::Held { .. })
    }

    /// Returns the tap level of `source`.
    #[must_use]
    pub const fn tap_level(&self, source: PowerSource) -> TapLevel {
        self.taps.get(source)
    }
}

/// The tap level of each power source.
#[derive(Debug, Default)]
struct TapLevels {
    stand: TapLevel,
    battery: TapLevel,
}

impl TapLevels {
    /// Returns the tap level of `source`.
    const fn get(&self, source: PowerSource) -> TapLevel {
        match source {
            PowerSource::Stand => self.stand,
            PowerSource::Battery => self.battery,
        }
    }

    /// Sets the tap level of `source`.
    const fn set(&mut self, source: PowerSource, level: TapLevel) {
        match source {
            PowerSource::Stand => self.stand = level,
            PowerSource::Battery => self.battery = level,
        }
    }

    /// Returns the lit level of a tap at `source`, or `None` if the tap level
    /// is 0.
    const fn of(&self, source: PowerSource) -> Option<Level> {
        match self.get(source) {
            TapLevel::Default => Some(default_level(source)),
            TapLevel::Set(percent) => Level::new(percent.get()),
        }
    }
}

/// The front light, as the core last sent it to the shell.
#[derive(Debug, Default)]
enum Light {
    /// The light is off, and no light timer runs.
    #[default]
    Off,
    /// A tap turned the light on. The light timer turns it off.
    Tap {
        /// The level.
        level: Level,
        /// The light timer. When it expires, the light goes off.
        timer: Running,
    },
    /// Home Assistant turned the light on. Only Home Assistant, a sleep or
    /// `Start` turns it off.
    Held {
        /// The level.
        level: Level,
    },
}

impl Light {
    /// Returns the level, or `None` if the light is off.
    const fn level(&self) -> Option<Level> {
        match self {
            Self::Off => None,
            Self::Tap { level, .. } | Self::Held { level } => Some(*level),
        }
    }

    /// Clears the light timer, if it runs.
    fn clear(self) {
        if let Self::Tap { timer, .. } = self {
            timer.clear();
        }
    }
}

/// The publish of the state of the light on one session.
///
/// | `Sync` | Event | Next | What the core does |
/// | --- | --- | --- | --- |
/// | (none) | a new session | `Due` | the queue publishes the state |
/// | `Due` | the session takes it | `InFlight { sent: now, again: false }` | nothing |
/// | `InFlight` | the light changes | `InFlight` | nothing yet |
/// | `InFlight` | a command of Home Assistant | `InFlight { again: true }` | nothing yet |
/// | `InFlight { again: false }` | its PUBACK | `Sent(sent)` | the queue publishes if the light is not `sent` |
/// | `InFlight { again: true }` | its PUBACK | `Due` | the queue publishes again |
/// | `Sent(s)` | the light is not `s` | (offer) | the queue publishes it |
/// | `InFlight` | a live birth message of Home Assistant | `InFlight { again: true }` | nothing yet |
/// | `Sent(s)` | a command of Home Assistant | `Due` | the queue publishes it |
/// | `Sent(s)` | a live birth message of Home Assistant | `Due` | the queue publishes it |
/// | any | the session ends | (none) | nothing |
///
/// The sync is in the [`Live`](crate::features::connection::Live) of the
/// session, so each session starts with `Due`. At most one state message
/// of the light waits for a PUBACK. During a dry run, the queue publishes
/// no state of the light.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sync {
    /// The session must publish the state: a new session, or a command of
    /// Home Assistant came.
    #[default]
    Due,
    /// The state message waits for its PUBACK.
    InFlight {
        /// The packet id of the message.
        packet: PacketId,
        /// The level in the message, or `None` for OFF.
        sent: Option<Level>,
        /// A command of Home Assistant came while the message waits, so the
        /// state goes again after the PUBACK.
        again: bool,
    },
    /// The broker acknowledged the state with this level (`None` is OFF).
    Sent(Option<Level>),
}

impl Sync {
    /// A command or a birth message of Home Assistant came: the state goes
    /// again, also if it did not change. Home Assistant then gets a state
    /// for each command.
    pub(crate) const fn again(&mut self) {
        match self {
            Self::Due => {}
            Self::InFlight { again, .. } => *again = true,
            Self::Sent(_) => *self = Self::Due,
        }
    }
}

/// Returns the level of a tap at `source` until Home Assistant sets a tap
/// level.
#[must_use]
pub const fn default_level(source: PowerSource) -> Level {
    match source {
        PowerSource::Stand => DEFAULT_STAND_LEVEL,
        PowerSource::Battery => DEFAULT_BATTERY_LEVEL,
    }
}

/// Returns `true` if the front light is on.
#[must_use]
pub const fn is_on(model: &Model) -> bool {
    model.light.light.level().is_some()
}

/// Returns the command that sets the front light from `old` to `new`, or no
/// command if the level is the same.
fn front_light(old: Option<Level>, new: Option<Level>) -> Cmd {
    if old == new {
        Cmd::done()
    } else {
        FrontLight::set(new.map_or(0, Level::get)).build()
    }
}

/// The user touched the panel: turns the light on at the tap level of the
/// power source, and starts the light timer again.
///
/// The new light timer replaces the timer that runs, and the core clears
/// the old timer. If the light is already on at that level, the core sends
/// no `FrontLight`. The core then only starts a new timer. At the tap level
/// 0, the touch does not turn the light on. If Home Assistant turned the
/// light on, the touch does not change it.
///
/// The function does not pump the connection. The caller publishes the
/// state after the action of the touch. Refer to the [module docs](self).
pub fn touched(model: &mut Model) -> Cmd {
    if model.light.is_held() {
        return Cmd::done();
    }
    let Some(level) = model.light.taps.of(model.power.source()) else {
        return off(model);
    };
    model.light.timer = model.light.timer.wrapping_add(1);
    let id = model.light.timer;
    let (start, timer) = Running::start(id, LIGHT_TIMEOUT, Event::Timeout(id).into());
    let old = core::mem::replace(&mut model.light.light, Light::Tap { level, timer });
    let light = front_light(old.level(), Some(level));
    old.clear();
    light.and(start)
}

/// The light timer with the id `id` expired. If it is the newest timer of
/// a tap, turns the light off and publishes the state.
///
/// The timer already expired, so the core does not clear it.
pub fn timeout(id: u32, model: &mut Model) -> Cmd {
    match &model.light.light {
        Light::Tap { timer, .. } if timer.id() == id => {
            model.light.light = Light::Off;
            FrontLight::set(0).build().and(device::changed(model))
        }
        Light::Off | Light::Tap { .. } | Light::Held { .. } => Cmd::done(),
    }
}

/// Turns the light off, and clears the light timer that runs.
///
/// If the light is already off, the command has no effect.
pub fn off(model: &mut Model) -> Cmd {
    let old = core::mem::take(&mut model.light.light);
    let light = front_light(old.level(), None);
    old.clear();
    light
}

/// Turns the light off after a start, and sets each tap level to
/// `Default`.
///
/// The core does not know the state of the light after a reset, so it
/// always sends `FrontLight`. The core clears the light timer that runs.
/// The retained tap levels come again at the next connect.
pub fn reset(model: &mut Model) -> Cmd {
    core::mem::take(&mut model.light.light).clear();
    model.light.taps = TapLevels::default();
    FrontLight::set(0).build()
}

/// The power source or its tap level changed: if a tap turned the light
/// on, sets the tap level of the power source now.
///
/// The light timer continues to run. At the tap level 0, the light goes
/// off. A light that Home Assistant turned on does not change.
///
/// The function does not pump the connection. The caller publishes the
/// state.
pub fn source_changed(model: &mut Model) -> Cmd {
    let new = model.light.taps.of(model.power.source());
    match (&mut model.light.light, new) {
        (Light::Tap { level, .. }, Some(new)) => {
            let old = core::mem::replace(level, new);
            front_light(Some(old), Some(new))
        }
        (Light::Tap { .. }, None) => off(model),
        (Light::Off | Light::Held { .. }, _) => Cmd::done(),
    }
}

/// Home Assistant sent `command` on `set/light`. Refer to the
/// [module docs](self).
///
/// While a restart waits or the device sleeps, the core ignores the
/// command and writes a Debug line. Else the core sets the light, writes a
/// Debug line, and publishes the state again, also if it did not change.
fn commanded(command: Command, model: &mut Model) -> Cmd {
    if model.connection.restart_waits() {
        return logs::debug(
            Area::Device,
            "Home Assistant sent a light command, but the chip restarts, so the core ignored it",
        );
    }
    if model.power.is_asleep() {
        return logs::debug(
            Area::Device,
            "Home Assistant sent a light command, but the device sleeps, so the core ignored it",
        );
    }
    let new = match command {
        Command::Off => None,
        Command::On(Some(level)) => Some(level),
        Command::On(None) => Some(model.light.light.level().unwrap_or_else(|| {
            let source = model.power.source();
            model
                .light
                .taps
                .of(source)
                .unwrap_or_else(|| default_level(source))
        })),
    };
    let old = core::mem::replace(
        &mut model.light.light,
        new.map_or(Light::Off, |level| Light::Held { level }),
    );
    let light = front_light(old.level(), new);
    old.clear();
    let line = logs::debug(
        Area::Device,
        new.map_or_else(
            || "Home Assistant turned the front light off".into(),
            |level| {
                format!(
                    "Home Assistant turned the front light on at {}%",
                    level.get()
                )
            },
        ),
    );
    if let Some(live) = model.connection.live_mut() {
        live.light.again();
    }
    line.and(light).and(device::changed(model))
}

/// Home Assistant set the tap level of `source` to `level`. If a tap turned
/// the light on and `source` is the power source now, the light takes the
/// new level at once, and the light timer continues. Refer to the
/// [module docs](self).
///
/// The same tap level again does nothing. The broker sends the retained
/// tap levels at each connect.
fn tap_level_set(source: PowerSource, level: TapLevel, model: &mut Model) -> Cmd {
    if model.light.taps.get(source) == level {
        return Cmd::done();
    }
    model.light.taps.set(source, level);
    let place = match source {
        PowerSource::Stand => "on the stand",
        PowerSource::Battery => "on battery",
    };
    let line = logs::debug(
        Area::Device,
        match level {
            TapLevel::Default => format!("Home Assistant set the tap level {place} to the default"),
            TapLevel::Set(percent) => {
                format!(
                    "Home Assistant set the tap level {place} to {}%",
                    percent.get()
                )
            }
        },
    );
    let light = if source == model.power.source() {
        source_changed(model)
    } else {
        Cmd::done()
    };
    line.and(light).and(device::changed(model))
}

#[cfg(test)]
mod tests;
