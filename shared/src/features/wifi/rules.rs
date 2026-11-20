//! The pure rules of the Wi-Fi policy:
//!
//! - when a join uses the access point of the last join, and when the core
//!   scans for a better one ([`ApCache`]);
//! - when the core changes to a better access point
//!   ([`better_access_point`]);
//! - when the core leaves and joins again after an MQTT session failed, and
//!   when it restarts the chip ([`Health`]).
//!
//! A join first goes to the access point of the last join, on its channel,
//! with no scan of all the channels. This join is fast. When the link is
//! quiet, the core asks for one scan of all the channels, and changes to an
//! access point that is clearly better. If the join to the cached access
//! point fails, the core joins the access point with the strongest signal.

use crate::{
    capabilities::radio::{AccessPoint, Seen, Target},
    mqtt::End,
};

/// What the core does after a join that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterFailure {
    /// The join with the cached access point failed. Join the access point
    /// with the strongest signal, with no backoff.
    Rescan,
    /// The join with the strongest access point failed. Wait for the backoff
    /// of the connection, and then try again.
    Fail,
}

/// The access point of the last good join, and if a scan for a better one is
/// due.
///
/// The core keeps the cache in its model, which light sleep keeps. The
/// rules:
///
/// - The next join goes to the cached access point on its channel, with no
///   scan of all the channels.
/// - If a join with the cached access point fails (also when DHCP gives no
///   address), the core forgets the access point and joins the strongest
///   access point.
/// - After a join with the cached access point, one scan for a better access
///   point is due. The core does it when the link is quiet. A join to the
///   strongest access point already found the best access point, so no scan
///   is due after it. No scan is due after a change of access point
///   ([`ApCache::roam_to`]) either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApCache {
    access_point: Option<AccessPoint>,
    scan_due: bool,
    roaming: bool,
}

impl ApCache {
    /// Returns the target of the next join.
    #[must_use]
    pub fn plan(&self) -> Target {
        self.access_point.map_or(Target::Strongest, Target::Cached)
    }

    /// Returns the access point of the last good join.
    #[must_use]
    pub const fn current(&self) -> Option<AccessPoint> {
        self.access_point
    }

    /// Records a join to `target` that worked, to `access_point`.
    ///
    /// After a join with the cached access point, a scan is due. After the
    /// join of a change of access point, no scan is due.
    pub const fn joined(&mut self, target: Target, access_point: AccessPoint) {
        self.access_point = Some(access_point);
        self.scan_due = matches!(target, Target::Cached(_)) && !self.roaming;
        self.roaming = false;
    }

    /// Records a join to `target` that failed, and returns what the core does
    /// next.
    pub const fn failed(&mut self, target: Target) -> AfterFailure {
        self.access_point = None;
        self.scan_due = false;
        self.roaming = false;
        match target {
            Target::Cached(_) => AfterFailure::Rescan,
            Target::Strongest => AfterFailure::Fail,
        }
    }

    /// Records a change to the access point `best` that a scan found. The
    /// next join goes to `best`, and no scan is due after that join, because
    /// the core just scanned.
    pub const fn roam_to(&mut self, best: AccessPoint) {
        self.access_point = Some(best);
        self.scan_due = false;
        self.roaming = true;
    }

    /// Returns `true` if a scan for a better access point is due: after a
    /// join with the cached access point, until a scan completes or fails.
    /// A scan that a request stops does not count, so the scan stays due.
    #[must_use]
    pub const fn scan_due(&self) -> bool {
        self.scan_due
    }

    /// Records a scan for a better access point that completed or failed.
    pub const fn scanned(&mut self) {
        self.scan_due = false;
    }
}

/// The margin in dB by which another access point must be stronger than the
/// current one before the core changes to it.
///
/// The signal of each access point changes by some dB from one scan to the
/// next. Without a margin, the device can change between two access points
/// with almost the same signal. A change closes the MQTT session for a
/// short time.
pub const ROAM_MARGIN_DB: i32 = 10;

/// Returns the access point to change to, or `None` to stay.
///
/// `current` is the access point of the link, and `rssi` its signal in dBm
/// from the shell. If the shell gives no signal, the function uses the
/// signal of `current` in the scan. If there is no signal for `current`, the
/// function returns `None`, because it cannot compare. The function returns
/// the strongest other access point in `seen` only if its signal is at least
/// [`ROAM_MARGIN_DB`] stronger.
#[must_use]
pub fn better_access_point(current: AccessPoint, rssi: Option<i32>, seen: &[Seen]) -> Option<Seen> {
    let rssi = rssi.or_else(|| {
        seen.iter()
            .filter(|found| found.access_point.bssid == current.bssid)
            .map(|found| found.rssi)
            .max()
    })?;
    seen.iter()
        .filter(|found| found.access_point.bssid != current.bssid)
        .max_by_key(|found| found.rssi)
        .filter(|best| best.rssi >= rssi + ROAM_MARGIN_DB)
        .copied()
}

/// The number of joins with no address from DHCP, after an MQTT session
/// failed, before the core restarts the chip.
///
/// The Wi-Fi driver of `esp-radio` 1.0.0-beta.1 counts the frames that it did
/// not send yet. A frame that the driver drops while the link is down stays in
/// that count. When the count is full, the network stack can send and receive
/// nothing. A new join does not reset the count, and only a restart of the
/// chip does. The join itself works, but DHCP gets no address.
///
/// The value 3 is the join to the cached access point, the join to the
/// strongest access point, and one more join after the backoff of the
/// connection. Each join waits up to 20 s for DHCP, so the restart comes
/// about one minute after the session failed.
pub const RESTART_AFTER: u32 = 3;

/// What the core does after a join that got no address, or a stale one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Continue. The core leaves and joins again, or keeps the stale
    /// address.
    Continue,
    /// The link is stuck. Restart the chip.
    Restart,
}

/// Returns `true` if a session that ended with `end` makes the link suspect.
///
/// Only a timer that expired and a stream that closed are failures of the
/// link. A broker that refuses the connection, a packet that is not valid,
/// a session that the broker kept and a field that is too long are problems
/// of the broker or of the configuration. A clean end is not a failure.
#[must_use]
pub const fn fails_the_link(end: &End) -> bool {
    matches!(end, End::Timeout(_) | End::Transport(_))
}

/// The health of the link.
///
/// The rules:
///
/// - An MQTT session that fails makes the link suspect ([`Health::failed`],
///   [`Health::ended`]). Refer to [`fails_the_link`].
/// - While the link is suspect, a join with the device associated leaves and
///   joins again ([`Health::keep`] is `false`), so the core does not keep a
///   link that is stuck.
/// - While the link is suspect, each join that gets no address from DHCP
///   counts ([`Health::no_lease`]). At [`RESTART_AFTER`], the core restarts
///   the chip.
/// - The first bytes from the broker on a stream prove that the link moves
///   data in both directions. They clear the suspicion and the count
///   ([`Health::traffic`]). A new address from DHCP also proves traffic, so
///   it clears the count ([`Health::leased`]).
/// - If the network stack kept its old address through the join, the address
///   proves nothing. It counts as a join with no address
///   ([`Health::stale`]), but the core keeps the address.
///
/// A join that fails before DHCP (the access point is off, or it refuses
/// the device) does not count, because the cause is not on the device. The
/// core makes a new `Health` when it starts, as after a restart of the chip,
/// so after a restart the link is not suspect. Then the next restart needs
/// an address, a session that fails, and [`RESTART_AFTER`] joins with no
/// address. Thus the chip cannot restart again and again while DHCP or the
/// access point is down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Health {
    suspect: bool,
    no_lease: u32,
}

impl Health {
    /// Records an MQTT session that failed.
    pub const fn failed(&mut self) {
        self.suspect = true;
    }

    /// Records an MQTT session that ended with `end`. Only a failure of the
    /// link counts. Refer to [`fails_the_link`].
    pub const fn ended(&mut self, end: &End) {
        if fails_the_link(end) {
            self.failed();
        }
    }

    /// Records the first bytes from the broker on a stream.
    pub const fn traffic(&mut self) {
        self.suspect = false;
        self.no_lease = 0;
    }

    /// Records a new address from DHCP after a join.
    pub const fn leased(&mut self) {
        self.no_lease = 0;
    }

    /// Returns `true` if an MQTT session failed after the last proof of
    /// traffic.
    #[must_use]
    pub const fn suspect(&self) -> bool {
        self.suspect
    }

    /// Returns `true` if a join keeps an association that the device has.
    /// While the link is suspect, the join leaves and joins again.
    #[must_use]
    pub const fn keep(&self) -> bool {
        !self.suspect
    }

    /// Records a join to an access point that got no address from DHCP, and
    /// returns what the core does next.
    pub const fn no_lease(&mut self) -> Recovery {
        if !self.suspect {
            return Recovery::Continue;
        }
        self.no_lease += 1;
        if self.no_lease >= RESTART_AFTER {
            Recovery::Restart
        } else {
            Recovery::Continue
        }
    }

    /// Records a join after which the network stack has the address that it
    /// had before the join. The stack did not see the link go down, so it can
    /// be stuck. Counts as [`Health::no_lease`].
    pub const fn stale(&mut self) -> Recovery {
        self.no_lease()
    }

    /// Returns the number of joins with no address that counted.
    #[must_use]
    pub const fn joins_with_no_lease(&self) -> u32 {
        self.no_lease
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{
        AfterFailure, ApCache, Health, RESTART_AFTER, ROAM_MARGIN_DB, Recovery,
        better_access_point, fails_the_link,
    };
    use crate::{
        capabilities::radio::{AccessPoint, Seen, Target},
        mqtt::{End, TimerKind},
    };

    const NEAR: AccessPoint = AccessPoint {
        bssid: [0x24, 0x5a, 0x4c, 0x01, 0x02, 0x03],
        channel: 6,
    };
    const FAR: AccessPoint = AccessPoint {
        bssid: [0x24, 0x5a, 0x4c, 0x0a, 0x0b, 0x0c],
        channel: 11,
    };
    const OTHER: AccessPoint = AccessPoint {
        bssid: [0x24, 0x5a, 0x4c, 0x0d, 0x0e, 0x0f],
        channel: 1,
    };

    const fn seen(access_point: AccessPoint, rssi: i32) -> Seen {
        Seen { access_point, rssi }
    }

    #[test]
    fn the_first_join_goes_to_the_strongest_and_then_no_scan_is_due() {
        let mut cache = ApCache::default();
        assert_eq!(cache.plan(), Target::Strongest);
        cache.joined(Target::Strongest, NEAR);
        assert!(!cache.scan_due());
        assert_eq!(cache.plan(), Target::Cached(NEAR));
    }

    #[test]
    fn a_cached_join_makes_one_scan_due() {
        let mut cache = ApCache::default();
        cache.joined(Target::Strongest, NEAR);
        cache.joined(Target::Cached(NEAR), NEAR);
        assert!(cache.scan_due());
        cache.scanned();
        assert!(!cache.scan_due());
    }

    #[test]
    fn a_scan_that_did_not_complete_stays_due() {
        let mut cache = ApCache::default();
        cache.joined(Target::Cached(NEAR), NEAR);
        // A request stops the scan, or the link is down, so the core does
        // not record the scan.
        assert!(cache.scan_due());
        // A join that keeps the association does not change the cache, so
        // the scan stays due for the next quiet link.
        assert_eq!(cache.plan(), Target::Cached(NEAR));
        assert!(cache.scan_due());
    }

    #[test]
    fn a_roam_joins_the_better_access_point_with_no_scan_due() {
        let mut cache = ApCache::default();
        cache.joined(Target::Cached(NEAR), NEAR);
        assert!(cache.scan_due());
        cache.roam_to(FAR);
        assert!(!cache.scan_due());
        assert_eq!(cache.plan(), Target::Cached(FAR));
        cache.joined(Target::Cached(FAR), FAR);
        assert!(!cache.scan_due());
        assert_eq!(cache.current(), Some(FAR));
        // The next cached join is not a roam, so a scan is due again.
        cache.joined(Target::Cached(FAR), FAR);
        assert!(cache.scan_due());
    }

    #[test]
    fn a_roam_that_fails_forgets_the_cache_and_goes_to_the_strongest() {
        let mut cache = ApCache::default();
        cache.joined(Target::Cached(NEAR), NEAR);
        cache.roam_to(FAR);
        assert_eq!(cache.failed(Target::Cached(FAR)), AfterFailure::Rescan);
        assert_eq!(cache.plan(), Target::Strongest);
        cache.joined(Target::Strongest, NEAR);
        cache.joined(Target::Cached(NEAR), NEAR);
        assert!(cache.scan_due());
    }

    #[test]
    fn a_failed_cached_join_forgets_the_cache_and_goes_to_the_strongest() {
        let mut cache = ApCache::default();
        cache.joined(Target::Strongest, NEAR);
        assert_eq!(cache.failed(Target::Cached(NEAR)), AfterFailure::Rescan);
        assert_eq!(cache.plan(), Target::Strongest);
        assert_eq!(cache.current(), None);
    }

    #[test]
    fn a_failed_join_to_the_strongest_fails() {
        let mut cache = ApCache::default();
        assert_eq!(cache.failed(Target::Strongest), AfterFailure::Fail);
        assert_eq!(cache.plan(), Target::Strongest);
    }

    #[test]
    fn a_clearly_stronger_access_point_is_better() {
        let found = [seen(NEAR, -80), seen(FAR, -60), seen(OTHER, -75)];
        assert_eq!(
            better_access_point(NEAR, Some(-80), &found),
            Some(seen(FAR, -60))
        );
    }

    #[test]
    fn an_access_point_at_the_margin_is_better() {
        let found = [seen(FAR, -70 + ROAM_MARGIN_DB)];
        assert_eq!(
            better_access_point(NEAR, Some(-70), &found),
            Some(seen(FAR, -60))
        );
    }

    #[test]
    fn an_access_point_under_the_margin_is_not_better() {
        let found = [seen(NEAR, -70), seen(FAR, -70 + ROAM_MARGIN_DB - 1)];
        assert_eq!(better_access_point(NEAR, Some(-70), &found), None);
    }

    #[test]
    fn the_current_access_point_is_never_better() {
        let found = [seen(NEAR, -40)];
        assert_eq!(better_access_point(NEAR, Some(-80), &found), None);
    }

    #[test]
    fn with_no_reading_the_scan_gives_the_current_signal() {
        let found = [seen(NEAR, -80), seen(FAR, -65)];
        assert_eq!(
            better_access_point(NEAR, None, &found),
            Some(seen(FAR, -65))
        );
    }

    #[test]
    fn with_no_signal_for_the_current_access_point_the_core_stays() {
        let found = [seen(FAR, -40)];
        assert_eq!(better_access_point(NEAR, None, &found), None);
    }

    #[test]
    fn an_empty_scan_stays() {
        assert_eq!(better_access_point(NEAR, Some(-90), &[]), None);
    }

    #[test]
    fn a_healthy_link_keeps_the_association() {
        let health = Health::default();
        assert!(health.keep());
    }

    #[test]
    fn after_a_failed_session_a_join_leaves_and_joins_again() {
        let mut health = Health::default();
        health.failed();
        assert!(!health.keep());
        // The rejoin got an address, but the link is suspect until the first
        // bytes from the broker.
        health.leased();
        assert!(!health.keep());
        health.traffic();
        assert!(health.keep());
    }

    #[test]
    fn a_stuck_link_restarts_the_chip_after_the_limit() {
        // The session stops, the rejoin and the retries get no address.
        let mut health = Health::default();
        health.failed();
        for _ in 1..RESTART_AFTER {
            assert_eq!(health.no_lease(), Recovery::Continue);
        }
        assert_eq!(health.no_lease(), Recovery::Restart);
    }

    #[test]
    fn an_address_starts_the_count_again() {
        let mut health = Health::default();
        health.failed();
        for _ in 1..RESTART_AFTER {
            assert_eq!(health.no_lease(), Recovery::Continue);
        }
        health.leased();
        assert_eq!(health.joins_with_no_lease(), 0);
        // The next session fails too, so the link stays suspect.
        health.failed();
        assert_eq!(health.no_lease(), Recovery::Continue);
    }

    #[test]
    fn a_dhcp_outage_with_no_failed_session_never_restarts() {
        // After a restart, or while the broker is up and Wi-Fi is new, the
        // link is not suspect.
        let mut health = Health::default();
        for _ in 0..(RESTART_AFTER * 4) {
            assert_eq!(health.no_lease(), Recovery::Continue);
        }
        assert_eq!(health.joins_with_no_lease(), 0);
    }

    #[test]
    fn a_connack_clears_the_suspicion_and_the_count() {
        let mut health = Health::default();
        health.failed();
        health.no_lease();
        health.traffic();
        assert!(!health.suspect());
        assert_eq!(health.joins_with_no_lease(), 0);
        for _ in 0..(RESTART_AFTER * 2) {
            assert_eq!(health.no_lease(), Recovery::Continue);
        }
    }

    #[test]
    fn an_old_address_after_a_rejoin_does_not_stop_the_restart() {
        // The stack is stuck, so it never drops the address of the old link.
        // Each rejoin "gets" that address, and each new session fails.
        let mut health = Health::default();
        for _ in 1..RESTART_AFTER {
            health.failed();
            assert_eq!(health.stale(), Recovery::Continue);
        }
        health.failed();
        assert_eq!(health.stale(), Recovery::Restart);
    }

    #[test]
    fn only_a_timeout_or_a_transport_end_fails_the_link() {
        let failures = [
            End::Timeout(TimerKind::PingResp),
            End::Transport(String::from("reset by peer")),
        ];
        for end in &failures {
            assert!(fails_the_link(end), "{end:?}");
            let mut health = Health::default();
            health.ended(end);
            assert!(health.suspect(), "{end:?}");
        }
    }

    #[test]
    fn a_refused_or_malformed_session_is_not_a_failure_of_the_link() {
        let others = [
            End::Clean,
            End::Refused(5),
            End::Malformed,
            End::SessionPresent,
            End::TooLong,
        ];
        for end in &others {
            assert!(!fails_the_link(end), "{end:?}");
            let mut health = Health::default();
            health.ended(end);
            assert!(!health.suspect(), "{end:?}");
            assert!(health.keep(), "{end:?}");
        }
    }

    #[test]
    fn a_start_clears_the_health() {
        // The core makes a new `Health` at each start, as a restart of the
        // chip clears its RAM.
        let mut health = Health::default();
        health.failed();
        for _ in 1..RESTART_AFTER {
            assert_eq!(health.no_lease(), Recovery::Continue);
        }
        health = Health::default();
        assert!(health.keep());
        assert_eq!(health.joins_with_no_lease(), 0);
        // A new restart needs a failed session and the full count again.
        assert_eq!(health.no_lease(), Recovery::Continue);
        health.failed();
        for _ in 1..RESTART_AFTER {
            assert_eq!(health.no_lease(), Recovery::Continue);
        }
        assert_eq!(health.no_lease(), Recovery::Restart);
    }
}
