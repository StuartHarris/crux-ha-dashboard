//! The core half of each custom capability: the operations, their outputs and
//! the commands that make them.
//!
//! Each shell has the other half, which performs the operations.

pub mod light;
pub mod log;
pub mod power;
pub mod radio;
pub mod stream;

use core::future::Future;

use crux_core::{Command, Request, capability::Operation, command::NotificationBuilder};

/// Makes a builder for a command that sends a notification to the shell.
#[must_use]
pub(crate) fn notify<Op, Effect, Event>(
    operation: Op,
) -> NotificationBuilder<Effect, Event, impl Future<Output = ()>>
where
    Op: Operation,
    Effect: From<Request<Op>> + Send + 'static,
    Event: Send + 'static,
{
    Command::notify_shell(operation)
}
