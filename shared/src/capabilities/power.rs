//! The power capability: sleep, and the power source that the shell reports.

use alloc::vec::Vec;

use crux_core::{Command, Request, command::RequestBuilder, macros::Operation};

use crate::Control;

/// Where the device gets its power.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerSource {
    /// The device is on its stand, which charges it.
    #[default]
    Stand,
    /// The device runs from its battery.
    Battery,
}

/// A thing that wakes the device from sleep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeSource {
    /// A touch on the panel.
    Touch,
    /// A timer that expires after this number of seconds.
    Timer {
        /// The time to sleep, in seconds.
        seconds: u32,
    },
}

/// What ended a sleep. The answer to [`Sleep`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// A touch on the panel, on this control or (`None`) on no control. The
    /// shell gets the control from `ui::hit_test`, as for
    /// [`Event::Tapped`](crate::Event::Tapped). The last image shows the
    /// page and its controls, so the touch wakes the device and also acts
    /// as a tap. If the shell cannot read the point, it answers `None`, and
    /// the touch wakes the device and turns the front light on, but it does
    /// not act on a control.
    Touch(Option<Control>),
    /// The timer of [`WakeSource::Timer`] expired. The core decides what the
    /// timer means. It can send a new `Sleep` at once.
    Timer,
    /// The shell saw a change of the power source. It sends
    /// [`Event::PowerSourceChanged`](crate::Event::PowerSourceChanged) first.
    PowerSource,
    /// The device woke with no cause in `wake_on`, or the hardware rejected
    /// the sleep.
    Other,
}

/// Puts the device to sleep until one of `wake_on` occurs. The panel keeps
/// its last image.
///
/// The first `Sleep` of a sleep comes together with the `Render` of the last
/// image. The shell must draw that image on the panel before the device
/// sleeps. A later `Sleep` of the same sleep has no `Render`, and the panel
/// keeps its image.
///
/// If `wake_on` has more than one timer, the shortest timer wins.
///
/// The shell answers once, at the first wake, with the [`Wake`] that ended
/// the sleep. It answers each wake at once, also a wake with no cause in
/// `wake_on` ([`Wake::Other`]). The shell keeps no schedule. The core
/// decides what to do next. If the shell sees a change of the power source
/// before it answers, it answers [`Wake::PowerSource`].
///
/// After a deep sleep, the device starts again and the shell sends
/// [`Event::Start`](crate::Event::Start). Then no answer comes.
#[derive(Operation, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = Wake)]
pub struct Sleep {
    /// The things that wake the device.
    pub wake_on: Vec<WakeSource>,
}

/// Makes the commands for the power operations.
pub struct Power;

impl Power {
    /// Puts the device to sleep until one of `wake_on` occurs. The request
    /// gives the cause of the wake.
    #[must_use]
    pub fn sleep<Effect, Event>(
        wake_on: Vec<WakeSource>,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = Wake>>
    where
        Effect: From<Request<Sleep>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Sleep { wake_on })
    }
}
