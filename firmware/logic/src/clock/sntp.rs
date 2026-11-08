//! The packets of an SNTP client (RFC 4330).
//!
//! The client sends one 48-byte request in a UDP datagram to port 123. The
//! transmit timestamp of the request is a random number (the nonce). The
//! server copies it to the originate timestamp of its reply, so the client
//! can find the reply to its request.
//!
//! The client uses the transmit timestamp of the server as the time when the
//! reply arrives. The client does no delay compensation. Thus the clock can
//! be early by the one-way delay from the server to the device. This delay
//! is usually less than 100 ms, and the dashboard shows minutes.
//!
//! The client ignores a leap second warning (LI 1 or 2). On the day of a
//! leap second, the clock can be wrong by 1 second until the next request.
//! The dashboard shows minutes, so this error is acceptable.
//!
//! The client checks the originate timestamp first. A reply with a
//! different originate timestamp is for a different request, so the client
//! ignores it and waits for its own reply ([`Rejected::ends_request`]).

/// The UDP port of an SNTP server.
pub const PORT: u16 = 123;
/// The length of a request and of a reply with no extensions.
pub const PACKET_LEN: usize = 48;
/// The seconds from 1900-01-01 (the NTP epoch) to 1970-01-01.
pub const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// The first byte of a request: LI 0 (no warning), VN 4, mode 3 (client).
const REQUEST_HEADER: u8 = (4 << 3) | 3;
/// The mode of a reply from a server.
const MODE_SERVER: u8 = 4;
/// The LI value that tells that the server clock is not synchronized.
const LI_ALARM: u8 = 3;

/// A time after the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnixTime {
    /// The whole seconds.
    pub seconds: u64,
    /// The nanoseconds after the whole seconds, less than 1,000,000,000.
    pub nanos: u32,
}

/// Why the client does not use a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The reply has less than 48 bytes.
    Short,
    /// The mode is not 4 (server).
    Mode(u8),
    /// The stratum is not 1 to 15. Stratum 0 is a "kiss-o'-death" packet.
    Stratum(u8),
    /// The leap indicator is 3: the server clock is not synchronized.
    NotSynchronized,
    /// The originate timestamp is not the nonce of the request. The reply is
    /// for a different request.
    NotOurs,
    /// The transmit timestamp is 0, or it is before 1970.
    NoTime,
}

impl Rejected {
    /// Returns `true` if the reply is the answer to the request, and the
    /// request failed. Returns `false` for a datagram that is not the answer
    /// ([`Rejected::Short`] or [`Rejected::NotOurs`]). The client ignores
    /// such a datagram and continues to wait.
    ///
    /// For example, a "kiss-o'-death" packet (stratum 0) with the nonce of
    /// the request ends the request.
    #[must_use]
    pub const fn ends_request(self) -> bool {
        !matches!(self, Self::Short | Self::NotOurs)
    }
}

/// Returns the request with `nonce` as its transmit timestamp.
#[must_use]
pub fn request(nonce: u64) -> [u8; PACKET_LEN] {
    let mut packet = [0; PACKET_LEN];
    packet[0] = REQUEST_HEADER;
    packet[40..48].copy_from_slice(&nonce.to_be_bytes());
    packet
}

/// Checks the reply to the request with `nonce`, and returns the transmit
/// time of the server.
///
/// # Errors
///
/// Returns the reason if the reply is not a valid answer to the request.
pub fn parse(reply: &[u8], nonce: u64) -> Result<UnixTime, Rejected> {
    if reply.len() < PACKET_LEN {
        return Err(Rejected::Short);
    }
    if timestamp(reply, 24) != nonce {
        return Err(Rejected::NotOurs);
    }
    let leap = reply[0] >> 6;
    let mode = reply[0] & 0x07;
    let stratum = reply[1];
    if mode != MODE_SERVER {
        return Err(Rejected::Mode(mode));
    }
    if !(1..=15).contains(&stratum) {
        return Err(Rejected::Stratum(stratum));
    }
    if leap == LI_ALARM {
        return Err(Rejected::NotSynchronized);
    }
    to_unix(timestamp(reply, 40)).ok_or(Rejected::NoTime)
}

/// Returns the 64-bit timestamp at `offset` in `packet`.
fn timestamp(packet: &[u8], offset: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&packet[offset..offset + 8]);
    u64::from_be_bytes(bytes)
}

/// Converts an NTP timestamp (32 bits of seconds and 32 bits of fraction)
/// to a Unix time. Returns `None` for 0, and for a time before 1970.
///
/// The seconds wrap in February 2036. As RFC 4330 tells, a timestamp with
/// bit 0 (the most significant bit) set is from 1968 to 2036, and a
/// timestamp with bit 0 clear is from 2036 to 2104.
fn to_unix(timestamp: u64) -> Option<UnixTime> {
    if timestamp == 0 {
        return None;
    }
    let ntp_seconds = timestamp >> 32;
    let fraction = timestamp & 0xFFFF_FFFF;
    let ntp_seconds = if ntp_seconds & 0x8000_0000 == 0 {
        ntp_seconds + (1 << 32)
    } else {
        ntp_seconds
    };
    let seconds = ntp_seconds.checked_sub(NTP_UNIX_OFFSET)?;
    let nanos = u32::try_from((fraction * 1_000_000_000) >> 32).ok()?;
    Some(UnixTime { seconds, nanos })
}

#[cfg(test)]
mod tests {
    use super::{NTP_UNIX_OFFSET, PACKET_LEN, Rejected, UnixTime, parse, request, to_unix};

    const NONCE: u64 = 0x0123_4567_89AB_CDEF;
    /// 2026-11-08 13:45:30 UTC, in NTP seconds.
    const NTP_SECONDS: u64 = 1_794_145_530 + NTP_UNIX_OFFSET;

    /// A valid reply from a stratum 2 server, with a transmit time of
    /// 2026-11-08 13:45:30.25 UTC.
    fn reply() -> [u8; PACKET_LEN] {
        let mut packet = [0; PACKET_LEN];
        packet[0] = (4 << 3) | 4;
        packet[1] = 2;
        packet[24..32].copy_from_slice(&NONCE.to_be_bytes());
        let receive = (NTP_SECONDS << 32) | 0x3000_0000;
        packet[32..40].copy_from_slice(&receive.to_be_bytes());
        let transmit = (NTP_SECONDS << 32) | 0x4000_0000;
        packet[40..48].copy_from_slice(&transmit.to_be_bytes());
        packet
    }

    #[test]
    fn the_request_has_the_header_and_the_nonce() {
        let packet = request(NONCE);
        assert_eq!(packet[0], 0x23);
        assert!(packet[1..40].iter().all(|byte| *byte == 0));
        assert_eq!(packet[40..48], NONCE.to_be_bytes());
    }

    #[test]
    fn a_known_reply() {
        assert_eq!(
            parse(&reply(), NONCE),
            Ok(UnixTime {
                seconds: 1_794_145_530,
                nanos: 250_000_000,
            })
        );
        // A reply with an extension is longer.
        let mut long = [0; 68];
        long[..PACKET_LEN].copy_from_slice(&reply());
        assert!(parse(&long, NONCE).is_ok());
    }

    #[test]
    fn a_leap_warning_is_not_an_error() {
        let mut packet = reply();
        packet[0] |= 1 << 6;
        assert!(parse(&packet, NONCE).is_ok());
    }

    #[test]
    fn a_short_reply_is_rejected() {
        assert_eq!(parse(&reply()[..47], NONCE), Err(Rejected::Short));
    }

    #[test]
    fn a_reply_that_is_not_from_a_server_is_rejected() {
        let mut packet = reply();
        packet[0] = (4 << 3) | 3;
        assert_eq!(parse(&packet, NONCE), Err(Rejected::Mode(3)));
    }

    #[test]
    fn a_bad_stratum_is_rejected() {
        for stratum in [0, 16, 255] {
            let mut packet = reply();
            packet[1] = stratum;
            assert_eq!(parse(&packet, NONCE), Err(Rejected::Stratum(stratum)));
        }
        let mut packet = reply();
        packet[1] = 15;
        assert!(parse(&packet, NONCE).is_ok());
    }

    #[test]
    fn a_server_that_is_not_synchronized_is_rejected() {
        let mut packet = reply();
        packet[0] |= 3 << 6;
        assert_eq!(parse(&packet, NONCE), Err(Rejected::NotSynchronized));
    }

    #[test]
    fn a_reply_to_another_request_is_rejected() {
        assert_eq!(parse(&reply(), NONCE + 1), Err(Rejected::NotOurs));
        // The nonce is checked first, so a bad packet for another request
        // is also only `NotOurs`.
        let mut packet = reply();
        packet[1] = 0;
        assert_eq!(parse(&packet, NONCE + 1), Err(Rejected::NotOurs));
    }

    #[test]
    fn only_an_answer_to_the_request_ends_it() {
        assert!(!Rejected::Short.ends_request());
        assert!(!Rejected::NotOurs.ends_request());
        // A kiss-o'-death packet with the nonce of the request.
        let mut packet = reply();
        packet[1] = 0;
        let rejected = parse(&packet, NONCE).unwrap_err();
        assert_eq!(rejected, Rejected::Stratum(0));
        assert!(rejected.ends_request());
        assert!(Rejected::Mode(3).ends_request());
        assert!(Rejected::NotSynchronized.ends_request());
        assert!(Rejected::NoTime.ends_request());
    }

    #[test]
    fn a_reply_with_no_time_is_rejected() {
        let mut packet = reply();
        packet[40..48].fill(0);
        assert_eq!(parse(&packet, NONCE), Err(Rejected::NoTime));
    }

    #[test]
    fn timestamps_after_2036_use_the_next_era() {
        // 2036-02-07 06:28:16.5 UTC is 0.5 seconds in era 1.
        assert_eq!(
            to_unix(0x8000_0000),
            Some(UnixTime {
                seconds: 2_085_978_496,
                nanos: 500_000_000,
            })
        );
        assert_eq!(
            to_unix(100 << 32).map(|time| time.seconds),
            Some((1 << 32) + 100 - NTP_UNIX_OFFSET)
        );
        // 1968 is before the Unix epoch.
        assert_eq!(to_unix(0x8000_0000 << 32), None);
    }

    #[test]
    fn the_fraction_converts_to_nanos() {
        let time = |fraction: u64| to_unix((NTP_SECONDS << 32) | fraction).unwrap().nanos;
        assert_eq!(time(0), 0);
        assert_eq!(time(0x8000_0000), 500_000_000);
        assert_eq!(time(0xFFFF_FFFF), 999_999_999);
    }
}
