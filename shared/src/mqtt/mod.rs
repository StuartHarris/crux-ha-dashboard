//! An MQTT 3.1.1 client with no I/O (sans-io).
//!
//! The client reads and writes bytes. It does not open sockets and does not
//! read a clock. The caller gives it the events of a byte stream and of its
//! timers, and does the actions that it returns. Thus each wait of the
//! protocol is a state with a timer, and a link that stops cannot make the
//! client wait without a limit.
//!
//! - [`codec`] encodes the packets of the client and decodes the packets of
//!   the broker.
//! - [`framer`] finds the packets in the byte stream.
//! - [`session`] is the state machine of one connection.
//! - [`types`] has the data of a connection: the options, the messages and
//!   the result of a subscribe.
//!
//! The client uses a subset of MQTT 3.1.1: a clean session, subscriptions at
//! `QoS` 0, publishes at `QoS` 0 or 1, and a last will at `QoS` 1.

use core::time::Duration;

pub mod codec;
pub mod framer;
pub mod session;
pub mod types;

#[cfg(test)]
mod tests;

pub use session::{Action, End, Input, Limits, Refused, Session, SubscribeId, Timer, TimerKind};
pub use types::{Connect, Credentials, LastWill, Message, Publish, Qos, SubscribeOutcome};

/// The longest packet body that the session reads (256 KiB).
pub const MAX_PACKET: usize = 256 * 1024;

/// The largest number of publishes at `QoS` 1 that wait for a PUBACK.
pub const MAX_IN_FLIGHT: usize = 64;

/// The largest remaining length of a SUBSCRIBE packet.
pub const SUBSCRIBE_PACKET: usize = 1024;

/// The time for the stream to open.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The time for the CONNACK packet after the stream opens.
pub const CONNACK_TIMEOUT: Duration = Duration::from_secs(10);

/// The time for the PINGRESP packet after a PINGREQ packet.
pub const PING_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// The time for the shell to send the bytes of one `Send` action.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The time for the broker to acknowledge a publish at `QoS` 1 or a
/// SUBSCRIBE packet, while acknowledgements wait.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest number of bytes that wait for the next `Send` (16 KiB).
///
/// The burst after the CONNACK must fit: the outbox, the `online` status
/// and the SUBSCRIBE packets for all the topics of the dashboard. The
/// SUBSCRIBE packets for the 115 topics are about 8 KiB (7,855 bytes).
pub const MAX_OUTGOING: usize = 16 * 1024;

/// Returns the time between two ping packets for a keep-alive interval in
/// seconds.
///
/// The client sends a ping after three quarters of the interval, so the
/// broker gets it in time. An interval of 0 turns keep-alive off, and the
/// function returns `None`.
#[must_use]
pub fn ping_interval(keep_alive_secs: u16) -> Option<Duration> {
    if keep_alive_secs == 0 {
        return None;
    }
    let millis = u64::from(keep_alive_secs) * 750;
    Some(Duration::from_millis(millis.max(500)))
}

/// Returns the period of the silence timer for a keep-alive interval in
/// seconds.
///
/// The broker answers each ping, so it sends a packet at least once in each
/// interval. The period is two intervals, and at least 60 seconds. The
/// session ends if no bytes arrive in one whole period, so it finds a silent
/// broker one to two periods after its last bytes. An interval of 0 turns
/// keep-alive off, and the function returns `None`.
#[must_use]
pub fn silence_limit(keep_alive_secs: u16) -> Option<Duration> {
    if keep_alive_secs == 0 {
        return None;
    }
    Some(Duration::from_secs(u64::from(keep_alive_secs.max(30)) * 2))
}
