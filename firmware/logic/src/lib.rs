//! The logic of the firmware shell that does not touch the hardware.
//!
//! The firmware binary (`firmware/`) connects these parts to esp-hal, the
//! Wi-Fi driver, the panel and the touch controller. This crate has no
//! hardware code, so its tests run on the host with `just test-firmware`.
//!
//! - [`shell`] owns the core, routes each effect to a task, and keeps the
//!   MQTT connections and the timers.
//! - [`mqtt`] is the bookkeeping of the MQTT task.
//! - [`timers`] keeps the timers of the timer task.
//! - [`touch`] converts touch samples to taps on the panel.
//! - [`config`] reads and checks the build-time configuration.
//! - [`backoff`] is a wait that doubles after each failure.
//! - [`wifi`] is the rule of the Wi-Fi task for a request during a join.
//! - [`clock`] reads and writes the RTC, makes and checks SNTP packets, and
//!   decides when to set the clock.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod backoff;
pub mod clock;
pub mod config;
pub mod mqtt;
pub mod shell;
pub mod timers;
pub mod touch;
pub mod wifi;
