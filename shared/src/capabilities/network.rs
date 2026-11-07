//! The network capability: Wi-Fi on and off.
//!
//! On the desktop, the shell answers `WifiOn` with `Connected` at once and
//! ignores `WifiOff`.

use crux_core::{Command, Request, command::RequestBuilder, macros::Operation};
use facet::Facet;
use serde::{Deserialize, Serialize};

use super::notify;

/// Turns Wi-Fi on and joins the network.
///
/// The shell answers when the device has an IP address, or when it gives up.
/// If Wi-Fi is already on, the shell answers `Connected` at once.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = WifiStatus)]
pub struct WifiOn;

/// The answer to [`WifiOn`].
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum WifiStatus {
    /// The device joined the network and has an IP address.
    Connected,
    /// The device did not join the network.
    Failed,
}

/// Turns Wi-Fi off. The shell does not answer.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(notify)]
pub struct WifiOff;

/// Makes the commands for the network operations.
pub struct Network;

impl Network {
    /// Turns Wi-Fi on. The request gives the result.
    #[must_use]
    pub fn wifi_on<Effect, Event>()
    -> RequestBuilder<Effect, Event, impl Future<Output = WifiStatus>>
    where
        Effect: From<Request<WifiOn>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(WifiOn)
    }

    /// Turns Wi-Fi off.
    pub fn wifi_off<Effect, Event>() -> Command<Effect, Event>
    where
        Effect: From<Request<WifiOff>> + Send + 'static,
        Event: Send + 'static,
    {
        notify(WifiOff)
    }
}
