//! The logic of the firmware shell that does not touch the hardware.
//!
//! The firmware binary (`firmware/`) connects these parts to esp-hal, the
//! Wi-Fi driver, the panel and the touch controller. This crate has no
//! hardware code, so its tests run on the host with `just test-firmware`.
//!
//! - [`shell`] owns the core, routes each effect to a task, and keeps the
//!   streams and the timers.
//! - [`stream`] is the bookkeeping of the stream task, and the time of each
//!   update of the core.
//! - [`timers`] keeps the timers of the timer task.
//! - [`touch`] converts touch samples to taps on the panel.
//! - [`power`] reads the power source and the charge from the charger and
//!   the fuel gauge, and decides which changes to send to the core.
//! - [`config`] reads and checks the build-time configuration.
//! - [`backoff`] is a wait that doubles after each failure.
//! - [`wifi`] is the rule of the Wi-Fi task for a request during a join.
//! - [`sleep`] decides when the chip can go into light sleep, and which
//!   wake is a real wake.
//! - [`waveform`] builds the lines and the lookup table of the extra frames
//!   of a clean refresh of the panel.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod backoff;
pub mod config;
pub mod power;
pub mod shell;
pub mod sleep;
pub mod stream;
pub mod timers;
pub mod touch;
pub mod waveform;
pub mod wifi;
