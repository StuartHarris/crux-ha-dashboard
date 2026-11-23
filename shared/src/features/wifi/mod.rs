//! The Wi-Fi feature: the Wi-Fi policy of the core.
//!
//! The core decides when to join, which access point to join, when to scan
//! for a better one and when to restart the chip. The shell does only the
//! radio work. Refer to [`crate::capabilities::radio`]. [`rules`] has the
//! pure rules, and this module has the state machine that uses them.
//!
//! The connection asks for the link with [`link`]. The feature then calls
//! back [`connection::link_up`] when the device has an address, or
//! [`connection::link_failed`] when the join failed. The steps:
//!
//! - From `Off` or `Idle`, the core joins the target of [`ApCache::plan`].
//! - From `Up` with a link that is ready ([`Linked::ready`]), the core
//!   connects with no join.
//! - From another linked phase, or from an association during a sleep
//!   ([`Phase::Associated`]), the core joins again and keeps the
//!   association. If the link is suspect, or the cache has another access
//!   point, the core leaves first. Refer to [`Health`].
//! - If a sleep started before the answer to a join or to the `Address`
//!   request, the answer gives [`Phase::Associated`]. It counts nothing,
//!   and the steps of the sleep turn the radio off.
//! - After the association, the core waits for an address. A lease that is
//!   not new can count toward a restart. A join with no lease leaves, and
//!   then joins the strongest access point or fails.
//! - When the link is quiet, the core scans once for a better access point
//!   if a scan is due. If it finds one, it closes the MQTT session with a
//!   DISCONNECT packet, and the reconnect goes to the better access point.
//!   During the close, the view shows that the core connects.
//!
//! The core sends one radio request at a time. Each request has a new id
//! and a time limit, which is an ordinary timer. The first of the answer
//! and the timer counts. The core then clears the timer and takes a new id,
//! so it ignores the other one. A time limit that expires acts as a failure
//! of the request, but it never counts toward a restart.
//!
//! # The Wi-Fi facts
//!
//! The link ([`Linked`]) holds the facts that the core publishes to Home
//! Assistant in the state message: the access point (its BSSID), the
//! address from the lease and the signal. Only a phase with a link has
//! them. An association during a sleep is [`Phase::Associated`], also when
//! the shell gave an address. It keeps no facts, and the snapshot of the
//! state reads it as no link. No state message goes during a sleep, and
//! the lease after a wake gives the facts again. The core updates the facts
//! at these times:
//!
//! - The join gives the access point and the first signal. The `Leasing`
//!   phase keeps them until the lease.
//! - The lease gives the address, and the link starts with all three facts.
//! - A scan (after a join with the cached access point, for example after
//!   each wake) that sees the current access point gives a new signal. The
//!   core then publishes the state, at most once in the throttle window
//!   ([`device`]).
//!
//! The device on the stand scans only after a join with the cached access
//! point. A stable link on the stand thus keeps the signal of its last join
//! or scan, for hours or days.
//!
//! Each decision writes a line through the log capability
//! ([`crate::capabilities::log`]), with the area `wifi`. The core writes only
//! the decisions that are not usual. The line comes
//! before the effects of the decision. The usual steps (a join, a new
//! address, a rejoin that keeps the association) write no line.

pub mod rules;

use alloc::{format, string::String};
use core::time::Duration;

use crate::{
    Cmd, Model,
    capabilities::{
        log::Area,
        radio::{AccessPoint, Joined, Lease, Radio, Scanned, Target, signal},
    },
    features::{connection, device, logs, power, timer::Slot},
    mqtt::End,
};

use rules::{
    AfterFailure, ApCache, Health, RESTART_AFTER, ROAM_MARGIN_DB, Recovery, better_access_point,
    current_signal,
};

/// The internal events of the Wi-Fi feature. The core makes them from the
/// answers to its radio requests and from its time limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The answer to the `Join` request with this id.
    Joined {
        /// The id of the radio request. The core ignores the answer to an
        /// older request.
        radio: u32,
        /// The answer.
        joined: Joined,
    },
    /// The answer to the `Address` request with this id.
    Leased {
        /// The id of the radio request. The core ignores the answer to an
        /// older request.
        radio: u32,
        /// The answer.
        lease: Lease,
    },
    /// The answer to the `Leave` request with this id.
    Left(u32),
    /// The answer to the `Scan` request with this id.
    Scanned {
        /// The id of the radio request. The core ignores the answer to an
        /// older request.
        radio: u32,
        /// The answer.
        scanned: Scanned,
    },
    /// The answer to the `StopScan` request with this id.
    ScanStopped(u32),
    /// The time limit of the radio request with this id expired. Refer to
    /// [`crate::features::wifi`].
    RadioTimeout(u32),
    /// The answer to the `Off` request with this id.
    RadioOff(u32),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Wifi(event)
    }
}

/// Handles an internal event of the Wi-Fi feature.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::Joined { radio, joined } => self::joined(radio, &joined, model),
        Event::Leased { radio, lease } => leased(radio, lease, model),
        Event::Left(radio) => left(radio, model),
        Event::Scanned { radio, scanned } => self::scanned(radio, &scanned, model),
        Event::ScanStopped(radio) => scan_stopped(radio, model),
        Event::RadioTimeout(radio) => timeout(radio, model),
        Event::RadioOff(radio) => radio_off(radio, model),
    }
}

/// The time limit of the core for a `Join` request. The shell gives up
/// after 20 seconds at most, with the leave before the join and the
/// disconnect after a failure.
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
/// 1.1 seconds at most: a disconnect of 1 second, and 100 ms for the last
/// frames.
pub const OFF_TIMEOUT: Duration = Duration::from_secs(3);

/// The reason of a failure when a time limit of the core expires.
const NO_ANSWER: &str = "no answer in the time limit";

/// What the radio does now, as the core sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// The radio is off.
    #[default]
    Off,
    /// The radio is on, but the device has no link.
    Idle,
    /// A `Join` request to this target waits for its answer.
    Joining(Target),
    /// An `Address` request waits for its answer, after a join.
    Leasing {
        /// The target of the join.
        target: Target,
        /// The access point of the association.
        access_point: AccessPoint,
        /// The signal after the join, in dBm, if the shell can read it.
        rssi: Option<i32>,
        /// The device had an address before the join.
        had_address: bool,
        /// The join kept an association that the device had.
        kept: bool,
    },
    /// A `Leave` request waits for its answer. Then the core does this.
    Leaving(Then),
    /// The device associated with this access point after a sleep started.
    /// The core counts nothing, keeps nothing in the cache, and keeps no
    /// address or signal from the answer, so this phase has no link
    /// ([`Phase::linked`]). The steps of the sleep turn the radio off. If
    /// the device wakes before that, [`link`] joins again from this access
    /// point. For an MQTT session that ends, this phase counts as a link.
    /// Refer to [`session_ended`].
    Associated(AccessPoint),
    /// The device has an address.
    Up(Linked),
    /// A `Scan` request waits for its answer. The link stays up.
    Scanning(Linked),
    /// A `StopScan` request for a sleep waits for its answer. The answer
    /// starts the next step of the sleep. If the device wakes and a new
    /// sleep starts before the answer, the new sleep takes the request
    /// ([`stop_scan`]).
    StoppingScan {
        /// The link, which stays up.
        linked: Linked,
        /// The id of the sleep.
        sleep: u32,
    },
    /// An `Off` request for a sleep waits for its answer. The answer starts
    /// the next step of the sleep.
    TurningOff {
        /// The id of the sleep.
        sleep: u32,
        /// The core wants the link after the answer, because the device
        /// woke while the radio turned off.
        wanted: bool,
    },
    /// The core asked the shell to restart the chip. The core does no more
    /// radio work.
    Restarting,
}

impl Phase {
    /// Returns the link of the device with an address, or `None` if the
    /// phase has no link.
    ///
    /// A scan keeps the link to the access point, so `Scanning` and
    /// `StoppingScan` have a link too.
    #[must_use]
    pub const fn linked(mut self) -> Option<Linked> {
        self.linked_mut().copied()
    }

    /// Returns the link of the device with an address, for a change, or
    /// `None` if the phase has no link. This function is the only list of
    /// the phases with a link. Refer to [`Phase::linked`].
    pub const fn linked_mut(&mut self) -> Option<&mut Linked> {
        match self {
            Self::Up(linked) | Self::Scanning(linked) | Self::StoppingScan { linked, .. } => {
                Some(linked)
            }
            _ => None,
        }
    }
}

/// The link of the device with an address.
///
/// The link holds the Wi-Fi facts that the core publishes to Home Assistant
/// (refer to [`device`]): the access point, the address and the signal.
/// Only a phase with a link has them, so the core cannot publish an address
/// without a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Linked {
    /// The access point of the link.
    pub access_point: AccessPoint,
    /// The IPv4 address from the lease.
    pub ip: [u8; 4],
    /// The last signal of the access point, in dBm, if the shell could
    /// read it. The join gives the first value. Each scan that sees the
    /// access point gives a new value.
    pub rssi: Option<i32>,
    /// The core can connect on this link with no new join. It is `false`
    /// after an MQTT session that did not end cleanly, and after a decision
    /// to change access point.
    pub ready: bool,
}

/// What the core does after a `Leave` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Join this target.
    Join(Target),
    /// The join with the cached access point got no address. Join the
    /// strongest access point, with no backoff.
    Rescan,
    /// The join with the strongest access point, this access point, got no
    /// address. The link failed, so the connection waits for its backoff.
    Fail(AccessPoint),
}

/// The state of the Wi-Fi feature.
#[derive(Debug, Default)]
pub struct State {
    /// What the radio does now.
    pub phase: Phase,
    /// The id of the newest radio request. The core ignores an answer or a
    /// time limit with another id.
    pub radio: u32,
    /// The time limit of the radio request that waits.
    limit: Slot,
    /// The access point of the last good join. Light sleep keeps it.
    pub cache: ApCache,
    /// The health of the link. A start makes a new one.
    pub health: Health,
}

/// Forgets the radio after the shell starts, as after a restart of the
/// chip. The core keeps the cache and makes a new [`Health`]. It ignores
/// the answers to earlier requests.
pub fn reset(model: &mut Model) {
    let state = &mut model.wifi;
    state.phase = Phase::Off;
    state.radio = state.radio.wrapping_add(1);
    state.limit.stop();
    state.health = Health::default();
}

/// Gets a link for the connection. Refer to the module documentation.
///
/// While a request waits, the core does nothing, because its answer
/// continues the steps. While the radio turns off for a sleep, the core
/// joins after the answer.
pub fn link(model: &mut Model) -> Cmd {
    let state = &mut model.wifi;
    match state.phase {
        Phase::Up(Linked { ready: true, .. }) => connection::link_up(model),
        // A new request stops the scan.
        Phase::Up(linked) | Phase::Scanning(linked) | Phase::StoppingScan { linked, .. } => {
            rejoin(linked.access_point, model)
        }
        Phase::Associated(access_point) => rejoin(access_point, model),
        Phase::Off | Phase::Idle => {
            let target = state.cache.plan();
            join(target, false, model)
        }
        Phase::TurningOff { .. } => {
            if let Phase::TurningOff { wanted, .. } = &mut state.phase {
                *wanted = true;
            }
            Cmd::done()
        }
        Phase::Joining(_) | Phase::Leasing { .. } | Phase::Leaving(_) | Phase::Restarting => {
            Cmd::done()
        }
    }
}

/// Joins again while the device has a link to `current`.
///
/// The join keeps the association, unless the link is suspect or the cache
/// has another access point. In those cases the core leaves first. A leave
/// for a suspect link writes a warning, or a `Debug` line in a report
/// ([`in_report`]).
fn rejoin(current: AccessPoint, model: &mut Model) -> Cmd {
    let state = &model.wifi;
    let target = state.cache.plan();
    if state.health.suspect() {
        let reason = retry_line(model)(
            Area::Wifi,
            format!("the link is suspect, so leave {current} and join again"),
        );
        reason.and(leave(Then::Join(target), model))
    } else if target != Target::Cached(current) {
        leave(Then::Join(target), model)
    } else {
        join(target, true, model)
    }
}

/// Records the end of an MQTT session with `end`.
///
/// A failure of the link makes it suspect. An end by the broker host proves
/// traffic, but only while the device has a link ([`Phase::linked`]) or an
/// association during a sleep ([`Phase::Associated`]). Refer to
/// [`Health::ended`].
///
/// No session can end in `Associated`, because a session starts only on a
/// ready link. If one did, an end by the broker host needs an address on
/// the device, so the end is a proof.
///
/// After an end that is not clean, the link can be the cause, so the next
/// [`link`] joins again. A clean end comes from the core, so the link stays
/// ready.
///
/// The command writes a line to the log only when the link becomes suspect
/// or stops being suspect. In a report, the line is a `Debug` line
/// ([`in_report`]).
pub fn session_ended(end: &End, model: &mut Model) -> Cmd {
    let has_link =
        model.wifi.phase.linked().is_some() || matches!(model.wifi.phase, Phase::Associated(_));
    let health = &mut model.wifi.health;
    let was_suspect = health.suspect();
    health.ended(end, has_link);
    let is_suspect = health.suspect();
    if *end != End::Clean
        && let Some(link) = model.wifi.phase.linked_mut()
    {
        link.ready = false;
    }
    match (was_suspect, is_suspect) {
        (false, true) => in_report(model, logs::warn)(
            Area::Wifi,
            format!("the link is suspect, because the MQTT session failed: {end}"),
        ),
        (true, false) => in_report(model, logs::info)(
            Area::Wifi,
            "the broker host ended the session, so the link moves data and is not suspect".into(),
        ),
        _ => Cmd::done(),
    }
}

/// Records bytes from the broker, which prove that the link moves data. An
/// end of the stream by the broker host is the same proof. Refer to
/// [`session_ended`].
///
/// The core calls this function for each chunk of bytes. The command writes
/// a line to the log only if the link was suspect. In a report, the line is
/// a `Debug` line ([`in_report`]).
pub fn traffic(model: &mut Model) -> Cmd {
    let health = &mut model.wifi.health;
    let was_suspect = health.suspect();
    health.traffic();
    if was_suspect {
        in_report(model, logs::info)(
            Area::Wifi,
            "bytes from the broker arrived, so the link is not suspect".into(),
        )
    } else {
        Cmd::done()
    }
}

/// The link is quiet, because the subscribe is complete. If a scan for a
/// better access point is due, the core scans now. In a report, the core
/// does not scan, and the scan stays due for the next wake.
pub fn quiet(model: &mut Model) -> Cmd {
    let state = &mut model.wifi;
    let Phase::Up(linked) = state.phase else {
        return Cmd::done();
    };
    if !state.cache.scan_due() || !model.may_scan() {
        return Cmd::done();
    }
    model.wifi.phase = Phase::Scanning(linked);
    let radio = next_id(model);
    Radio::scan(radio)
        .then_send(move |scanned| Event::Scanned { radio, scanned }.into())
        .and(limit(radio, SCAN_TIMEOUT, model))
}

/// Stops a scan before the sleep `sleep` disconnects. Returns `None` if no
/// scan runs and no `StopScan` request waits. The answer starts the next
/// step of the sleep.
///
/// If the `StopScan` request of an older sleep still waits, the new sleep
/// takes it over. The core sends no other request, and the time limit of
/// the old request stays. The answer then starts the next step of the new
/// sleep.
pub fn stop_scan(sleep: u32, model: &mut Model) -> Option<Cmd> {
    let linked = match &mut model.wifi.phase {
        Phase::Scanning(linked) => *linked,
        Phase::StoppingScan { sleep: waiter, .. } => {
            *waiter = sleep;
            return Some(Cmd::done());
        }
        _ => return None,
    };
    model.wifi.phase = Phase::StoppingScan { linked, sleep };
    let radio = next_id(model);
    let stop = Radio::stop_scan(radio).then_send(move |()| Event::ScanStopped(radio).into());
    Some(stop.and(limit(radio, STOP_SCAN_TIMEOUT, model)))
}

/// Turns the radio off for the sleep `sleep`. The answer starts the next
/// step of the sleep.
///
/// `Off` stops a join that waits, also the join of a change of access
/// point, so the next join is not a change of access point
/// ([`ApCache::stopped`]). After the core asked for a restart, it does
/// nothing: the restart wins over the sleep.
pub fn off(sleep: u32, model: &mut Model) -> Cmd {
    let state = &mut model.wifi;
    if state.phase == Phase::Restarting {
        return Cmd::done();
    }
    state.cache.stopped();
    state.phase = Phase::TurningOff {
        sleep,
        wanted: false,
    };
    let radio = next_id(model);
    Radio::off(radio)
        .then_send(move |()| Event::RadioOff(radio).into())
        .and(limit(radio, OFF_TIMEOUT, model))
}

/// Handles the answer to the `Join` request `radio`.
///
/// `Stopped` is the answer to a join that a newer request stopped, so the
/// core ignores it. Only `Off` stops a join that has not reached its time
/// limit, and [`off`] records the stop in the cache.
pub fn joined(radio: u32, joined: &Joined, model: &mut Model) -> Cmd {
    if *joined == Joined::Stopped || !accept(radio, model) {
        return Cmd::done();
    }
    after_join(joined, model)
}

/// Handles the answer to the `Address` request `radio`. The core ignores
/// `Stopped`.
///
/// In a report, a join with no address or with an old address does not
/// count toward [`RESTART_AFTER`], and the core writes no line about the
/// count. The recovery is the same: leave, and join the strongest access
/// point. Refer to [`power::report_failed`].
///
/// If the core no longer wants a connection (a sleep started), the answer
/// changes no count and cannot restart the chip. The steps of the sleep
/// continue, and the device keeps the association until `Off`.
pub fn leased(radio: u32, lease: Lease, model: &mut Model) -> Cmd {
    if lease == Lease::Stopped || !accept(radio, model) {
        return Cmd::done();
    }
    let Phase::Leasing {
        target,
        access_point,
        rssi,
        had_address,
        kept,
    } = model.wifi.phase
    else {
        return Cmd::done();
    };
    if !model.wants_online() {
        return associated_during_sleep(access_point, model);
    }
    // The joins in a report do not count toward a restart. A restart
    // cannot fix an access point that is down, and it wakes the device.
    let counts = !model.power.is_reporting();
    let ip = match lease {
        Lease::Leased { ip } => ip,
        Lease::NoLease | Lease::Stopped if !counts => {
            return no_address(target, access_point, model);
        }
        Lease::NoLease | Lease::Stopped => {
            let state = &mut model.wifi;
            let recovery = state.health.no_lease();
            let count = counted(state.health);
            if recovery == Recovery::Restart {
                let reason = format!("no address after {RESTART_AFTER} joins");
                return count.and(restart_chip(Area::Wifi, reason, model));
            }
            return count.and(no_address(target, access_point, model));
        }
    };
    let state = &mut model.wifi;
    let mut lines = Cmd::done();
    // The stack kept its address through the join, so the address proves
    // nothing. The line comes only when the address counts: on a new join
    // while the link is suspect. A join that keeps the association keeps
    // the address too. In a report, the address does not count, and the
    // count stays as it is.
    if had_address && counts {
        if !kept && state.health.suspect() {
            lines = logs::warn(
                Area::Wifi,
                "the address is from before the join, so the network stack can be stuck",
            );
        }
        let recovery = state.health.stale();
        lines = lines.and(counted(state.health));
        if recovery == Recovery::Restart {
            let reason = format!("a stale address after {RESTART_AFTER} joins");
            return lines.and(restart_chip(Area::Wifi, reason, model));
        }
    } else if !had_address {
        state.health.leased();
    }
    if !kept {
        state.cache.joined(target, access_point);
    }
    state.phase = Phase::Up(Linked {
        access_point,
        ip,
        rssi,
        ready: true,
    });
    lines.and(connection::link_up(model))
}

/// Writes the count of joins with no new address to the log, after a join
/// that counted. A join counts only while the link is suspect. Refer to
/// [`Health::no_lease`].
fn counted(health: Health) -> Cmd {
    if !health.suspect() {
        return Cmd::done();
    }
    let count = health.joins_with_no_lease();
    logs::warn(
        Area::Wifi,
        format!("{count} of {RESTART_AFTER} joins got no new address while the link is suspect"),
    )
}

/// Handles the answer to the `Leave` request `radio`.
pub fn left(radio: u32, model: &mut Model) -> Cmd {
    if !accept(radio, model) {
        return Cmd::done();
    }
    after_leave(model)
}

/// Handles the answer to the `Scan` request `radio`. The core ignores
/// `Stopped`, so the scan stays due.
pub fn scanned(radio: u32, scanned: &Scanned, model: &mut Model) -> Cmd {
    if *scanned == Scanned::Stopped || !accept(radio, model) {
        return Cmd::done();
    }
    after_scan(scanned, model)
}

/// Handles the answer to the `StopScan` request `radio`.
pub fn scan_stopped(radio: u32, model: &mut Model) -> Cmd {
    if !accept(radio, model) {
        return Cmd::done();
    }
    after_stop_scan(model)
}

/// Handles the answer to the `Off` request `radio`.
pub fn radio_off(radio: u32, model: &mut Model) -> Cmd {
    if !accept(radio, model) {
        return Cmd::done();
    }
    after_off(model)
}

/// Handles the end of the time limit of the request `radio`.
///
/// If the request did not get its answer, the core acts as if the request
/// failed, and writes a warning. In a report, the line is a `Debug` line
/// ([`in_report`]). This never counts toward a restart. If the core no longer wants
/// a connection, a join or an address that did not end changes nothing in
/// the cache. The steps of the sleep turn the radio off.
pub fn timeout(radio: u32, model: &mut Model) -> Cmd {
    if !accept(radio, model) {
        return Cmd::done();
    }
    let write = in_report(model, logs::warn);
    let reason = request_name(model.wifi.phase).map_or_else(Cmd::done, |request| {
        write(
            Area::Wifi,
            format!("the {request} request got no answer in its time limit"),
        )
    });
    if !model.wants_online()
        && matches!(model.wifi.phase, Phase::Joining(_) | Phase::Leasing { .. })
    {
        model.wifi.phase = Phase::Idle;
        return reason;
    }
    let next = match model.wifi.phase {
        Phase::Joining(_) => after_join(&Joined::Failed(NO_ANSWER.into()), model),
        Phase::Leasing {
            target,
            access_point,
            ..
        } => no_address(target, access_point, model),
        Phase::Leaving(_) => after_leave(model),
        Phase::Scanning(_) => after_scan(&Scanned::Failed(NO_ANSWER.into()), model),
        Phase::StoppingScan { .. } => after_stop_scan(model),
        Phase::TurningOff { .. } => after_off(model),
        Phase::Off | Phase::Idle | Phase::Associated(_) | Phase::Up(_) | Phase::Restarting => {
            Cmd::done()
        }
    };
    reason.and(next)
}

/// Returns the name of the radio request that waits in `phase`, for the
/// log, or `None` if no request waits.
const fn request_name(phase: Phase) -> Option<&'static str> {
    match phase {
        Phase::Joining(_) => Some("join"),
        Phase::Leasing { .. } => Some("address"),
        Phase::Leaving(_) => Some("leave"),
        Phase::Scanning(_) => Some("scan"),
        Phase::StoppingScan { .. } => Some("stop scan"),
        Phase::TurningOff { .. } => Some("off"),
        Phase::Off | Phase::Idle | Phase::Associated(_) | Phase::Up(_) | Phase::Restarting => None,
    }
}

/// Continues after the answer to a join.
fn after_join(joined: &Joined, model: &mut Model) -> Cmd {
    let Phase::Joining(target) = model.wifi.phase else {
        return Cmd::done();
    };
    let &Joined::Associated {
        access_point,
        rssi,
        kept,
        had_address,
    } = joined
    else {
        // A join that fails never counts toward a restart, because the
        // cause is not on the device.
        model.wifi.phase = Phase::Idle;
        let why = match joined {
            Joined::Failed(why) => why.as_str(),
            Joined::Associated { .. } | Joined::Stopped => "stopped",
        };
        return match (model.wifi.cache.failed(target), target) {
            (AfterFailure::Rescan, Target::Cached(cached)) if model.wants_online() => {
                let reason = retry_line(model)(
                    Area::Wifi,
                    format!(
                        "the join to the cached access point {cached} failed, so join the strongest access point"
                    ),
                );
                reason.and(join(Target::Strongest, false, model))
            }
            (AfterFailure::Fail, _) if model.wants_online() => retry_line(model)(
                Area::Wifi,
                "the join to the strongest access point failed, so the link is down".into(),
            )
            .and(connection::link_failed(
                &format!("the join to the strongest access point failed: {why}"),
                model,
            )),
            (AfterFailure::Rescan | AfterFailure::Fail, _) => {
                connection::link_failed(&format!("the join failed: {why}"), model)
            }
        };
    };
    if !model.wants_online() {
        return associated_during_sleep(access_point, model);
    }
    model.wifi.phase = Phase::Leasing {
        target,
        access_point,
        rssi,
        had_address,
        kept,
    };
    let radio = next_id(model);
    Radio::address(radio)
        .then_send(move |lease| Event::Leased { radio, lease }.into())
        .and(limit(radio, ADDRESS_TIMEOUT, model))
}

/// Records an association with `access_point` after a sleep started: the
/// answer to a join, or an answer to the `Address` request with an address
/// or with none. The core counts nothing, keeps nothing in the cache, and
/// keeps no address or signal from the answer. Nothing reads them before
/// the wake or the `Off`, and the lease after a wake gives them again. The
/// steps of the sleep turn the radio off. If the device wakes before that,
/// [`link`] joins again from [`Phase::Associated`].
fn associated_during_sleep(access_point: AccessPoint, model: &mut Model) -> Cmd {
    model.wifi.phase = Phase::Associated(access_point);
    Cmd::done()
}

/// The join to `target` got no address from `access_point`. The core
/// forgets the access point and leaves. Then it joins the strongest access
/// point, or the link fails.
fn no_address(target: Target, access_point: AccessPoint, model: &mut Model) -> Cmd {
    let (then, message) = match model.wifi.cache.failed(target) {
        AfterFailure::Rescan => (
            Then::Rescan,
            format!(
                "{access_point} gave no address, so leave it and join the strongest access point"
            ),
        ),
        AfterFailure::Fail => (
            Then::Fail(access_point),
            format!("the strongest access point {access_point} gave no address, so leave it"),
        ),
    };
    retry_line(model)(Area::Wifi, message).and(leave(then, model))
}

/// Returns the function that writes a line about a failed try to get the
/// link. In a report, the line is a `Debug` line, so a report that fails
/// each hour does not fill the ring. Refer to
/// [`power::report_failed`](crate::features::power::report_failed).
/// Otherwise the line is a warning.
fn retry_line(model: &Model) -> fn(Area, String) -> Cmd {
    in_report(model, logs::warn)
}

/// Returns `logs::debug` in a report, and `usual` otherwise. A line about a
/// try to connect in a report is a `Debug` line, so the core does not keep
/// it in the ring. The line at the limit of the report tells the result.
fn in_report(model: &Model, usual: fn(Area, String) -> Cmd) -> fn(Area, String) -> Cmd {
    if model.power.is_reporting() {
        logs::debug
    } else {
        usual
    }
}

/// Continues after the answer to a leave.
fn after_leave(model: &mut Model) -> Cmd {
    let Phase::Leaving(then) = model.wifi.phase else {
        return Cmd::done();
    };
    model.wifi.phase = Phase::Idle;
    if !model.wants_online() {
        return Cmd::done();
    }
    match then {
        Then::Join(target) => join(target, false, model),
        Then::Rescan => join(Target::Strongest, false, model),
        Then::Fail(access_point) => connection::link_failed(
            &format!("the strongest access point {access_point} gave no address"),
            model,
        ),
    }
}

/// Continues after the answer to a scan. If the scan found a better access
/// point, the core closes the MQTT session, and the reconnect goes to that
/// access point.
fn after_scan(scanned: &Scanned, model: &mut Model) -> Cmd {
    let state = &mut model.wifi;
    let Phase::Scanning(linked) = state.phase else {
        return Cmd::done();
    };
    let current = linked.access_point;
    state.cache.scanned();
    state.phase = Phase::Up(linked);
    let Scanned::Found { seen, rssi } = scanned else {
        return logs::warn(Area::Wifi, format!("the scan failed, so stay on {current}"));
    };
    let Some(rssi_now) = current_signal(current, *rssi, seen) else {
        return logs::debug(
            Area::Wifi,
            format!("the signal of {current} is unknown, so stay"),
        );
    };
    // The scan saw the current access point, so its signal is new.
    let linked = Linked {
        rssi: Some(rssi_now),
        ..linked
    };
    state.phase = Phase::Up(linked);
    let now = signal(Some(rssi_now));
    let Some(best) = better_access_point(current, *rssi, seen) else {
        // The state message publishes the new signal, at most once in the
        // throttle window. Refer to [`device`].
        return logs::debug(
            Area::Wifi,
            format!(
                "the scan found no access point that is {ROAM_MARGIN_DB} dB stronger than {current} ({now}), so stay"
            ),
        )
        .and(device::changed(model));
    };
    state.cache.roam_to(best.access_point);
    state.phase = Phase::Up(Linked {
        ready: false,
        ..linked
    });
    let to = best.access_point;
    // During the steps of a sleep, the sleep closes the session. The next
    // wake joins the better access point. In both cases the session ends
    // soon, so the core does not publish the new signal now. The next
    // session publishes the state.
    if model.wants_online() {
        let reason = logs::info(
            Area::Wifi,
            format!(
                "change to {to} ({} dBm), because the signal of {current} is {now}",
                best.rssi
            ),
        );
        reason.and(connection::close_for_roam(model))
    } else {
        logs::info(
            Area::Wifi,
            format!("change to {to} at the next wake, because the signal of {current} is {now}"),
        )
    }
}

/// Continues after the answer to a stop of a scan, with the next step of
/// the sleep.
fn after_stop_scan(model: &mut Model) -> Cmd {
    let Phase::StoppingScan { linked, sleep } = model.wifi.phase else {
        return Cmd::done();
    };
    model.wifi.phase = Phase::Up(linked);
    power::scan_stopped(sleep, model)
}

/// Continues after the answer to `Off`, with the next step of the sleep.
/// If the device woke while the radio turned off, the core joins now.
fn after_off(model: &mut Model) -> Cmd {
    let state = &mut model.wifi;
    let Phase::TurningOff { sleep, wanted } = state.phase else {
        return Cmd::done();
    };
    state.phase = Phase::Off;
    let next = power::radio_off(sleep, model);
    if wanted {
        next.and(connection::go_online(model))
    } else {
        next
    }
}

/// Sends a `Join` request to `target`.
fn join(target: Target, keep: bool, model: &mut Model) -> Cmd {
    model.wifi.phase = Phase::Joining(target);
    let radio = next_id(model);
    Radio::join(radio, target, keep)
        .then_send(move |joined| Event::Joined { radio, joined }.into())
        .and(limit(radio, JOIN_TIMEOUT, model))
}

/// Sends a `Leave` request. After the answer, the core does `then`.
fn leave(then: Then, model: &mut Model) -> Cmd {
    model.wifi.phase = Phase::Leaving(then);
    let radio = next_id(model);
    Radio::leave(radio)
        .then_send(move |()| Event::Left(radio).into())
        .and(limit(radio, LEAVE_TIMEOUT, model))
}

/// Asks the shell to restart the chip, for `reason`. `area` is the part of
/// the core that asks, for the log of the shell.
///
/// The phase becomes `Restarting`, so the core does no more radio work. It
/// ignores each late answer and time limit of a radio request. The
/// connection calls this function after a clean close for a restart that
/// Home Assistant asked for. Refer to
/// [`connection::close_for_restart`].
pub(crate) fn restart_chip(area: Area, reason: String, model: &mut Model) -> Cmd {
    model.wifi.phase = Phase::Restarting;
    Radio::restart(area, reason).build()
}

/// Takes a new id for a radio request, and clears the time limit of the
/// request before it.
fn next_id(model: &mut Model) -> u32 {
    model.wifi.limit.stop();
    model.wifi.radio = model.wifi.radio.wrapping_add(1);
    model.wifi.radio
}

/// Returns `true` if `radio` is the id of the newest request. The core then
/// clears the time limit and takes a new id, so it ignores the other one of
/// the answer and the time limit.
fn accept(radio: u32, model: &mut Model) -> bool {
    if radio != model.wifi.radio {
        return false;
    }
    model.wifi.limit.stop();
    model.wifi.radio = model.wifi.radio.wrapping_add(1);
    true
}

/// Starts the time limit `after` of the request `radio`.
fn limit(radio: u32, after: Duration, model: &mut Model) -> Cmd {
    model
        .wifi
        .limit
        .start(radio, after, Event::RadioTimeout(radio).into())
}

#[cfg(test)]
mod tests;
