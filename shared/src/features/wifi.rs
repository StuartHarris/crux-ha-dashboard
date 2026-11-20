//! The Wi-Fi feature: the Wi-Fi policy of the core.
//!
//! The core decides when to join, which access point to join, when to scan
//! for a better one and when to restart the chip. The shell does only the
//! radio work. Refer to [`crate::capabilities::radio`].
//!
//! [`rules`] has the pure rules. The state machine that uses them comes in a
//! later change.

pub mod rules;
