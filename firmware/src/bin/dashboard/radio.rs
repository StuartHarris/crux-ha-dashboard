//! The radio task: it does the radio jobs of the shell (`logic::shell::Job`
//! with a `RadioOp`), and it restarts the chip for `Job::Restart`.
//!
//! The core decides which access point to join, when to scan, when to leave
//! and when to restart the chip (`shared::capabilities::radio`). This task
//! only does the work of the Wi-Fi driver, and answers each job with
//! `Signal::Radio` and the token of the job. The SSID and the password stay
//! in the firmware.
//!
//! `esp-radio` 1.0.0-beta.1 has no public call to stop the radio. The task
//! makes a `WifiController` for the first `Join` after the radio is off, and
//! drops it at the end of an `Off` job. The drop stops the Wi-Fi driver and
//! turns the radio off. The network interface does not belong to the
//! controller, so it stays. The task answers the `Off` job after the drop.
//!
//! Before the drop, the task disconnects, which stops new frames, and waits
//! [`TX_DRAIN`]. The driver counts the frames that it did not send yet, and
//! a drop does not reset that count. If a frame stays in the count after a
//! drop, the next controller can send fewer frames. The core closes the
//! stream before it sends `Off`, so no MQTT packet waits.
//!
//! A new job stops each older job that waits, as the stop rule of the radio
//! capability tells. The shell answers the stopped jobs itself, and gives
//! their tokens in the `stop` list of the new job. The task then obeys these
//! rules:
//!
//! - While a job runs, the task also waits for the next request. If the
//!   request stops the job, the task drops the job and sends no answer. A
//!   `Restart` stops each job.
//! - Before the task starts a job, it takes all the requests that wait. It
//!   skips each job that a later job stops, and it does a `Restart` first.
//! - After a `Join`, a `Leave` or an `Off` stops, the driver can still try
//!   to connect, or it can be part-way through a disconnect. The task then
//!   completes a disconnect, so a later `Join` with `keep` never keeps a
//!   link that the task started to leave.
//! - `esp-radio` has no public call to stop a scan. When a `Scan` job stops,
//!   the task drops the scan, and the driver completes the scan that it
//!   started. A `StopScan` job only answers, because the `Scan` job already
//!   stopped.
//!
//! The log tells each job with its token, the time of each join, its access
//! point and its signal, the address from DHCP and the result of each scan.

use alloc::{collections::VecDeque, format, string::ToString, vec::Vec};

use dashboard_firmware::network;
use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::peripherals::WIFI as WifiPeripheral;
use esp_hal::system::software_reset;
use esp_radio::wifi::WifiController;
use logic::shell::{RadioAnswer, RadioOp, Signal};
use shared::capabilities::radio::{
    AccessPoint, Join, Joined, Lease, Restart, Scanned, Seen, Target,
};

use crate::{RADIO, SIGNALS, config::CONFIG};

/// The longest time for a `Join` job with a scan of all the channels. The
/// limit includes the leave before the join and the disconnect after a
/// failure.
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time for a `Join` job to the cached access point. The scan
/// for one BSSID that starts on its channel is short, so a longer join means
/// that the access point is gone or moved. The limit includes the leave
/// before the join and the disconnect after a failure.
const CACHED_JOIN_TIMEOUT: Duration = Duration::from_secs(8);
/// The longest time to get an address from DHCP.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time to scan all the channels.
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest time for the network stack to drop the address after a
/// disconnect.
const LINK_DOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest time to wait for the disconnect.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// The time from the disconnect to the drop of the controller, for the
/// frames in the driver.
const TX_DRAIN: Duration = Duration::from_millis(100);
/// The time for the log line to go out before a restart of the chip.
const LOG_DRAIN: Duration = Duration::from_millis(200);

/// Work for the radio task.
#[derive(Debug)]
pub enum Request {
    /// A radio job (`logic::shell::Job::Radio`).
    Job {
        /// The token of the job.
        token: u32,
        /// The tokens of the older jobs that this job stops.
        stop: Vec<u32>,
        /// The operation.
        op: RadioOp,
    },
    /// Restart the chip (`logic::shell::Job::Restart`).
    Restart(Restart),
}

/// The requests that the task took from [`RADIO`] and did not do yet, from
/// the oldest.
#[derive(Default)]
struct Queue(VecDeque<Request>);

impl Queue {
    /// Adds `request`. A `Restart` goes to the front. A job removes each
    /// queued job that it stops, with no answer, because the shell already
    /// answered it.
    fn push(&mut self, request: Request) {
        match &request {
            Request::Restart(_) => self.0.push_front(request),
            Request::Job {
                token: newer, stop, ..
            } => {
                self.0.retain(|queued| match queued {
                    Request::Job { token, .. } if stop.contains(token) => {
                        log::info!(
                            "wifi: radio job {token} stops for job {newer} before it starts"
                        );
                        false
                    }
                    _ => true,
                });
                self.0.push_back(request);
            }
        }
    }

    /// Takes all the requests from [`RADIO`], and returns the next one to
    /// do. The future is safe to cancel.
    async fn next(&mut self) -> Request {
        loop {
            for request in RADIO.take_all() {
                self.push(request);
            }
            if let Some(request) = self.0.pop_front() {
                return request;
            }
            // No await follows the pop, so a cancel loses no request.
            let request = RADIO.pop().await;
            self.push(request);
        }
    }

    /// Takes the requests from [`RADIO`] until one stops the job `token`: a
    /// job with `token` in its `stop` list, or a `Restart`. The future is
    /// safe to cancel.
    async fn stops(&mut self, token: u32) {
        loop {
            // No await follows the pop, so a cancel loses no request.
            let request = RADIO.pop().await;
            let stops = match &request {
                Request::Job { stop, .. } => stop.contains(&token),
                Request::Restart(_) => true,
            };
            self.push(request);
            if stops {
                log::info!("wifi: radio job {token} stops for a newer request");
                return;
            }
        }
    }
}

#[embassy_executor::task]
pub async fn task(mut wifi: WifiPeripheral<'static>, stack: Stack<'static>) {
    let mut queue = Queue::default();
    loop {
        // The radio is off here.
        let (token, op) = match queue.next().await {
            Request::Restart(restart) => restart_chip(&restart).await,
            Request::Job { token, op, .. } => (token, op),
        };
        log_job(token, &op);
        let answer = match op {
            RadioOp::Join(join) => {
                if let Some(off) = radio_on(wifi.reborrow(), stack, token, join, &mut queue).await {
                    log::info!("wifi: off, and the radio is off");
                    reply(off, RadioAnswer::Done);
                }
                continue;
            }
            RadioOp::Address(_) => {
                log::info!("wifi: no address, because the radio is off");
                RadioAnswer::Lease(Lease::NoLease)
            }
            RadioOp::Scan(_) => RadioAnswer::Scanned(Scanned::Failed("the radio is off".into())),
            // The radio is already off, so the device is not associated and
            // no scan runs.
            RadioOp::Leave(_) | RadioOp::StopScan(_) | RadioOp::Off(_) => RadioAnswer::Done,
        };
        reply(token, answer);
    }
}

/// Sends the answer to the job `token` to the core task.
fn reply(token: u32, answer: RadioAnswer) {
    SIGNALS.push(Signal::Radio { token, answer });
}

/// Turns the radio on, does the `Join` job `token`, and then does the jobs
/// after it. Returns the token of the `Off` job that turned the radio off.
/// The return drops the controller, which turns the radio off. Returns
/// `None` if the radio does not start.
async fn radio_on(
    wifi: WifiPeripheral<'_>,
    stack: Stack<'static>,
    token: u32,
    join: Join,
    queue: &mut Queue,
) -> Option<u32> {
    let mut controller = match WifiController::new(wifi, network::controller_config(&CONFIG)) {
        Ok(controller) => controller,
        Err(error) => {
            log::warn!("wifi: the radio does not start: {error:?}");
            let reason = format!("the radio does not start: {error:?}");
            reply(token, RadioAnswer::Joined(Joined::Failed(reason)));
            return None;
        }
    };
    let mut next = Some((token, RadioOp::Join(join)));
    loop {
        let (token, op) = match next.take() {
            Some(job) => job,
            None => match queue.next().await {
                Request::Restart(restart) => restart_chip(&restart).await,
                Request::Job { token, op, .. } => {
                    log_job(token, &op);
                    (token, op)
                }
            },
        };
        match select(work(&mut controller, stack, &op), queue.stops(token)).await {
            Either::First(Outcome::Answer(outcome)) => reply(token, outcome),
            Either::First(Outcome::Off) => return Some(token),
            Either::Second(()) => {
                // A join can stop before the device is connected, and the
                // driver then still tries to connect. A leave or an off can
                // stop part-way through the disconnect, and `is_connected`
                // can then still be true. A disconnect completes each of
                // them, also if `is_connected` is false.
                if matches!(op, RadioOp::Join(_) | RadioOp::Leave(_) | RadioOp::Off(_)) {
                    disconnect(&mut controller).await;
                }
            }
        }
    }
}

/// The end of a job.
enum Outcome {
    /// The answer to the job.
    Answer(RadioAnswer),
    /// The `Off` job is ready for the drop of the controller.
    Off,
}

/// Does the job `op` with the radio on. The future is safe to cancel, but
/// after a cancel of a `Join`, a `Leave` or an `Off`, the caller must
/// disconnect.
async fn work(controller: &mut WifiController<'_>, stack: Stack<'static>, op: &RadioOp) -> Outcome {
    let answer = match op {
        RadioOp::Join(join) => RadioAnswer::Joined(self::join(controller, stack, *join).await),
        RadioOp::Address(_) => RadioAnswer::Lease(address(stack).await),
        RadioOp::Leave(_) => {
            leave(controller, stack).await;
            RadioAnswer::Done
        }
        RadioOp::Scan(_) => RadioAnswer::Scanned(scan(controller).await),
        // The new job stopped the scan, so this job has no more work.
        RadioOp::StopScan(_) => RadioAnswer::Done,
        RadioOp::Off(_) => {
            log::info!("wifi: stop, because Wi-Fi is off");
            stop(controller).await;
            return Outcome::Off;
        }
    };
    Outcome::Answer(answer)
}

/// Writes the job `token` to the log.
fn log_job(token: u32, op: &RadioOp) {
    match op {
        RadioOp::Join(Join { target, keep, .. }) => match target {
            Target::Cached(access_point) => log::info!(
                "wifi: radio job {token}: join {:02x?} on channel {} (keep {keep})",
                access_point.bssid,
                access_point.channel,
            ),
            Target::Strongest => {
                log::info!("wifi: radio job {token}: join the strongest (keep {keep})");
            }
        },
        RadioOp::Address(_) => log::info!("wifi: radio job {token}: wait for an address"),
        RadioOp::Leave(_) => log::info!("wifi: radio job {token}: leave the access point"),
        RadioOp::Scan(_) => log::info!("wifi: radio job {token}: scan all the channels"),
        RadioOp::StopScan(_) => log::info!("wifi: radio job {token}: stop the scan"),
        RadioOp::Off(_) => log::info!("wifi: radio job {token}: turn the radio off"),
    }
}

/// Writes the reason to the log, waits [`LOG_DRAIN`] for the log to go, and
/// restarts the chip.
async fn restart_chip(restart: &Restart) -> ! {
    log::warn!("wifi: restart the chip: {}", restart.reason);
    Timer::after(LOG_DRAIN).await;
    software_reset()
}

/// Does the `Join` job in its time limit, [`CACHED_JOIN_TIMEOUT`] or
/// [`JOIN_TIMEOUT`]. The limit includes the leave before the join and the
/// disconnect after a failure, so the answer always comes before the time
/// limit of the core.
async fn join(controller: &mut WifiController<'_>, stack: Stack<'static>, join: Join) -> Joined {
    let limit = match join.target {
        Target::Cached(_) => CACHED_JOIN_TIMEOUT,
        Target::Strongest => JOIN_TIMEOUT,
    };
    // The time for the disconnect after the limit is part of the limit.
    let attempt = with_timeout(
        limit - DISCONNECT_TIMEOUT,
        associate(controller, stack, join),
    );
    if let Ok(joined) = attempt.await {
        return joined;
    }
    let reason = format!("join timed out after {} s", limit.as_secs());
    log::warn!("wifi: {reason}");
    // The driver can still try to connect after the limit, or it can be
    // part-way through the leave, so stop that.
    disconnect(controller).await;
    Joined::Failed(reason)
}

/// Associates with `join.target`, and returns the answer to the `Join` job.
/// The caller sets the time limit.
///
/// If the device is associated and `join.keep` is `true`, the function
/// answers at once with `kept: true`. If the device is associated and
/// `join.keep` is `false`, the function leaves the access point first.
async fn associate(
    controller: &mut WifiController<'_>,
    stack: Stack<'static>,
    join: Join,
) -> Joined {
    if controller.is_connected() {
        match (join.keep, controller.ap_info()) {
            (true, Ok(info)) => {
                let rssi = controller.rssi().ok();
                log::info!(
                    "wifi: keep {:02x?} on channel {}, signal {rssi:?} dBm",
                    info.bssid,
                    info.channel,
                );
                return Joined::Associated {
                    access_point: AccessPoint {
                        bssid: info.bssid,
                        channel: info.channel,
                    },
                    rssi,
                    kept: true,
                    had_address: stack.is_config_up(),
                };
            }
            (true, Err(error)) => {
                log::warn!("wifi: the access point is not known ({error:?}), so join again");
            }
            (false, _) => log::info!("wifi: leave the access point before the join"),
        }
        leave(controller, stack).await;
    }
    let config = match network::join_config(&CONFIG, join.target) {
        Ok(config) => config,
        Err(error) => {
            log::warn!("wifi: the settings are not valid: {error:?}");
            return Joined::Failed(format!("the settings are not valid: {error:?}"));
        }
    };
    if let Err(error) = controller.set_config(&config) {
        log::warn!("wifi: the configuration failed: {error:?}");
        return Joined::Failed(format!("the configuration failed: {error:?}"));
    }
    log::info!("wifi: join the network with {}", target_name(join.target));
    // An address from before the join is not a new lease.
    let had_address = stack.is_config_up();
    let started = Instant::now();
    let error = match controller.connect_async().await {
        Ok(info) => {
            let elapsed = started.elapsed().as_millis();
            let rssi = controller.rssi().ok();
            log::info!(
                "wifi: joined {:02x?} on channel {} with {} in {elapsed} ms, signal {rssi:?} dBm",
                info.bssid,
                info.channel,
                target_name(join.target),
            );
            return Joined::Associated {
                access_point: AccessPoint {
                    bssid: info.bssid,
                    channel: info.channel,
                },
                rssi,
                kept: false,
                had_address,
            };
        }
        Err(error) => error,
    };
    let reason = format!("join failed: {error:?}");
    let elapsed = started.elapsed().as_millis();
    log::warn!("wifi: {reason} ({elapsed} ms)");
    // A disconnect makes sure that the driver does not try to connect again.
    disconnect(controller).await;
    Joined::Failed(reason)
}

/// Returns the name of `target` for the log.
const fn target_name(target: Target) -> &'static str {
    match target {
        Target::Cached(_) => "the cached access point (no scan)",
        Target::Strongest => "a scan of all the channels",
    }
}

/// Waits for an address from DHCP, for no more than [`DHCP_TIMEOUT`].
async fn address(stack: Stack<'static>) -> Lease {
    if with_timeout(DHCP_TIMEOUT, stack.wait_config_up())
        .await
        .is_err()
    {
        log::warn!("wifi: no address from DHCP");
        return Lease::NoLease;
    }
    let Some(config) = stack.config_v4() else {
        // The stack is up only with an IPv4 configuration, so this does not
        // occur. With no address, the core cannot use the link.
        log::warn!("wifi: the network stack is up with no IPv4 address");
        return Lease::NoLease;
    };
    log::info!("wifi: address {}", config.address);
    Lease::Leased {
        ip: config.address.address().octets(),
    }
}

/// Leaves the access point, and waits for the network stack to drop the
/// address.
async fn leave(controller: &mut WifiController<'_>, stack: Stack<'static>) {
    // A join that stopped can leave the driver in a connect, so the task
    // disconnects also if `is_connected` is false.
    disconnect(controller).await;
    // The stack keeps the old address until it sees the link go down.
    // Without this wait, the next join does not wait for a new lease.
    if with_timeout(LINK_DOWN_TIMEOUT, stack.wait_config_down())
        .await
        .is_err()
    {
        log::warn!("wifi: the old address stays after the disconnect");
    }
}

/// Scans all the channels for the access points of the network, for no
/// more than [`SCAN_TIMEOUT`]. Then reads the signal of the current access
/// point.
async fn scan(controller: &mut WifiController<'_>) -> Scanned {
    let config = match network::scan_config(&CONFIG) {
        Ok(config) => config,
        Err(error) => {
            log::warn!("wifi: the settings are not valid: {error:?}");
            return Scanned::Failed(format!("the settings are not valid: {error:?}"));
        }
    };
    let started = Instant::now();
    let found = match with_timeout(SCAN_TIMEOUT, controller.scan_async(&config)).await {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => {
            log::warn!("wifi: the scan failed: {error:?}");
            return Scanned::Failed(format!("the scan failed: {error:?}"));
        }
        Err(_) => {
            let secs = SCAN_TIMEOUT.as_secs();
            log::warn!("wifi: the scan timed out after {secs} s");
            return Scanned::Failed(format!("the scan timed out after {secs} s"));
        }
    };
    let seen: Vec<Seen> = found
        .iter()
        .map(|info| Seen {
            access_point: AccessPoint {
                bssid: info.bssid,
                channel: info.channel,
            },
            rssi: i32::from(info.signal_strength),
        })
        .collect();
    let rssi = controller.rssi().ok();
    let elapsed = started.elapsed().as_millis();
    let current = controller.ap_info().map_or_else(
        |_| "none".to_string(),
        |info| format!("{:02x?} on channel {}", info.bssid, info.channel),
    );
    log::info!(
        "wifi: the scan found {} access points in {elapsed} ms. Current {current}, signal {rssi:?} dBm",
        seen.len(),
    );
    for found in &seen {
        log::info!(
            "wifi:   {:02x?} on channel {}, signal {} dBm",
            found.access_point.bssid,
            found.access_point.channel,
            found.rssi
        );
    }
    Scanned::Found { seen, rssi }
}

/// Prepares the drop of the controller: disconnects, and waits for the
/// frames in the driver.
async fn stop(controller: &mut WifiController<'_>) {
    // A join that stopped can leave the driver in a connect, so the task
    // disconnects also if `is_connected` is false.
    disconnect(controller).await;
    Timer::after(TX_DRAIN).await;
}

/// Disconnects from the access point, or stops a connect, for no more than
/// [`DISCONNECT_TIMEOUT`]. If the driver does not connect and is not
/// connected, returns at once: `disconnect_async` then returns
/// `WifiError::NotConnected` with no wait (`esp-radio` 1.0.0-beta.1,
/// `src/wifi/mod.rs:3412`).
async fn disconnect(controller: &mut WifiController<'_>) {
    match with_timeout(DISCONNECT_TIMEOUT, controller.disconnect_async()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::debug!("wifi: disconnect: {error:?}"),
        Err(_) => log::debug!("wifi: disconnect timed out"),
    }
}
