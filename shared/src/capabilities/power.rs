//! The power capability: sleep, and the power source that the shell reports.

use alloc::vec::Vec;

use crux_core::{Command, Request, command::RequestBuilder, macros::Operation};
use facet::Facet;

use crate::Control;
use serde::{Deserialize, Serialize};

/// Where the device gets its power.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub enum PowerSource {
    /// The device is on its stand, which charges it.
    #[default]
    Stand,
    /// The device runs from its battery.
    Battery,
}

/// A thing that wakes the device from sleep.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
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
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Wake {
    /// A touch on the panel, on this control or (`None`) on no control. The
    /// shell gets the control from `ui::hit_test`, as for
    /// [`Event::Tapped`](crate::Event::Tapped). The last image shows the
    /// page and its controls, so the touch wakes the device and also acts
    /// as a tap. If the shell cannot read the point, it answers `None`, and
    /// the touch wakes the device and turns the front light on, but it does
    /// not act on a control.
    Touch(Option<Control>),
    /// The timer of [`WakeSource::Timer`] expired.
    Timer,
    /// The shell saw a change of the power source. It sends
    /// [`Event::PowerSourceChanged`](crate::Event::PowerSourceChanged) first.
    PowerSource,
}

/// Puts the device to sleep until one of `wake_on` occurs. The panel keeps
/// its last image.
///
/// The core sends `Sleep` together with the `Render` of the last image. The
/// shell must draw that image on the panel before the device sleeps.
///
/// The shell answers once, at the first real wake, with the [`Wake`] that
/// ended the sleep. A wake that has no cause in `wake_on` (for example a
/// sleep that the hardware rejects) is a detail of the shell. The shell then
/// sleeps again and does not answer. If the shell sees a change of the power
/// source before it answers, it answers [`Wake::PowerSource`].
///
/// After a deep sleep, the device starts again and the shell sends
/// [`Event::Start`](crate::Event::Start). Then no answer comes.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
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
