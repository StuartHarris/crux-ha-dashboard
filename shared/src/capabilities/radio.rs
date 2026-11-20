//! The radio capability: the Wi-Fi radio work that the core asks the shell to
//! do, and the commands that make its operations.
//!
//! The core decides when to join, which access point to join, when to scan
//! for a better one and when to restart the chip. The shell does only the
//! radio work. The SSID and the password stay in the shell.
//!
//! Each operation, except `Restart`, carries the `radio` id of the core. The
//! core makes a new id for each request, and it ignores an answer with an
//! older id. The core waits for one radio request at a time. When it sends a
//! new request, it no longer waits for an older one: the older request
//! reached the time limit of the core, or the new request stops it.
//!
//! - [`Join`] is a request. The shell associates with an access point and
//!   answers with [`Joined`].
//! - [`Address`] is a request. The shell waits for an IP address and answers
//!   with [`Lease`].
//! - [`Leave`] is a request. The shell leaves the access point and answers
//!   when the address is gone.
//! - [`Scan`] is a request. The shell scans all the channels and answers with
//!   [`Scanned`].
//! - [`StopScan`] is a request. The shell stops a scan and answers.
//! - [`Off`] is a request. The shell turns the radio off and answers.
//! - [`Restart`] is a notification. The shell restarts the chip.
//!
//! The shell must obey these rules:
//!
//! - Each request has a time limit in the shell, which its documentation
//!   gives. The core also has a time limit for each request.
//! - A new request stops each older request that did not get its answer.
//!   The shell answers each stopped request with `Stopped` (or `()`) first,
//!   and then it does the new request. For example, `Off` stops a `Join`,
//!   and a `Leave` stops a `Scan`.

use alloc::{string::String, vec::Vec};

use crux_core::{Command, Request, command::RequestBuilder, macros::Operation};
use facet::Facet;
use serde::{Deserialize, Serialize};

/// An access point: its BSSID and its channel.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessPoint {
    /// The BSSID (the MAC address) of the access point.
    pub bssid: [u8; 6],
    /// The primary channel of the access point.
    pub channel: u8,
}

/// An access point that a scan found.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The access point.
    pub access_point: AccessPoint,
    /// The signal of the access point, in dBm.
    pub rssi: i32,
}

/// The access point that a [`Join`] goes to.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Target {
    /// Join this access point on its channel, with no scan of all the
    /// channels.
    Cached(AccessPoint),
    /// Scan all the channels, and join the access point with the strongest
    /// signal.
    Strongest,
}

/// Associates with an access point of the network.
///
/// If the radio is off, the shell turns it on first. If the device is
/// already associated and `keep` is `true`, the shell answers
/// `Associated { kept: true }` at once. If the device is already associated
/// and `keep` is `false`, the shell leaves the access point first.
///
/// The shell gives up after 8 seconds for a `Cached` target and after 20
/// seconds for `Strongest`. Each limit includes the leave before the join
/// and the disconnect after a failure.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = Joined)]
pub struct Join {
    /// The id of the request in the core.
    pub radio: u32,
    /// The access point to join.
    pub target: Target,
    /// If `true`, the shell keeps an association that it has.
    pub keep: bool,
}

/// The answer to a [`Join`] request.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Joined {
    /// The device is associated with an access point.
    Associated {
        /// The access point.
        access_point: AccessPoint,
        /// The signal after the join, in dBm, if the shell can read it.
        rssi: Option<i32>,
        /// If `true`, the device was already associated, and the shell kept
        /// that association.
        kept: bool,
        /// If `true`, the device had an IP address before the join. A lease
        /// after such a join can be a stale lease.
        had_address: bool,
    },
    /// The join failed, for this reason.
    Failed(String),
    /// A newer request stopped the join.
    Stopped,
}

/// Waits for an IP address from DHCP.
///
/// The shell gives up after 20 seconds.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = Lease)]
pub struct Address {
    /// The id of the request in the core.
    pub radio: u32,
}

/// The answer to an [`Address`] request.
#[derive(Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Lease {
    /// The device has an IP address.
    Leased,
    /// The device did not get an IP address in the time limit.
    NoLease,
    /// A newer request stopped the wait.
    Stopped,
}

/// Leaves the access point.
///
/// The shell disconnects (time limit 1 second), and then waits until the IP
/// address is gone (time limit 2 seconds). If the device is not associated,
/// the shell answers at once.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct Leave {
    /// The id of the request in the core.
    pub radio: u32,
}

/// Scans all the channels for the access points of the network.
///
/// The link can drop for a short time during the scan. The shell gives up
/// after 10 seconds.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = Scanned)]
pub struct Scan {
    /// The id of the request in the core.
    pub radio: u32,
}

/// The answer to a [`Scan`] request.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Scanned {
    /// The scan is complete.
    Found {
        /// The access points of the network that the scan found.
        seen: Vec<Seen>,
        /// The signal of the current access point after the scan, in dBm,
        /// if the shell can read it.
        rssi: Option<i32>,
    },
    /// The scan failed, for this reason.
    Failed(String),
    /// A newer request stopped the scan.
    Stopped,
}

/// Stops a scan.
///
/// The shell stops the scan at once. The `Scan` request gets `Stopped`
/// first, and then the shell answers this request. If no scan runs, the
/// shell answers at once.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct StopScan {
    /// The id of the request in the core.
    pub radio: u32,
}

/// Turns the radio off.
///
/// The shell stops the request that runs, leaves the access point, waits
/// 100 ms for the last frames to go, and then turns the radio off. If the
/// radio is already off, the shell answers at once.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct Off {
    /// The id of the request in the core.
    pub radio: u32,
}

/// Restarts the chip.
///
/// The core sends this notification when the radio driver does not
/// recover. The shell writes `reason` to its log, waits 200 ms for the log
/// to go, and restarts the chip. A shell that cannot restart the chip
/// starts a new core.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(notify)]
pub struct Restart {
    /// Why the core restarts the chip, for the log of the shell.
    pub reason: String,
}

/// Makes the commands for the radio operations.
pub struct Radio;

impl Radio {
    /// Joins `target` for the request `radio`. The request gives the
    /// result. Refer to [`Join`].
    #[must_use]
    pub fn join<Effect, Event>(
        radio: u32,
        target: Target,
        keep: bool,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = Joined>>
    where
        Effect: From<Request<Join>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Join {
            radio,
            target,
            keep,
        })
    }

    /// Waits for an IP address for the request `radio`. The request gives
    /// the result.
    #[must_use]
    pub fn address<Effect, Event>(
        radio: u32,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = Lease>>
    where
        Effect: From<Request<Address>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Address { radio })
    }

    /// Leaves the access point for the request `radio`. The request ends
    /// when the address is gone.
    #[must_use]
    pub fn leave<Effect, Event>(
        radio: u32,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<Leave>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Leave { radio })
    }

    /// Scans for access points for the request `radio`. The request gives
    /// the result.
    #[must_use]
    pub fn scan<Effect, Event>(
        radio: u32,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = Scanned>>
    where
        Effect: From<Request<Scan>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Scan { radio })
    }

    /// Stops a scan for the request `radio`. The request ends when the scan
    /// stopped.
    #[must_use]
    pub fn stop_scan<Effect, Event>(
        radio: u32,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<StopScan>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(StopScan { radio })
    }

    /// Turns the radio off for the request `radio`. The request ends when
    /// the radio is off.
    #[must_use]
    pub fn off<Effect, Event>(radio: u32) -> RequestBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<Off>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Off { radio })
    }

    /// Restarts the chip, and gives `reason` to the log of the shell. Refer
    /// to [`Restart`].
    pub fn restart<Effect, Event>(reason: String) -> Command<Effect, Event>
    where
        Effect: From<Request<Restart>> + Send + 'static,
        Event: Send + 'static,
    {
        super::notify(Restart { reason })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec, vec::Vec};
    use core::fmt::Debug;

    use crux_core::{Command, Request};
    use serde::{Serialize, de::DeserializeOwned};

    use super::{
        AccessPoint, Address, Join, Joined, Lease, Leave, Off, Radio, Restart, Scan, Scanned, Seen,
        StopScan, Target,
    };

    const HALL: AccessPoint = AccessPoint {
        bssid: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
        channel: 6,
    };

    const KITCHEN: AccessPoint = AccessPoint {
        bssid: [0x02, 0x66, 0x77, 0x88, 0x99, 0xAA],
        channel: 11,
    };

    /// The effects of the radio capability, for the tests only.
    enum Effect {
        Join(Request<Join>),
        Address(Request<Address>),
        Leave(Request<Leave>),
        Scan(Request<Scan>),
        StopScan(Request<StopScan>),
        Off(Request<Off>),
        Restart(Request<Restart>),
    }

    macro_rules! from_request {
        ($($operation:ident),*) => {$(
            impl From<Request<$operation>> for Effect {
                fn from(request: Request<$operation>) -> Self {
                    Self::$operation(request)
                }
            }
        )*};
    }

    from_request!(Join, Address, Leave, Scan, StopScan, Off, Restart);

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Joined(Joined),
        Lease(Lease),
        Scanned(Scanned),
        Done,
    }

    /// Returns the one effect of `command`.
    fn effect(command: &mut Command<Effect, Event>) -> Effect {
        let mut effects: Vec<Effect> = command.effects().collect();
        assert_eq!(effects.len(), 1, "one effect");
        effects.remove(0)
    }

    /// Checks that `value` is the same after a JSON round trip.
    fn round_trip<T: Serialize + DeserializeOwned + Debug + PartialEq>(value: &T) {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&back, value, "{json}");
    }

    #[test]
    fn join_gives_the_result() {
        let answers = [
            Joined::Associated {
                access_point: HALL,
                rssi: Some(-58),
                kept: false,
                had_address: true,
            },
            Joined::Failed("authentication failed".into()),
            Joined::Stopped,
        ];
        for joined in answers {
            let mut command = Radio::join(3, Target::Cached(HALL), true).then_send(Event::Joined);
            let Effect::Join(mut request) = effect(&mut command) else {
                panic!("a Join request");
            };
            assert_eq!(
                request.operation,
                Join {
                    radio: 3,
                    target: Target::Cached(HALL),
                    keep: true,
                }
            );
            request
                .resolve(joined.clone())
                .expect("the core waits for the join");
            let events: Vec<Event> = command.events().collect();
            assert_eq!(events, [Event::Joined(joined)]);
            assert!(command.is_done());
        }
    }

    #[test]
    fn address_gives_the_lease() {
        for lease in [Lease::Leased, Lease::NoLease, Lease::Stopped] {
            let mut command = Radio::address(4).then_send(Event::Lease);
            let Effect::Address(mut request) = effect(&mut command) else {
                panic!("an Address request");
            };
            assert_eq!(request.operation, Address { radio: 4 });
            request
                .resolve(lease)
                .expect("the core waits for the lease");
            let events: Vec<Event> = command.events().collect();
            assert_eq!(events, [Event::Lease(lease)]);
            assert!(command.is_done());
        }
    }

    #[test]
    fn scan_gives_the_access_points_and_the_signal() {
        let answers = [
            Scanned::Found {
                seen: vec![
                    Seen {
                        access_point: HALL,
                        rssi: -70,
                    },
                    Seen {
                        access_point: KITCHEN,
                        rssi: -52,
                    },
                ],
                rssi: None,
            },
            Scanned::Failed("driver busy".into()),
            Scanned::Stopped,
        ];
        for scanned in answers {
            let mut command = Radio::scan(5).then_send(Event::Scanned);
            let Effect::Scan(mut request) = effect(&mut command) else {
                panic!("a Scan request");
            };
            assert_eq!(request.operation, Scan { radio: 5 });
            request
                .resolve(scanned.clone())
                .expect("the core waits for the scan");
            let events: Vec<Event> = command.events().collect();
            assert_eq!(events, [Event::Scanned(scanned)]);
            assert!(command.is_done());
        }
    }

    #[test]
    fn leave_stop_scan_and_off_end_when_the_shell_answers() {
        let mut leave = Radio::leave(6).then_send(|()| Event::Done);
        let mut stop_scan = Radio::stop_scan(7).then_send(|()| Event::Done);
        let mut off = Radio::off(8).then_send(|()| Event::Done);

        let Effect::Leave(mut request) = effect(&mut leave) else {
            panic!("a Leave request");
        };
        assert_eq!(request.operation, Leave { radio: 6 });
        request.resolve(()).expect("the core waits for the leave");

        let Effect::StopScan(mut request) = effect(&mut stop_scan) else {
            panic!("a StopScan request");
        };
        assert_eq!(request.operation, StopScan { radio: 7 });
        request.resolve(()).expect("the core waits for the stop");

        let Effect::Off(mut request) = effect(&mut off) else {
            panic!("an Off request");
        };
        assert_eq!(request.operation, Off { radio: 8 });
        request
            .resolve(())
            .expect("the core waits for the radio off");

        for mut command in [leave, stop_scan, off] {
            let events: Vec<Event> = command.events().collect();
            assert_eq!(events, [Event::Done]);
            assert!(command.is_done());
        }
    }

    #[test]
    fn restart_is_a_notification_with_its_reason() {
        let mut command: Command<Effect, Event> =
            Radio::restart(String::from("no address after 3 joins"));
        let Effect::Restart(request) = effect(&mut command) else {
            panic!("a Restart notification");
        };
        assert_eq!(
            request.operation,
            Restart {
                reason: "no address after 3 joins".into(),
            }
        );
        assert!(command.events().next().is_none());
        assert!(command.is_done());
    }

    #[test]
    fn operations_and_answers_survive_a_round_trip() {
        round_trip(&Join {
            radio: 1,
            target: Target::Cached(HALL),
            keep: false,
        });
        round_trip(&Join {
            radio: 2,
            target: Target::Strongest,
            keep: true,
        });
        round_trip(&Address { radio: 3 });
        round_trip(&Leave { radio: 4 });
        round_trip(&Scan { radio: 5 });
        round_trip(&StopScan { radio: 6 });
        round_trip(&Off { radio: 7 });
        round_trip(&Restart {
            reason: "stale lease".into(),
        });
        round_trip(&Joined::Associated {
            access_point: KITCHEN,
            rssi: None,
            kept: true,
            had_address: false,
        });
        round_trip(&Joined::Failed("no access point".into()));
        round_trip(&Joined::Stopped);
        round_trip(&Lease::Leased);
        round_trip(&Lease::NoLease);
        round_trip(&Lease::Stopped);
        round_trip(&Scanned::Found {
            seen: vec![Seen {
                access_point: KITCHEN,
                rssi: -49,
            }],
            rssi: Some(-63),
        });
        round_trip(&Scanned::Failed("driver busy".into()));
        round_trip(&Scanned::Stopped);
    }
}
