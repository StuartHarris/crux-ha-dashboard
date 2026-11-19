//! The light capability: the front light of the panel.
//!
//! On the desktop, the simulator shows the light as a tint over the panel,
//! and it answers `FrontLight` at once.

use crux_core::{Command, Request, command::RequestBuilder, macros::Operation};
use facet::Facet;
use serde::{Deserialize, Serialize};

/// Sets the brightness of the front light.
///
/// The core decides the level and when the light goes on and off. The shell
/// only drives the hardware. The shell answers when the light has the new
/// level, so the core can put `FrontLight` before the steps of a sleep.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct FrontLight {
    /// The brightness in percent, from 0 to 100. The value 0 turns the
    /// light off.
    pub level: u8,
}

/// Makes the commands for the light operations.
pub struct Light;

impl Light {
    /// Sets the front light to `level` percent. The request ends when the
    /// light has the new level.
    #[must_use]
    pub fn set<Effect, Event>(level: u8) -> RequestBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<FrontLight>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(FrontLight { level })
    }
}
