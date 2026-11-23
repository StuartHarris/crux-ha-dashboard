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
//! - [`ReadSignal`] is a request. The shell reads the signal of the current
//!   access point and answers with [`SignalRead`]. It does no radio work.
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

use alloc::{format, string::String, vec::Vec};
use core::fmt;

use crux_core::{
    Command, Request,
    command::{NotificationBuilder, RequestBuilder},
    macros::Operation,
};

use super::log::Area;

/// An access point: its BSSID and its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessPoint {
    /// The BSSID (the MAC address) of the access point.
    pub bssid: [u8; 6],
    /// The primary channel of the access point.
    pub channel: u8,
}

/// Shows the BSSID and the channel, for the log. For example
/// `24:5a:4c:01:02:03 on channel 6`.
impl fmt::Display for AccessPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.bssid.iter().enumerate() {
            if index > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02x}")?;
        }
        write!(f, " on channel {}", self.channel)
    }
}

/// Returns the text of an IPv4 address, for example `192.168.1.50`.
#[must_use]
pub fn ip_text(ip: [u8; 4]) -> String {
    let [a, b, c, d] = ip;
    format!("{a}.{b}.{c}.{d}")
}

/// Returns the text of a MAC address, for example a BSSID, as lowercase hex
/// with colons, for example `24:5a:4c:01:02:03`.
#[must_use]
pub fn mac_text(bssid: [u8; 6]) -> String {
    let bytes: Vec<String> = bssid.iter().map(|byte| format!("{byte:02x}")).collect();
    bytes.join(":")
}

/// Returns the text of a signal for the log: `-62 dBm`, or `no signal` if
/// the shell cannot read the signal.
#[must_use]
pub fn signal(rssi: Option<i32>) -> String {
    rssi.map_or_else(|| "no signal".into(), |rssi| format!("{rssi} dBm"))
}

/// An access point that a scan found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The access point.
    pub access_point: AccessPoint,
    /// The signal of the access point, in dBm.
    pub rssi: i32,
}

/// The access point that a [`Join`] goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
/// The shell answers with the IPv4 address ([`Lease::Leased`]). The core
/// publishes it to Home Assistant. The shell gives up after 20 seconds.
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = Lease)]
pub struct Address {
    /// The id of the request in the core.
    pub radio: u32,
}

/// The answer to an [`Address`] request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    /// The device has this IPv4 address.
    Leased {
        /// The address, for example `[192, 168, 1, 50]`.
        ip: [u8; 4],
    },
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
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct Leave {
    /// The id of the request in the core.
    pub radio: u32,
}

/// Scans all the channels for the access points of the network.
///
/// The link can drop for a short time during the scan. The shell gives up
/// after 10 seconds.
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = Scanned)]
pub struct Scan {
    /// The id of the request in the core.
    pub radio: u32,
}

/// The answer to a [`Scan`] request.
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct StopScan {
    /// The id of the request in the core.
    pub radio: u32,
}

/// Reads the signal of the access point with which the device is
/// associated.
///
/// The shell reads the signal that the driver has now, and answers at once.
/// It sends no frame and does no scan.
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = SignalRead)]
pub struct ReadSignal {
    /// The id of the request in the core.
    pub radio: u32,
}

/// The answer to a [`ReadSignal`] request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalRead {
    /// The signal of the current access point, in dBm, or `None` if the
    /// shell cannot read it, for example because the device is not
    /// associated.
    Read(Option<i32>),
    /// A newer request stopped the read.
    Stopped,
}

/// Turns the radio off.
///
/// The shell stops the request that runs, leaves the access point, waits
/// 100 ms for the last frames to go, and then turns the radio off. If the
/// radio is already off, the shell answers at once.
#[derive(Operation, Clone, Copy, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct Off {
    /// The id of the request in the core.
    pub radio: u32,
}

/// Restarts the chip.
///
/// The core sends this notification when the radio driver does not
/// recover, and when Home Assistant asks for a restart. The shell writes
/// the warning `{area}: restart the chip: {reason}` to its log, waits
/// 200 ms for the log to go, and restarts the chip. A shell that cannot
/// restart the chip starts a new core.
#[derive(Operation, Clone, Debug, PartialEq, Eq)]
#[operation(notify)]
pub struct Restart {
    /// The part of the core that asks for the restart, for the log of the
    /// shell.
    pub area: Area,
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

    /// Reads the signal of the current access point for the request
    /// `radio`. The request gives the signal. Refer to [`ReadSignal`].
    #[must_use]
    pub fn read_signal<Effect, Event>(
        radio: u32,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = SignalRead>>
    where
        Effect: From<Request<ReadSignal>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(ReadSignal { radio })
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

    /// Restarts the chip, and gives `area` and `reason` to the log of the
    /// shell. Refer to [`Restart`].
    #[must_use]
    pub fn restart<Effect, Event>(
        area: Area,
        reason: String,
    ) -> NotificationBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<Restart>> + Send + 'static,
        Event: Send + 'static,
    {
        super::notify(Restart { area, reason })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        string::{String, ToString},
        vec,
        vec::Vec,
    };

    use crux_core::{Command, Request};

    use super::{
        AccessPoint, Address, Area, Join, Joined, Lease, Leave, Off, Radio, ReadSignal, Restart,
        Scan, Scanned, Seen, SignalRead, StopScan, Target, ip_text, mac_text, signal,
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
        ReadSignal(Request<ReadSignal>),
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

    from_request!(
        Join, Address, Leave, Scan, StopScan, ReadSignal, Off, Restart
    );

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Joined(Joined),
        Lease(Lease),
        Scanned(Scanned),
        SignalRead(SignalRead),
        Done,
    }

    /// Returns the one effect of `command`.
    fn effect(command: &mut Command<Effect, Event>) -> Effect {
        let mut effects: Vec<Effect> = command.effects().collect();
        assert_eq!(effects.len(), 1, "one effect");
        effects.remove(0)
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
        for lease in [
            Lease::Leased {
                ip: [192, 168, 1, 50],
            },
            Lease::NoLease,
            Lease::Stopped,
        ] {
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
    fn read_signal_gives_the_signal() {
        for read in [
            SignalRead::Read(Some(-61)),
            SignalRead::Read(None),
            SignalRead::Stopped,
        ] {
            let mut command = Radio::read_signal(9).then_send(Event::SignalRead);
            let Effect::ReadSignal(mut request) = effect(&mut command) else {
                panic!("a ReadSignal request");
            };
            assert_eq!(request.operation, ReadSignal { radio: 9 });
            request
                .resolve(read)
                .expect("the core waits for the signal");
            let events: Vec<Event> = command.events().collect();
            assert_eq!(events, [Event::SignalRead(read)]);
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
    fn restart_is_a_notification_with_its_area_and_reason() {
        let mut command: Command<Effect, Event> =
            Radio::restart(Area::Wifi, String::from("no address after 3 joins")).build();
        let Effect::Restart(request) = effect(&mut command) else {
            panic!("a Restart notification");
        };
        assert_eq!(
            request.operation,
            Restart {
                area: Area::Wifi,
                reason: "no address after 3 joins".into(),
            }
        );
        assert!(command.events().next().is_none());
        assert!(command.is_done());
    }

    #[test]
    fn the_log_shows_an_access_point_and_a_signal() {
        let access_point = AccessPoint {
            bssid: [0x24, 0x5A, 0x4C, 0x01, 0x02, 0x03],
            channel: 6,
        };
        assert_eq!(access_point.to_string(), "24:5a:4c:01:02:03 on channel 6");
        assert_eq!(KITCHEN.to_string(), "02:66:77:88:99:aa on channel 11");
        assert_eq!(signal(Some(-62)), "-62 dBm");
        assert_eq!(signal(None), "no signal");
        assert_eq!(ip_text([192, 168, 1, 50]), "192.168.1.50");
        assert_eq!(mac_text(access_point.bssid), "24:5a:4c:01:02:03");
    }
}
