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

use core::pin::pin;

use dashboard_firmware::network;
use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::peripherals::WIFI as WifiPeripheral;
use esp_radio::wifi::WifiController;
use logic::{
    shell::Signal,
    wifi::{DuringJoin, WifiRequest, during_join},
};
use shared::capabilities::network::WifiStatus;

use crate::{SIGNALS, WIFI, config::CONFIG};

/// The longest time to join the network.
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time to get an address from DHCP.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time to wait for the disconnect.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// The time from the disconnect to the drop of the controller, for the
/// frames in the driver.
const TX_DRAIN: Duration = Duration::from_millis(100);

#[embassy_executor::task]
pub async fn task(mut wifi: WifiPeripheral<'static>, stack: Stack<'static>) {
    loop {
        // The radio is off here.
        let WifiRequest::On { token } = WIFI.pop().await else {
            // The radio is already off.
            SIGNALS.push(Signal::RadioOff);
            continue;
        };
        if radio_on(wifi.reborrow(), stack, token).await {
            log::info!("wifi: off, and the radio is off");
            SIGNALS.push(Signal::RadioOff);
        }
    }
}

/// Turns the radio on, and answers the `WifiOn` request with `token` and
/// the requests after it. Returns `true` at the next `WifiOff`. The return
/// drops the controller, which turns the radio off. Returns `false` if the
/// radio does not start.
async fn radio_on(wifi: WifiPeripheral<'_>, stack: Stack<'static>, token: u32) -> bool {
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
    let mut next = Some(token);
    loop {
        let mut token = match next.take() {
            Some(token) => token,
            None => match WIFI.pop().await {
                WifiRequest::On { token } => token,
                WifiRequest::Off => {
                    stop(&mut controller).await;
                    return true;
                }
            },
        };
        let joined = {
            let mut joining = pin!(join(&mut controller, stack));
            loop {
                match select(joining.as_mut(), WIFI.pop()).await {
                    Either::First(joined) => break Some(joined),
                    Either::Second(request) => match during_join(token, request) {
                        DuringJoin::Continue { token: newer } => token = newer,
                        DuringJoin::Cancel => break None,
                    },
                }
            }
        };
        let Some(joined) = joined else {
            log::info!("wifi: the join stopped, because Wi-Fi is off");
            stop(&mut controller).await;
            return true;
        };
        let status = if joined {
            WifiStatus::Connected
        } else {
            WifiStatus::Failed
        };
        log::info!("wifi: {status:?}");
        SIGNALS.push(Signal::Wifi { token, status });
    }
}

/// Prepares the drop of the controller: disconnects, and waits for the
/// frames in the driver.
async fn stop(controller: &mut WifiController<'_>) {
    // The join can stop before the device is connected, so the task
    // disconnects also if `is_connected` is false.
    match with_timeout(DISCONNECT_TIMEOUT, controller.disconnect_async()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::debug!("wifi: disconnect: {error:?}"),
        Err(_) => log::debug!("wifi: disconnect timed out"),
    }
    Timer::after(TX_DRAIN).await;
}

/// Joins the network and waits for an address. Returns `true` if the device
/// has an address.
async fn join(controller: &mut WifiController<'_>, stack: Stack<'static>) -> bool {
    if controller.is_connected() && stack.is_config_up() {
        return true;
    }
    if !controller.is_connected() {
        log::info!("wifi: join the network");
        match with_timeout(JOIN_TIMEOUT, controller.connect_async()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                log::warn!("wifi: join failed: {error:?}");
                return false;
            }
            Err(_) => {
                log::warn!("wifi: join timed out");
                return false;
            }
        }
    }
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
