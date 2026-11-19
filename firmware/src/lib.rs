//! The hardware half of the dashboard firmware for the LilyGo T5 E-Paper S3
//! Pro Lite.
//!
//! The logic that does not touch the hardware is in the `logic` crate
//! (`firmware/logic`), which has host tests. This crate connects it to
//! esp-hal, the Wi-Fi driver, the panel and the touch controller.
//!
//! - [`board`] starts the chip: the clocks, the heap and the scheduler.
//! - [`panel`] drives the panel and reads the touch controller.
//! - [`lane`] is a queue between two tasks.
//! - [`light`] drives the front light.
//! - [`network`] makes the configuration of the Wi-Fi driver.
//!
//! Each binary includes the build-time configuration from `cfg.toml`
//! (`$OUT_DIR/config.rs`) as its own private module, so the passwords are
//! not public items of this library.
//!
//! The binaries are `dashboard` (the firmware) and `bringup` (the hardware
//! checks for the first start of a board).

#![no_std]
// The executor of Embassy runs all tasks on one thread, so no future here
// needs to be `Send`.
#![allow(clippy::future_not_send)]

extern crate alloc;

pub mod board;
pub mod lane;
pub mod light;
pub mod network;
pub mod panel;
