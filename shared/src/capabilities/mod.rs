//! The core half of each custom capability: the operations, their outputs and
//! the commands that make them.
//!
//! Each shell has the other half, which performs the operations.

pub mod light;
pub mod network;
pub mod power;
pub mod stream;

use crux_core::{Command, Request, capability::Operation};

/// Makes a command that sends a notification to the shell.
pub(crate) fn notify<Op, Effect, Event>(operation: Op) -> Command<Effect, Event>
where
    Op: Operation,
    Effect: From<Request<Op>> + Send + 'static,
    Event: Send + 'static,
{
    Command::notify_shell(operation).build()
}
