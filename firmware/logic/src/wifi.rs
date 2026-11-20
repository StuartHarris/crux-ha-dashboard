//! The rules of the Wi-Fi task:
//!
//! - what a join does with a request that arrives during it
//!   ([`during_join`]), and what a change of access point does with one
//!   ([`during_switch`]);
//! - when a join uses the access point of the last join, and when the task
//!   scans for a better one ([`ApCache`]);
//! - when the task changes to a better access point ([`better_access_point`]);
//! - when the task leaves and joins again after an MQTT session failed, and
//!   when it restarts the chip ([`LinkHealth`]).
//!
//! A join first goes to the access point of the last join, on its channel,
//! with no scan of all the channels. This join is fast. When the core says
//! that the link is quiet (`WifiQuiet`), the task scans all the channels once,
//! and changes to an access point that is clearly better. If the join to the
//! cached access point fails, the task scans all the channels and joins the
//! access point with the strongest signal.

/// Work for the Wi-Fi task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WifiRequest {
    /// Join the network, and answer with this token.
    On {
        /// The token of the request in the shell.
        token: u32,
    },
    /// Leave the network.
    Off,
    /// The link is quiet. Scan for a better access point, if a scan is due
    /// ([`ApCache::scan_due`]).
    Roam,
    /// The core closes the stream after a clean end of the session, for
    /// example for a sleep. Stop a scan for a better access point. A change
    /// of access point that started continues.
    Hold,
}

/// What a join in progress does with a new request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuringJoin {
    /// Continue the join, and answer with this token. The answer is also for
    /// the older requests.
    Continue {
        /// The token of the answer.
        token: u32,
    },
    /// Stop the join, and do not answer. The shell already answered the
    /// requests that wait.
    Cancel,
}

/// Returns what a join for `token` does when `request` arrives.
#[must_use]
pub fn during_join(token: u32, request: WifiRequest) -> DuringJoin {
    match request {
        WifiRequest::On { token: newer } => DuringJoin::Continue {
            token: token.max(newer),
        },
        WifiRequest::Off => DuringJoin::Cancel,
        WifiRequest::Roam | WifiRequest::Hold => DuringJoin::Continue { token },
    }
}

/// What a change of access point does with a new request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuringSwitch {
    /// Continue the change. When it ends, answer this token, if there is one.
    Continue {
        /// The token of the newest `WifiOn` request that waits.
        token: Option<u32>,
    },
    /// Stop the change, and do not answer.
    Cancel,
}

/// Returns what a change of access point does when `request` arrives.
/// `token` is the newest `WifiOn` request that waits for the change.
#[must_use]
pub fn during_switch(token: Option<u32>, request: WifiRequest) -> DuringSwitch {
    match request {
        WifiRequest::On { token: newer } => DuringSwitch::Continue {
            token: Some(token.map_or(newer, |token| token.max(newer))),
        },
        WifiRequest::Off => DuringSwitch::Cancel,
        WifiRequest::Roam | WifiRequest::Hold => DuringSwitch::Continue { token },
    }
}

/// The access point of a join: its BSSID and its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessPoint {
    /// The BSSID (the MAC address) of the access point.
    pub bssid: [u8; 6],
    /// The primary channel of the access point.
    pub channel: u8,
}

/// An access point that a scan found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The access point.
    pub access_point: AccessPoint,
    /// Its signal in dBm.
    pub rssi: i32,
}

/// How the task joins the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinPath {
    /// Join this access point on its channel, with no scan of all the
    /// channels.
    Cached(AccessPoint),
    /// Scan all the channels, and join the access point with the strongest
    /// signal.
    Scan,
}

/// What the task does after a join that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterFailure {
    /// The join with the cached access point failed. Join again with a scan
    /// of all the channels.
    Rescan,
    /// The join with a scan failed. Answer `Failed`.
    Fail,
}

/// The access point of the last good join, and if a scan for a better one is
/// due.
///
/// The task keeps the cache in RAM, which light sleep keeps. The rules:
///
/// - The next join goes to the cached access point on its channel, with no
///   scan of all the channels.
/// - If a join with the cached access point fails (also when DHCP gives no
///   address), the task forgets the access point and scans all the channels.
/// - After a join with the cached access point, one scan for a better access
///   point is due. The task does it when the link is quiet. A join with a
///   scan of all the channels already found the strongest access point, so
///   no scan is due after it, or after a change of access point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApCache {
    access_point: Option<AccessPoint>,
    scan_due: bool,
}

impl ApCache {
    /// Returns how the next join goes.
    #[must_use]
    pub fn plan(&self) -> JoinPath {
        self.access_point.map_or(JoinPath::Scan, JoinPath::Cached)
    }

    /// Returns the access point of the last good join.
    #[must_use]
    pub const fn current(&self) -> Option<AccessPoint> {
        self.access_point
    }

    /// Records a join on `path` that worked, to `access_point`.
    pub const fn joined(&mut self, path: JoinPath, access_point: AccessPoint) {
        self.access_point = Some(access_point);
        self.scan_due = matches!(path, JoinPath::Cached(_));
    }

    /// Records a join on `path` that failed, and returns what the task does
    /// next.
    pub const fn failed(&mut self, path: JoinPath) -> AfterFailure {
        self.access_point = None;
        self.scan_due = false;
        match path {
            JoinPath::Cached(_) => AfterFailure::Rescan,
            JoinPath::Scan => AfterFailure::Fail,
        }
    }

    /// Records the end of a change of access point. No scan is due, because
    /// the task just scanned.
    pub const fn switched(&mut self) {
        self.scan_due = false;
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
/// current one before the task changes to it.
///
/// The signal of each access point changes by some dB from one scan to the
/// next. Without a margin, the board can change between two access points
/// with almost the same signal. A change drops the MQTT connection for a
/// short time.
pub const ROAM_MARGIN_DB: i32 = 10;

/// Returns the access point to change to, or `None` to stay.
///
/// `current` is the access point of the link, and `rssi` its signal in dBm
/// from the driver. If the driver gives no signal, the function uses the
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
/// failed, before the task restarts the chip.
///
/// The Wi-Fi driver of `esp-radio` 1.0.0-beta.1 counts the frames that it did
/// not send yet. A frame that the driver drops while the link is down stays in
/// that count. When the count is full, the network stack can send and receive
/// nothing. A new join does not reset the count, and only a restart of the
/// chip does. The join itself works, but DHCP gets no address.
///
/// The value 3 is the join to the cached access point and the join after a
/// scan of the first `WifiOn`, and one more join after the backoff of the
/// core. Each join waits up to 20 s for DHCP, so the restart comes about one
/// minute after the session failed.
pub const RESTART_AFTER: u32 = 3;

/// How the task starts a join for `WifiOn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinStart {
    /// The driver is not connected. Join the network.
    Join,
    /// The driver is connected, and the link moved data after the last
    /// failure. Only wait for an address from DHCP.
    Keep,
    /// The driver is connected, but an MQTT session failed after the last
    /// proof of traffic. Leave the access point, wait for the network stack to
    /// drop the old address, and join again.
    Rejoin,
}

/// What the task does after a join that got no address from DHCP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAddress {
    /// Answer `Failed`. The core tries again after its backoff.
    Fail,
    /// The link is stuck. Restart the chip.
    Restart,
}

/// The health of the link, which the core task, the stream task and the
/// Wi-Fi task share.
///
/// The rules:
///
/// - An MQTT session that ends with an error makes the link suspect
///   ([`LinkHealth::session_failed`]). The core task records a `Close` with
///   `clean: false` from the core. The stream task records a socket error, a
///   reset from the broker, a write that stalls, and a stream that did not
///   open. A stream that a new `Open` replaced, a change of access point, a
///   FIN from the broker and a clean close are not errors.
/// - While the link is suspect, a `WifiOn` with the driver connected leaves
///   and joins again ([`JoinStart::Rejoin`]), so the task does not keep a
///   link that is stuck.
/// - While the link is suspect, each join that gets no address from DHCP
///   counts ([`LinkHealth::no_address`]). At [`RESTART_AFTER`], the task
///   restarts the chip.
/// - The first bytes from the broker on a stream prove that the link moves
///   data in both directions. They clear the suspicion and the count
///   ([`LinkHealth::traffic`]). A new address from DHCP also proves traffic,
///   so it clears the count ([`LinkHealth::address`]).
/// - If the network stack kept its old address through the join, the address
///   proves nothing. It counts as a join with no address
///   ([`LinkHealth::stale_address`]), but the task keeps the address.
///
/// A join that fails before DHCP (the access point is off, or it refuses
/// the device) does not count, because the cause is not on the device. A
/// restart clears the RAM, so after a restart the link is not suspect. Then
/// the next restart needs an address, a session that fails, and
/// [`RESTART_AFTER`] joins with no address. Thus the chip cannot restart
/// again and again while DHCP or the access point is down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkHealth {
    suspect: bool,
    no_address: u32,
}

impl LinkHealth {
    /// Returns a healthy link, for a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            suspect: false,
            no_address: 0,
        }
    }

    /// Records an MQTT session that ended with an error.
    pub const fn session_failed(&mut self) {
        self.suspect = true;
    }

    /// Records the first bytes from the broker on a stream.
    pub const fn traffic(&mut self) {
        self.suspect = false;
        self.no_address = 0;
    }

    /// Records a new address from DHCP after a join.
    pub const fn address(&mut self) {
        self.no_address = 0;
    }

    /// Returns `true` if an MQTT session failed after the last proof of
    /// traffic.
    #[must_use]
    pub const fn suspect(&self) -> bool {
        self.suspect
    }

    /// Returns how a join for `WifiOn` starts. `connected` is `true` if the
    /// driver says that the station is connected.
    #[must_use]
    pub const fn join_start(&self, connected: bool) -> JoinStart {
        match (connected, self.suspect) {
            (false, _) => JoinStart::Join,
            (true, false) => JoinStart::Keep,
            (true, true) => JoinStart::Rejoin,
        }
    }

    /// Records a join to an access point that got no address from DHCP, and
    /// returns what the task does next.
    pub const fn no_address(&mut self) -> NoAddress {
        if !self.suspect {
            return NoAddress::Fail;
        }
        self.no_address += 1;
        if self.no_address >= RESTART_AFTER {
            NoAddress::Restart
        } else {
            NoAddress::Fail
        }
    }

    /// Records a join after which the network stack has the address that it
    /// had before the join. The stack did not see the link go down, so it can
    /// be stuck. Counts as [`LinkHealth::no_address`].
    pub const fn stale_address(&mut self) -> NoAddress {
        self.no_address()
    }

    /// Returns the number of joins with no address that counted.
    #[must_use]
    pub const fn joins_with_no_address(&self) -> u32 {
        self.no_address
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccessPoint, AfterFailure, ApCache, DuringJoin, DuringSwitch, JoinPath, JoinStart,
        LinkHealth, NoAddress, RESTART_AFTER, ROAM_MARGIN_DB, Seen, WifiRequest,
        better_access_point, during_join, during_switch,
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
    fn wifi_off_cancels_a_join() {
        assert_eq!(during_join(3, WifiRequest::Off), DuringJoin::Cancel);
    }

    #[test]
    fn a_new_request_continues_the_join_with_its_token() {
        assert_eq!(
            during_join(3, WifiRequest::On { token: 4 }),
            DuringJoin::Continue { token: 4 }
        );
        assert_eq!(
            during_join(3, WifiRequest::On { token: 2 }),
            DuringJoin::Continue { token: 3 }
        );
    }

    #[test]
    fn a_roam_or_a_hold_does_not_change_a_join() {
        for request in [WifiRequest::Roam, WifiRequest::Hold] {
            assert_eq!(during_join(3, request), DuringJoin::Continue { token: 3 });
        }
    }

    #[test]
    fn a_change_of_access_point_answers_the_newest_wifi_on_after_it() {
        assert_eq!(
            during_switch(None, WifiRequest::On { token: 5 }),
            DuringSwitch::Continue { token: Some(5) }
        );
        assert_eq!(
            during_switch(Some(6), WifiRequest::On { token: 5 }),
            DuringSwitch::Continue { token: Some(6) }
        );
        // A hold for a sleep does not stop a change that started.
        assert_eq!(
            during_switch(Some(6), WifiRequest::Hold),
            DuringSwitch::Continue { token: Some(6) }
        );
        assert_eq!(
            during_switch(Some(6), WifiRequest::Off),
            DuringSwitch::Cancel
        );
    }

    #[test]
    fn the_first_join_scans_and_then_no_scan_is_due() {
        let mut cache = ApCache::default();
        assert_eq!(cache.plan(), JoinPath::Scan);
        cache.joined(JoinPath::Scan, NEAR);
        assert!(!cache.scan_due());
        assert_eq!(cache.plan(), JoinPath::Cached(NEAR));
    }

    #[test]
    fn a_cached_join_makes_one_scan_due() {
        let mut cache = ApCache::default();
        cache.joined(JoinPath::Scan, NEAR);
        cache.joined(JoinPath::Cached(NEAR), NEAR);
        assert!(cache.scan_due());
        cache.scanned();
        assert!(!cache.scan_due());
    }

    #[test]
    fn a_scan_that_did_not_complete_stays_due() {
        let mut cache = ApCache::default();
        cache.joined(JoinPath::Cached(NEAR), NEAR);
        // A request stops the scan, or the link is down, so the task does
        // not record the scan.
        assert!(cache.scan_due());
        assert!(cache.scan_due());
        // A `WifiOn` while the link is up does not join, so the scan stays
        // due for the next `Roam`.
        assert_eq!(cache.plan(), JoinPath::Cached(NEAR));
        assert!(cache.scan_due());
    }

    #[test]
    fn a_change_of_access_point_is_cached_with_no_scan_due() {
        let mut cache = ApCache::default();
        cache.joined(JoinPath::Cached(NEAR), NEAR);
        assert!(cache.scan_due());
        cache.joined(JoinPath::Cached(FAR), FAR);
        cache.switched();
        assert!(!cache.scan_due());
        assert_eq!(cache.plan(), JoinPath::Cached(FAR));
    }

    #[test]
    fn a_failed_cached_join_forgets_the_cache_and_scans() {
        let mut cache = ApCache::default();
        cache.joined(JoinPath::Scan, NEAR);
        assert_eq!(cache.failed(JoinPath::Cached(NEAR)), AfterFailure::Rescan);
        assert_eq!(cache.plan(), JoinPath::Scan);
        assert_eq!(cache.current(), None);
    }

    #[test]
    fn a_failed_scan_fails_the_join() {
        let mut cache = ApCache::default();
        assert_eq!(cache.failed(JoinPath::Scan), AfterFailure::Fail);
        assert_eq!(cache.plan(), JoinPath::Scan);
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
    fn with_no_signal_for_the_current_access_point_the_task_stays() {
        let found = [seen(FAR, -40)];
        assert_eq!(better_access_point(NEAR, None, &found), None);
    }

    #[test]
    fn an_empty_scan_stays() {
        assert_eq!(better_access_point(NEAR, Some(-90), &[]), None);
    }

    #[test]
    fn a_healthy_link_keeps_the_join() {
        let health = LinkHealth::default();
        assert_eq!(health.join_start(true), JoinStart::Keep);
        assert_eq!(health.join_start(false), JoinStart::Join);
    }

    #[test]
    fn after_a_failed_session_a_connected_driver_leaves_and_joins_again() {
        let mut health = LinkHealth::default();
        health.session_failed();
        assert_eq!(health.join_start(true), JoinStart::Rejoin);
        // After a leave, the driver is not connected.
        assert_eq!(health.join_start(false), JoinStart::Join);
        // The rejoin got an address, but the link is suspect until the first
        // bytes from the broker.
        health.address();
        assert_eq!(health.join_start(true), JoinStart::Rejoin);
        health.traffic();
        assert_eq!(health.join_start(true), JoinStart::Keep);
    }

    #[test]
    fn a_stuck_link_restarts_the_chip_after_the_limit() {
        // The session stops, the rejoin and the retries get no address.
        let mut health = LinkHealth::default();
        health.session_failed();
        for _ in 1..RESTART_AFTER {
            assert_eq!(health.no_address(), NoAddress::Fail);
        }
        assert_eq!(health.no_address(), NoAddress::Restart);
    }

    #[test]
    fn an_address_starts_the_count_again() {
        let mut health = LinkHealth::default();
        health.session_failed();
        for _ in 1..RESTART_AFTER {
            assert_eq!(health.no_address(), NoAddress::Fail);
        }
        health.address();
        assert_eq!(health.joins_with_no_address(), 0);
        // The next session fails too, so the link stays suspect.
        health.session_failed();
        assert_eq!(health.no_address(), NoAddress::Fail);
    }

    #[test]
    fn a_dhcp_outage_with_no_failed_session_never_restarts() {
        // After a restart, or while the broker is up and Wi-Fi is new, the
        // link is not suspect.
        let mut health = LinkHealth::default();
        for _ in 0..(RESTART_AFTER * 4) {
            assert_eq!(health.no_address(), NoAddress::Fail);
        }
        assert_eq!(health.joins_with_no_address(), 0);
    }

    #[test]
    fn a_connack_clears_the_suspicion_and_the_count() {
        let mut health = LinkHealth::default();
        health.session_failed();
        health.no_address();
        health.traffic();
        assert!(!health.suspect());
        assert_eq!(health.joins_with_no_address(), 0);
        for _ in 0..(RESTART_AFTER * 2) {
            assert_eq!(health.no_address(), NoAddress::Fail);
        }
    }

    #[test]
    fn an_old_address_after_a_rejoin_does_not_stop_the_restart() {
        // The stack is stuck, so it never drops the address of the old link.
        // Each rejoin "gets" that address, and each new session fails.
        let mut health = LinkHealth::default();
        for _ in 1..RESTART_AFTER {
            health.session_failed();
            assert_eq!(health.stale_address(), NoAddress::Fail);
        }
        health.session_failed();
        assert_eq!(health.stale_address(), NoAddress::Restart);
    }
}
