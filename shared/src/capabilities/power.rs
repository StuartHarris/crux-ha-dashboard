//! The power capability: sleep, and the power source that the shell reports.

use alloc::vec::Vec;

use crux_core::{Command, Request, macros::Operation};
use facet::Facet;
use serde::{Deserialize, Serialize};

use super::notify;

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

/// Puts the device to sleep. The panel keeps its last image.
///
/// The shell does not answer. After a deep sleep, the device starts again and
/// the shell sends [`Event::Start`](crate::Event::Start). After a light sleep,
/// the shell sends [`Event::Woke`](crate::Event::Woke).
///
/// The core sends `Sleep` after the `Render` of the last image. The shell must
/// finish that render on the panel before the device sleeps.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(notify)]
pub struct Sleep {
    /// The things that wake the device.
    pub wake_on: Vec<WakeSource>,
}

/// Makes the commands for the power operations.
pub struct Power;

impl Power {
    /// Puts the device to sleep until one of `wake_on` occurs.
    pub fn sleep<Effect, Event>(wake_on: Vec<WakeSource>) -> Command<Effect, Event>
    where
        Effect: From<Request<Sleep>> + Send + 'static,
        Event: Send + 'static,
    {
        notify(Sleep { wake_on })
    }
}
