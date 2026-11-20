//! A fake MQTT 3.1.1 broker that reads and writes bytes, with a fake Home
//! Assistant behind it, and a fake radio.
//!
//! - [`broker`] has the fake broker. It reads the packets of one client,
//!   answers them, keeps retained messages and publishes the last will.
//! - [`home_assistant`] publishes entity states as the `mqtt_statestream`
//!   integration does, and does the service calls that the dashboard
//!   publishes.
//! - [`radio`] has the fake radio. It answers each radio request at once.
//!
//! The tests use this module. Turn on the `fixtures` feature to use it from
//! another crate.

pub mod broker;
pub mod home_assistant;
pub mod radio;

use alloc::vec::Vec;
use core::time::Duration;

pub use broker::{Broker, matches};
pub use home_assistant::{MAX_TEMP, MIN_TEMP, ServiceCall, iso_time};
pub use radio::FakeRadio;

use crate::{
    features::connection::KEEP_ALIVE_SECS,
    mqtt::{
        ACK_TIMEOUT, CONNACK_TIMEOUT, OPEN_TIMEOUT, PING_RESPONSE_TIMEOUT, SEND_TIMEOUT,
        ping_interval, silence_limit,
    },
};

/// Returns the durations of the timers of the MQTT session. No timer of a
/// feature has one of these durations. A test checks this, so a test shell
/// can find a timer of the session by its duration.
#[must_use]
pub fn session_timers() -> Vec<Duration> {
    let mut durations = alloc::vec![
        OPEN_TIMEOUT,
        CONNACK_TIMEOUT,
        PING_RESPONSE_TIMEOUT,
        SEND_TIMEOUT,
        ACK_TIMEOUT,
    ];
    durations.extend(ping_interval(KEEP_ALIVE_SECS));
    durations.extend(silence_limit(KEEP_ALIVE_SECS));
    durations
}
