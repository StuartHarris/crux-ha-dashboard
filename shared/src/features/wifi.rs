//! The Wi-Fi feature: the Wi-Fi policy of the core.
//!
//! The core decides when to join, which access point to join, when to scan
//! for a better one and when to restart the chip. The shell does only the
//! radio work. Refer to [`crate::capabilities::radio`].
//!
//! [`rules`] has the pure rules. The state machine that uses them comes in a
//! later change.

pub mod rules;

use core::time::Duration;

/// The time limit of the core for a `Join` request. The shell gives up
/// after 20 seconds at most.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(25);

/// The time limit of the core for an `Address` request. The shell gives up
/// after 20 seconds.
pub const ADDRESS_TIMEOUT: Duration = Duration::from_secs(25);

/// The time limit of the core for a `Leave` request. The shell needs 3
/// seconds at most.
pub const LEAVE_TIMEOUT: Duration = Duration::from_secs(7);

/// The time limit of the core for a `Scan` request. The shell gives up
/// after 10 seconds.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// The time limit of the core for a `StopScan` request. The shell answers
/// at once.
pub const STOP_SCAN_TIMEOUT: Duration = Duration::from_secs(3);

/// The time limit of the core for an `Off` request. The shell needs about
/// 100 ms.
pub const OFF_TIMEOUT: Duration = Duration::from_secs(3);
