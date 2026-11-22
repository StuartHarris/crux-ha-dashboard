//! The fake radio.
//!
//! The fake radio answers each radio request at once, with the rules of
//! [`crate::capabilities::radio`]. Each method takes the operation of a
//! request and returns its answer. The public fields are the state of the
//! radio and of the network, and a test can change them between requests.

use alloc::{string::String, vec, vec::Vec};

use crate::{
    capabilities::radio::{
        AccessPoint, Address, Join, Joined, Lease, Leave, Off, Scan, Scanned, Seen, StopScan,
        Target,
    },
    features::wifi::rules::ROAM_MARGIN_DB,
};

/// The first access point of the default network.
pub const HALL: AccessPoint = AccessPoint {
    bssid: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
    channel: 6,
};

/// The second access point of the default network.
pub const LANDING: AccessPoint = AccessPoint {
    bssid: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
    channel: 11,
};

/// The signal of the strongest access point after
/// [`FakeRadio::move_strongest`], in dBm.
pub const STRONG_RSSI: i32 = -45;

/// The signal of each other access point after
/// [`FakeRadio::move_strongest`], in dBm.
pub const WEAK_RSSI: i32 = STRONG_RSSI - 3 * ROAM_MARGIN_DB;

/// The IP address that DHCP gives the device by default.
pub const IP: [u8; 4] = [192, 168, 1, 50];

/// The fake radio, and the access points of the network.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate state of the fake radio"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeRadio {
    /// The access points of the network, with their signals.
    pub access_points: Vec<Seen>,
    /// If `true`, the radio is on.
    pub on: bool,
    /// The access point with which the device is associated.
    pub associated: Option<AccessPoint>,
    /// If `true`, the network stack has an IP address.
    pub has_address: bool,
    /// The IP address that DHCP gives at the next lease. The default is
    /// [`IP`].
    pub ip: [u8; 4],
    /// If `true`, the access points refuse the device, as when the router
    /// blocks it. Each join fails with `"authentication failed"`.
    pub blocked: bool,
    /// If `true`, the driver is stuck. Each join works, but DHCP gives no
    /// address.
    pub stuck: bool,
    /// The number of joins that associated with an access point. A join
    /// that keeps an association does not count.
    pub joins: u32,
}

impl Default for FakeRadio {
    /// Returns a radio that is off, in a network with two access points.
    /// [`HALL`] is the stronger one.
    fn default() -> Self {
        Self::new(vec![
            Seen {
                access_point: HALL,
                rssi: -55,
            },
            Seen {
                access_point: LANDING,
                rssi: -70,
            },
        ])
    }
}

impl FakeRadio {
    /// Returns a radio that is off, in a network with `access_points`.
    #[must_use]
    pub const fn new(access_points: Vec<Seen>) -> Self {
        Self {
            access_points,
            on: false,
            associated: None,
            has_address: false,
            ip: IP,
            blocked: false,
            stuck: false,
            joins: 0,
        }
    }

    /// Answers a [`Join`] request.
    ///
    /// If the device is associated and `keep` is `true`, the radio keeps the
    /// association. If `keep` is `false`, the radio leaves first.
    pub fn join(&mut self, join: &Join) -> Joined {
        self.on = true;
        if let Some(access_point) = self.associated {
            if join.keep {
                return Joined::Associated {
                    access_point,
                    rssi: self.rssi(access_point),
                    kept: true,
                    had_address: self.has_address,
                };
            }
            self.leave(&Leave { radio: join.radio });
        }
        if self.blocked {
            return Joined::Failed(String::from("authentication failed"));
        }
        let found = match join.target {
            Target::Cached(cached) => self
                .access_points
                .iter()
                .find(|seen| seen.access_point.bssid == cached.bssid),
            Target::Strongest => self.access_points.iter().max_by_key(|seen| seen.rssi),
        };
        let Some(&Seen { access_point, rssi }) = found else {
            return Joined::Failed(String::from("no access point found"));
        };
        self.associated = Some(access_point);
        self.joins += 1;
        Joined::Associated {
            access_point,
            rssi: Some(rssi),
            kept: false,
            had_address: self.has_address,
        }
    }

    /// Answers an [`Address`] request. The device gets an address if it is
    /// associated and the driver is not stuck.
    pub const fn address(&mut self, _address: &Address) -> Lease {
        if self.associated.is_none() || self.stuck {
            return Lease::NoLease;
        }
        self.has_address = true;
        Lease::Leased { ip: self.ip }
    }

    /// Answers a [`Leave`] request. The device leaves the access point, and
    /// the address goes.
    pub const fn leave(&mut self, _leave: &Leave) {
        self.associated = None;
        self.has_address = false;
    }

    /// Answers a [`Scan`] request with all the access points, and the signal
    /// of the access point with which the device is associated.
    pub fn scan(&mut self, _scan: &Scan) -> Scanned {
        self.on = true;
        Scanned::Found {
            seen: self.access_points.clone(),
            rssi: self
                .associated
                .and_then(|access_point| self.rssi(access_point)),
        }
    }

    /// Answers a [`StopScan`] request. A scan of the fake radio is always
    /// complete, so there is nothing to stop.
    pub const fn stop_scan(&mut self, _stop_scan: &StopScan) {}

    /// Answers an [`Off`] request. The device leaves the access point, and
    /// the radio turns off.
    pub const fn off(&mut self, off: &Off) {
        self.leave(&Leave { radio: off.radio });
        self.on = false;
    }

    /// Drops the link, as when the access point goes off. The device is not
    /// associated, and the address goes.
    pub const fn drop_link(&mut self) {
        self.associated = None;
        self.has_address = false;
    }

    /// Makes another access point the strongest, so that the next scan
    /// finds a better access point. Returns the new strongest access point.
    ///
    /// The access point after the current one (in the order of
    /// `access_points`) gets [`STRONG_RSSI`], and each other one gets
    /// [`WEAK_RSSI`]. The current access point is the one with which the
    /// device is associated, or else the strongest one. If the network has
    /// no access points, the function returns `None`.
    pub fn move_strongest(&mut self) -> Option<AccessPoint> {
        if self.access_points.is_empty() {
            return None;
        }
        let current = self.associated.or_else(|| {
            self.access_points
                .iter()
                .max_by_key(|seen| seen.rssi)
                .map(|seen| seen.access_point)
        })?;
        let place = self
            .access_points
            .iter()
            .position(|seen| seen.access_point.bssid == current.bssid)
            .map_or(0, |place| (place + 1) % self.access_points.len());
        for (index, seen) in self.access_points.iter_mut().enumerate() {
            seen.rssi = if index == place {
                STRONG_RSSI
            } else {
                WEAK_RSSI
            };
        }
        Some(self.access_points[place].access_point)
    }

    /// Returns the signal of `access_point`, if the network has it.
    fn rssi(&self, access_point: AccessPoint) -> Option<i32> {
        self.access_points
            .iter()
            .find(|seen| seen.access_point.bssid == access_point.bssid)
            .map(|seen| seen.rssi)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::{FakeRadio, HALL, IP, LANDING, STRONG_RSSI, WEAK_RSSI};
    use crate::{
        capabilities::radio::{
            Address, Join, Joined, Lease, Leave, Off, Scan, Scanned, StopScan, Target,
        },
        features::wifi::rules::better_access_point,
    };

    const fn join(target: Target, keep: bool) -> Join {
        Join {
            radio: 1,
            target,
            keep,
        }
    }

    /// Returns a radio that joined the strongest access point and has an
    /// address.
    fn up() -> FakeRadio {
        let mut radio = FakeRadio::default();
        radio.join(&join(Target::Strongest, false));
        assert_eq!(
            radio.address(&Address { radio: 2 }),
            Lease::Leased { ip: IP }
        );
        radio
    }

    #[test]
    fn a_join_to_the_strongest_associates_with_the_strongest_access_point() {
        let mut radio = FakeRadio::default();
        assert_eq!(
            radio.join(&join(Target::Strongest, false)),
            Joined::Associated {
                access_point: HALL,
                rssi: Some(-55),
                kept: false,
                had_address: false,
            }
        );
        assert!(radio.on);
        assert_eq!(radio.associated, Some(HALL));
        assert_eq!(radio.joins, 1);
    }

    #[test]
    fn a_cached_join_associates_with_that_access_point() {
        let mut radio = FakeRadio::default();
        let Joined::Associated { access_point, .. } =
            radio.join(&join(Target::Cached(LANDING), false))
        else {
            panic!("an association");
        };
        assert_eq!(access_point, LANDING);
    }

    #[test]
    fn a_cached_join_to_an_access_point_that_is_gone_fails() {
        let mut radio = FakeRadio::default();
        radio
            .access_points
            .retain(|seen| seen.access_point != LANDING);
        assert!(matches!(
            radio.join(&join(Target::Cached(LANDING), false)),
            Joined::Failed(_)
        ));
        assert_eq!(radio.associated, None);
    }

    #[test]
    fn a_join_that_keeps_answers_with_the_association_it_has() {
        let mut radio = up();
        assert_eq!(
            radio.join(&join(Target::Cached(HALL), true)),
            Joined::Associated {
                access_point: HALL,
                rssi: Some(-55),
                kept: true,
                had_address: true,
            }
        );
        assert_eq!(radio.joins, 1);
        assert!(radio.has_address);
    }

    #[test]
    fn a_join_that_does_not_keep_leaves_first() {
        let mut radio = up();
        assert_eq!(
            radio.join(&join(Target::Cached(HALL), false)),
            Joined::Associated {
                access_point: HALL,
                rssi: Some(-55),
                kept: false,
                had_address: false,
            }
        );
        assert_eq!(radio.joins, 2);
    }

    #[test]
    fn an_address_that_the_stack_kept_shows_in_had_address() {
        // The stack did not see the link go down, so it kept its address.
        let mut radio = up();
        radio.associated = None;
        let Joined::Associated { had_address, .. } = radio.join(&join(Target::Strongest, false))
        else {
            panic!("an association");
        };
        assert!(had_address);
    }

    #[test]
    fn a_blocked_device_cannot_join() {
        let mut radio = FakeRadio {
            blocked: true,
            ..FakeRadio::default()
        };
        assert_eq!(
            radio.join(&join(Target::Strongest, false)),
            Joined::Failed("authentication failed".into())
        );
        assert_eq!(radio.associated, None);
        assert_eq!(radio.address(&Address { radio: 2 }), Lease::NoLease);
        radio.blocked = false;
        assert!(matches!(
            radio.join(&join(Target::Strongest, false)),
            Joined::Associated { .. }
        ));
    }

    #[test]
    fn a_stuck_driver_joins_but_gets_no_address() {
        let mut radio = FakeRadio {
            stuck: true,
            ..FakeRadio::default()
        };
        for _ in 0..3 {
            assert!(matches!(
                radio.join(&join(Target::Strongest, false)),
                Joined::Associated { .. }
            ));
            assert_eq!(radio.address(&Address { radio: 2 }), Lease::NoLease);
        }
        assert_eq!(radio.joins, 3);
        assert!(!radio.has_address);
    }

    #[test]
    fn leave_off_and_drop_link_remove_the_association_and_the_address() {
        let mut radio = up();
        radio.leave(&Leave { radio: 3 });
        assert_eq!((radio.associated, radio.has_address), (None, false));
        assert!(radio.on);

        let mut radio = up();
        radio.off(&Off { radio: 3 });
        assert_eq!((radio.associated, radio.has_address), (None, false));
        assert!(!radio.on);

        let mut radio = up();
        radio.drop_link();
        assert_eq!((radio.associated, radio.has_address), (None, false));
    }

    #[test]
    fn a_scan_finds_each_access_point_and_the_current_signal() {
        let mut radio = up();
        radio.stop_scan(&StopScan { radio: 3 });
        let Scanned::Found { seen, rssi } = radio.scan(&Scan { radio: 4 }) else {
            panic!("a complete scan");
        };
        assert_eq!(seen, radio.access_points);
        assert_eq!(rssi, Some(-55));

        let mut radio = FakeRadio::default();
        let Scanned::Found { rssi, .. } = radio.scan(&Scan { radio: 4 }) else {
            panic!("a complete scan");
        };
        assert_eq!(rssi, None);
    }

    #[test]
    fn move_strongest_makes_the_next_access_point_better_by_more_than_the_margin() {
        let mut radio = up();
        assert_eq!(radio.move_strongest(), Some(LANDING));
        let rssi: Vec<i32> = radio.access_points.iter().map(|seen| seen.rssi).collect();
        assert_eq!(rssi, [WEAK_RSSI, STRONG_RSSI]);
        let Scanned::Found { seen, rssi } = radio.scan(&Scan { radio: 4 }) else {
            panic!("a complete scan");
        };
        let best = better_access_point(HALL, rssi, &seen).expect("a better access point");
        assert_eq!(best.access_point, LANDING);

        // After the roam, the next move goes back to the first access point.
        radio.join(&join(Target::Cached(LANDING), false));
        assert_eq!(radio.move_strongest(), Some(HALL));
    }

    #[test]
    fn move_strongest_in_a_network_with_no_access_points_does_nothing() {
        let mut radio = FakeRadio::new(Vec::new());
        assert_eq!(radio.move_strongest(), None);
    }

    #[test]
    fn move_strongest_after_the_network_lost_all_its_access_points_does_nothing() {
        let mut radio = up();
        radio.access_points.clear();
        assert_eq!(radio.move_strongest(), None);
    }
}
