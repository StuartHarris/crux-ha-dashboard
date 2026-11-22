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
//! - From another linked phase, the core joins again and keeps the
//!   association. If the link is suspect, or the cache has another access
//!   point, the core leaves first. Refer to [`Health`].
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
    features::{connection, logs, power, timer::Slot},
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
        /// The device had an address before the join.
        had_address: bool,
        /// The join kept an association that the device had.
        kept: bool,
    },
    /// A `Leave` request waits for its answer. Then the core does this.
    Leaving(Then),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Linked {
    /// The access point of the link.
    pub access_point: AccessPoint,
    /// The core can connect on this link with no new join. It is `false`
    /// after an MQTT session that did not end cleanly, after a decision to
    /// change access point, and after an association during a sleep.
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
    /// The join with the strongest access point got no address. The link
    /// failed, so the connection waits for its backoff.
    Fail,
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
/// for a suspect link writes a line to the log.
fn rejoin(current: AccessPoint, model: &mut Model) -> Cmd {
    let state = &model.wifi;
    let target = state.cache.plan();
    if state.health.suspect() {
        let reason = logs::warn(
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
/// traffic, but only while the device has a link with an address. Refer to
/// [`Health::ended`] and [`Phase::linked`].
///
/// After an end that is not clean, the link can be the cause, so the next
/// [`link`] joins again. A clean end comes from the core, so the link stays
/// ready.
///
/// The command writes a line to the log only when the link becomes suspect
/// or stops being suspect.
pub fn session_ended(end: &End, model: &mut Model) -> Cmd {
    let has_link = model.wifi.phase.linked().is_some();
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
        (false, true) => logs::warn(
            Area::Wifi,
            format!("the link is suspect, because the MQTT session failed: {end}"),
        ),
        (true, false) => logs::info(
            Area::Wifi,
            "the broker host ended the session, so the link moves data and is not suspect",
        ),
        _ => Cmd::done(),
    }
}

/// Records bytes from the broker, which prove that the link moves data. An
/// end of the stream by the broker host is the same proof. Refer to
/// [`session_ended`].
///
/// The core calls this function for each chunk of bytes. The command writes
/// a line to the log only if the link was suspect.
pub fn traffic(model: &mut Model) -> Cmd {
    let health = &mut model.wifi.health;
    let was_suspect = health.suspect();
    health.traffic();
    if was_suspect {
        logs::info(
            Area::Wifi,
            "bytes from the broker arrived, so the link is not suspect",
        )
    } else {
        Cmd::done()
    }
}

/// The link is quiet, because the subscribe is complete. If a scan for a
/// better access point is due, the core scans now.
pub fn quiet(model: &mut Model) -> Cmd {
    let state = &mut model.wifi;
    let Phase::Up(linked) = state.phase else {
        return Cmd::done();
    };
    if !state.cache.scan_due() || !model.wants_online() {
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
        had_address,
        kept,
    } = model.wifi.phase
    else {
        return Cmd::done();
    };
    if !model.wants_online() {
        return associated_during_sleep(access_point, model);
    }
    let state = &mut model.wifi;
    if lease == Lease::NoLease {
        let recovery = state.health.no_lease();
        let count = counted(state.health);
        if recovery == Recovery::Restart {
            let reason = format!("no address after {RESTART_AFTER} joins");
            return count.and(restart(reason, model));
        }
        return count.and(no_address(target, access_point, model));
    }
    let mut lines = Cmd::done();
    if had_address {
        // The stack kept its address through the join, so the address
        // proves nothing. The line comes only when the address counts: on
        // a new join while the link is suspect. A join that keeps the
        // association keeps the address too.
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
            return lines.and(restart(reason, model));
        }
    } else {
        state.health.leased();
    }
    if !kept {
        state.cache.joined(target, access_point);
    }
    state.phase = Phase::Up(Linked {
        access_point,
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
/// failed. This never counts toward a restart. If the core no longer wants
/// a connection, a join or an address that did not end changes nothing in
/// the cache. The steps of the sleep turn the radio off.
pub fn timeout(radio: u32, model: &mut Model) -> Cmd {
    if !accept(radio, model) {
        return Cmd::done();
    }
    let reason = request_name(model.wifi.phase).map_or_else(Cmd::done, |request| {
        logs::warn(
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
        Phase::Off | Phase::Idle | Phase::Up(_) | Phase::Restarting => Cmd::done(),
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
        Phase::Off | Phase::Idle | Phase::Up(_) | Phase::Restarting => None,
    }
}

/// Continues after the answer to a join.
fn after_join(joined: &Joined, model: &mut Model) -> Cmd {
    let Phase::Joining(target) = model.wifi.phase else {
        return Cmd::done();
    };
    let &Joined::Associated {
        access_point,
        kept,
        had_address,
        ..
    } = joined
    else {
        // A join that fails never counts toward a restart, because the
        // cause is not on the device.
        model.wifi.phase = Phase::Idle;
        return match (model.wifi.cache.failed(target), target) {
            (AfterFailure::Rescan, Target::Cached(cached)) if model.wants_online() => {
                let reason = logs::warn(
                    Area::Wifi,
                    format!(
                        "the join to the cached access point {cached} failed, so join the strongest access point"
                    ),
                );
                reason.and(join(Target::Strongest, false, model))
            }
            (AfterFailure::Fail, _) if model.wants_online() => logs::warn(
                Area::Wifi,
                "the join to the strongest access point failed, so the link is down",
            )
            .and(connection::link_failed(model)),
            (AfterFailure::Rescan | AfterFailure::Fail, _) => connection::link_failed(model),
        };
    };
    if !model.wants_online() {
        return associated_during_sleep(access_point, model);
    }
    model.wifi.phase = Phase::Leasing {
        target,
        access_point,
        had_address,
        kept,
    };
    let radio = next_id(model);
    Radio::address(radio)
        .then_send(move |lease| Event::Leased { radio, lease }.into())
        .and(limit(radio, ADDRESS_TIMEOUT, model))
}

/// Records an association with `access_point` after a sleep started. The
/// core counts nothing and keeps nothing in the cache. The steps of the
/// sleep turn the radio off. If the device wakes before that, [`link`]
/// joins again from `Up`.
fn associated_during_sleep(access_point: AccessPoint, model: &mut Model) -> Cmd {
    model.wifi.phase = Phase::Up(Linked {
        access_point,
        ready: false,
    });
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
            Then::Fail,
            format!("the strongest access point {access_point} gave no address, so leave it"),
        ),
    };
    logs::warn(Area::Wifi, message).and(leave(then, model))
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
        Then::Fail => connection::link_failed(model),
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
    let now = signal(Some(rssi_now));
    let Some(best) = better_access_point(current, *rssi, seen) else {
        return logs::debug(
            Area::Wifi,
            format!(
                "the scan found no access point that is {ROAM_MARGIN_DB} dB stronger than {current} ({now}), so stay"
            ),
        );
    };
    state.cache.roam_to(best.access_point);
    state.phase = Phase::Up(Linked {
        access_point: current,
        ready: false,
    });
    let to = best.access_point;
    // During the steps of a sleep, the sleep closes the session. The next
    // wake joins the better access point.
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

/// Asks the shell to restart the chip, for `reason`.
fn restart(reason: String, model: &mut Model) -> Cmd {
    model.wifi.phase = Phase::Restarting;
    Radio::restart(reason).build()
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
