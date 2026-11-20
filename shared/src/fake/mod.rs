//! A fake MQTT 3.1.1 broker that reads and writes bytes, with a fake Home
//! Assistant behind it.
//!
//! - [`broker`] has the fake broker. It reads the packets of one client,
//!   answers them, keeps retained messages and publishes the last will.
//! - [`home_assistant`] publishes entity states as the `mqtt_statestream`
//!   integration does, and does the service calls that the dashboard
//!   publishes.
//!
//! The tests use this module. Turn on the `fixtures` feature to use it from
//! another crate.

pub mod broker;
pub mod home_assistant;

pub use broker::{Broker, matches};
pub use home_assistant::{MAX_TEMP, MIN_TEMP, ServiceCall, iso_time};
