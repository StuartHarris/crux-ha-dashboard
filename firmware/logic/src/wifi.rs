//! The rules of the Wi-Fi task:
//!
//! - what a join does with a request that arrives during it
//!   ([`during_join`]), and what a change of access point does with one
//!   ([`during_switch`]);
//! - when a join uses the access point of the last join, and when the task
//!   scans for a better one ([`ApCache`]);
//! - when the task changes to a better access point ([`better_access_point`]).
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
    /// The core started to close the connection, for example for a sleep.
    /// Stop a scan for a better access point. A change of access point that
    /// started continues.
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

#[cfg(test)]
mod tests {
    use super::{
        AccessPoint, AfterFailure, ApCache, DuringJoin, DuringSwitch, JoinPath, ROAM_MARGIN_DB,
        Seen, WifiRequest, better_access_point, during_join, during_switch,
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
}
