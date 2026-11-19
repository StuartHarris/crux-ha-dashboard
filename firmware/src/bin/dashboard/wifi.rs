//! The Wi-Fi task: it turns the radio on and joins the network for
//! `WifiOn`, and it leaves the network and turns the radio off for
//! `WifiOff`.
//!
//! `esp-radio` 1.0.0-beta.1 has no public call to stop the radio. The task
//! makes a `WifiController` for `WifiOn`, and drops it for `WifiOff`. The
//! drop stops the Wi-Fi driver and turns the radio off. The network
//! interface does not belong to the controller, so it stays. When the radio
//! is off, the task answers each `WifiOff` with `Signal::RadioOff`.
//!
//! Before the drop, the task disconnects, which stops new frames, and waits
//! [`TX_DRAIN`]. The driver counts the frames that it did not send yet, and
//! a drop does not reset that count. If a frame stays in the count after a
//! drop, the next controller can send fewer frames. The core closes the MQTT
//! connection before it sends `WifiOff`, so no MQTT packet waits.
//!
//! A `WifiOff` during a join stops the join, and the task does not answer
//! the `WifiOn`, because the shell already answered it. A `WifiOn` during a
//! join continues the join, and the answer has the newest token
//! (`logic::wifi`).
//!
//! The task keeps the access point of the last good join in a
//! `logic::wifi::ApCache`, which light sleep keeps in RAM. The next join goes
//! to that BSSID on its channel, with no scan of all the channels. If that
//! join fails, the task forgets the access point and scans all the channels
//! in the same `WifiOn`. After a join with the cached access point, the task
//! scans all the channels once, when the core sends `WifiQuiet`, and changes
//! to an access point that is clearly better. A `Hold` (the first network
//! step of a sleep) or any other request stops that scan. The log tells which
//! path each join took, its time, the scan result and each change.

use alloc::vec::Vec;
use core::pin::pin;

use dashboard_firmware::network;
use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::peripherals::WIFI as WifiPeripheral;
use esp_radio::wifi::WifiController;
use logic::{
    shell::Signal,
    wifi::{
        AccessPoint, AfterFailure, ApCache, DuringSwitch, JoinPath, Seen, WifiRequest,
        better_access_point, during_switch,
    },
};
use shared::capabilities::network::WifiStatus;

use crate::{LINK_CHANGED, SIGNALS, WIFI, config::CONFIG};

/// The longest time to join the network with a scan of all the channels.
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time to join the cached access point. The scan for one
/// BSSID that starts on its channel is short, so a longer join means that
/// the access point is gone or moved. The task then scans all the channels.
const CACHED_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest time to get an address from DHCP.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time for the network stack to drop the address after a
/// disconnect.
const LINK_DOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest time to wait for the disconnect.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// The time from the disconnect to the drop of the controller, for the
/// frames in the driver.
const TX_DRAIN: Duration = Duration::from_millis(100);

#[embassy_executor::task]
pub async fn task(mut wifi: WifiPeripheral<'static>, stack: Stack<'static>) {
    // The controller is dropped at each `WifiOff`, so the cache is here.
    let mut cache = ApCache::default();
    loop {
        // The radio is off here.
        match WIFI.pop().await {
            WifiRequest::On { token } => {
                if radio_on(wifi.reborrow(), stack, token, &mut cache).await {
                    log::info!("wifi: off, and the radio is off");
                    SIGNALS.push(Signal::RadioOff);
                }
            }
            // The radio is already off.
            WifiRequest::Off => SIGNALS.push(Signal::RadioOff),
            // With the radio off, there is no link to scan from.
            WifiRequest::Roam | WifiRequest::Hold => {}
        }
    }
}

/// Turns the radio on, and answers the `WifiOn` request with `token` and
/// the requests after it. Returns `true` at the next `WifiOff`. The return
/// drops the controller, which turns the radio off. Returns `false` if the
/// radio does not start.
async fn radio_on(
    wifi: WifiPeripheral<'_>,
    stack: Stack<'static>,
    token: u32,
    cache: &mut ApCache,
) -> bool {
    let mut controller = match WifiController::new(wifi, network::controller_config(&CONFIG)) {
        Ok(controller) => controller,
        Err(error) => {
            log::warn!("wifi: the radio does not start: {error:?}");
            SIGNALS.push(Signal::Wifi {
                token,
                status: WifiStatus::Failed,
            });
            return false;
        }
    };
    let mut next = Some(WifiRequest::On { token });
    loop {
        let request = match next.take() {
            Some(request) => request,
            None => WIFI.pop().await,
        };
        let outcome = match request {
            WifiRequest::Off => None,
            // No scan runs, so there is nothing to stop.
            WifiRequest::Hold => continue,
            WifiRequest::Roam => match roam(&mut controller, stack, cache).await {
                Roamed::Done => continue,
                Roamed::Interrupted(request) => {
                    next = Some(request);
                    continue;
                }
                Roamed::Joined(outcome) => outcome,
            },
            WifiRequest::On { token } => {
                let path = cache.plan();
                watched_join(&mut controller, stack, cache, path, Some(token)).await
            }
        };
        let Some((token, joined)) = outcome else {
            log::info!("wifi: stop, because Wi-Fi is off");
            stop(&mut controller).await;
            return true;
        };
        let status = if joined {
            WifiStatus::Connected
        } else {
            WifiStatus::Failed
        };
        log::info!("wifi: {status:?}");
        if let Some(token) = token {
            SIGNALS.push(Signal::Wifi { token, status });
        }
    }
}

/// Joins on `path` while the task takes the requests that arrive. Returns
/// the newest `WifiOn` token to answer (`token` or a newer one) and the
/// result, or `None` if a `WifiOff` stopped the join.
async fn watched_join(
    controller: &mut WifiController<'_>,
    stack: Stack<'static>,
    cache: &mut ApCache,
    path: JoinPath,
    mut token: Option<u32>,
) -> Option<(Option<u32>, bool)> {
    let mut joining = pin!(join(controller, stack, cache, path));
    loop {
        match select(joining.as_mut(), WIFI.pop()).await {
            Either::First(joined) => return Some((token, joined)),
            Either::Second(request) => match during_switch(token, request) {
                DuringSwitch::Continue { token: newer } => token = newer,
                DuringSwitch::Cancel => return None,
            },
        }
    }
}

/// The end of a scan for a better access point.
enum Roamed {
    /// The task stays on the current access point.
    Done,
    /// A request arrived during the scan. The scan stopped, and the task
    /// does the request next.
    Interrupted(WifiRequest),
    /// The task changed the access point. The `WifiOn` token to answer and
    /// the result, as from [`watched_join`].
    Joined(Option<(Option<u32>, bool)>),
}

/// Scans all the channels once after a join with the cached access point,
/// and changes to an access point that is clearly better
/// (`logic::wifi::better_access_point`).
///
/// The core sends `WifiQuiet` when the MQTT connection is ready, so a
/// change does not delay the connect. Any request stops the scan. After a
/// change, the task sets [`LINK_CHANGED`], and the MQTT task closes its
/// connection. The core then connects again and subscribes.
async fn roam(
    controller: &mut WifiController<'_>,
    stack: Stack<'static>,
    cache: &mut ApCache,
) -> Roamed {
    // The scan stays due until it completes or fails, so a scan that does
    // not run now runs at the next `WifiQuiet`.
    if !cache.scan_due() {
        log::debug!("wifi: no scan is due");
        return Roamed::Done;
    }
    let Some(current) = cache.current() else {
        return Roamed::Done;
    };
    if !controller.is_connected() {
        log::info!("wifi: no scan, because the link is down");
        return Roamed::Done;
    }
    let config = match network::scan_config(&CONFIG) {
        Ok(config) => config,
        Err(error) => {
            log::warn!("wifi: the settings are not valid: {error:?}");
            cache.scanned();
            return Roamed::Done;
        }
    };
    let started = Instant::now();
    let found = match select(controller.scan_async(&config), WIFI.pop()).await {
        Either::First(Ok(found)) => found,
        Either::First(Err(error)) => {
            log::warn!("wifi: the scan failed: {error:?}");
            cache.scanned();
            return Roamed::Done;
        }
        Either::Second(request) => {
            log::info!("wifi: the scan stopped for {request:?}, so it stays due");
            return Roamed::Interrupted(request);
        }
    };
    cache.scanned();
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
    log::info!(
        "wifi: the scan found {} access points in {} ms. Current {:02x?} on channel {}, signal {rssi:?} dBm",
        seen.len(),
        started.elapsed().as_millis(),
        current.bssid,
        current.channel,
    );
    for found in &seen {
        log::info!(
            "wifi:   {:02x?} on channel {}, signal {} dBm",
            found.access_point.bssid,
            found.access_point.channel,
            found.rssi
        );
    }
    let Some(best) = better_access_point(current, rssi, &seen) else {
        log::info!("wifi: stay on the current access point");
        return Roamed::Done;
    };
    log::info!(
        "wifi: change to {:02x?} on channel {}, signal {} dBm",
        best.access_point.bssid,
        best.access_point.channel,
        best.rssi
    );
    leave(controller).await;
    // The stack keeps the old address until it sees the link go down. Wait
    // for that, so the join below waits for a new lease from DHCP.
    if with_timeout(LINK_DOWN_TIMEOUT, stack.wait_config_down())
        .await
        .is_err()
    {
        log::warn!("wifi: the old address stays after the disconnect");
    }
    let path = JoinPath::Cached(best.access_point);
    let outcome = watched_join(controller, stack, cache, path, None).await;
    cache.switched();
    LINK_CHANGED.signal(());
    Roamed::Joined(outcome)
}

/// Prepares the drop of the controller: disconnects, and waits for the
/// frames in the driver.
async fn stop(controller: &mut WifiController<'_>) {
    // The join can stop before the device is connected, so the task
    // disconnects also if `is_connected` is false.
    leave(controller).await;
    Timer::after(TX_DRAIN).await;
}

/// Joins the network and waits for an address. Returns `true` if the device
/// has an address.
///
/// The first try uses `path`. If a join with a cached access point fails,
/// `cache` forgets it, and the task scans all the channels.
async fn join(
    controller: &mut WifiController<'_>,
    stack: Stack<'static>,
    cache: &mut ApCache,
    mut path: JoinPath,
) -> bool {
    if controller.is_connected() {
        // The link is up, so only DHCP is missing.
        return address(stack).await;
    }
    loop {
        let started = Instant::now();
        let Some(access_point) = attempt(controller, stack, path).await else {
            let elapsed = started.elapsed().as_millis();
            leave(controller).await;
            match cache.failed(path) {
                AfterFailure::Rescan => {
                    log::warn!(
                        "wifi: the join to the cached access point failed after {elapsed} ms, so scan all the channels"
                    );
                    path = JoinPath::Scan;
                    continue;
                }
                AfterFailure::Fail => return false,
            }
        };
        let elapsed = started.elapsed().as_millis();
        let rssi = controller.rssi().ok();
        log::info!(
            "wifi: joined {:02x?} on channel {} with {} in {elapsed} ms, signal {rssi:?} dBm",
            access_point.bssid,
            access_point.channel,
            path_name(path),
        );
        cache.joined(path, access_point);
        return true;
    }
}

/// Returns the name of `path` for the log.
const fn path_name(path: JoinPath) -> &'static str {
    match path {
        JoinPath::Cached(_) => "the cached access point (no scan)",
        JoinPath::Scan => "a scan of all the channels",
    }
}

/// Joins the network on `path` and waits for an address. Returns the access
/// point, or `None` if the join or DHCP failed.
async fn attempt(
    controller: &mut WifiController<'_>,
    stack: Stack<'static>,
    path: JoinPath,
) -> Option<AccessPoint> {
    let config = match network::join_config(&CONFIG, path) {
        Ok(config) => config,
        Err(error) => {
            log::warn!("wifi: the settings are not valid: {error:?}");
            return None;
        }
    };
    if let Err(error) = controller.set_config(&config) {
        log::warn!("wifi: the configuration failed: {error:?}");
        return None;
    }
    let limit = match path {
        JoinPath::Cached(_) => CACHED_JOIN_TIMEOUT,
        JoinPath::Scan => JOIN_TIMEOUT,
    };
    log::info!("wifi: join the network with {}", path_name(path));
    let info = match with_timeout(limit, controller.connect_async()).await {
        Ok(Ok(info)) => info,
        Ok(Err(error)) => {
            log::warn!("wifi: join failed: {error:?}");
            return None;
        }
        Err(_) => {
            log::warn!("wifi: join timed out after {} s", limit.as_secs());
            return None;
        }
    };
    address(stack).await.then_some(AccessPoint {
        bssid: info.bssid,
        channel: info.channel,
    })
}

/// Waits for an address from DHCP. Returns `true` if the device has one.
async fn address(stack: Stack<'static>) -> bool {
    if with_timeout(DHCP_TIMEOUT, stack.wait_config_up())
        .await
        .is_err()
    {
        log::warn!("wifi: no address from DHCP");
        return false;
    }
    if let Some(config) = stack.config_v4() {
        log::info!("wifi: address {}", config.address);
    }
    true
}

/// Leaves the access point, or stops a join in progress, before the next
/// join.
async fn leave(controller: &mut WifiController<'_>) {
    match with_timeout(DISCONNECT_TIMEOUT, controller.disconnect_async()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::debug!("wifi: disconnect: {error:?}"),
        Err(_) => log::debug!("wifi: disconnect timed out"),
    }
}
