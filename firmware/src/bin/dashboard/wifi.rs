//! The Wi-Fi task: it joins the network for `WifiOn` and leaves it for
//! `WifiOff`.
//!
//! A `WifiOff` during a join stops the join, and the task does not answer
//! it, because the shell already answered the request. A `WifiOn` during a
//! join continues the join, and the answer has the newest token
//! (`logic::wifi`).

use core::pin::pin;

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_time::{Duration, with_timeout};
use esp_radio::wifi::WifiController;
use logic::{
    shell::Signal,
    wifi::{DuringJoin, WifiRequest, during_join},
};
use shared::capabilities::network::WifiStatus;

use crate::{SIGNALS, WIFI};

/// The longest time to join the network.
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest time to get an address from DHCP.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

#[embassy_executor::task]
pub async fn task(mut controller: WifiController<'static>, stack: Stack<'static>) {
    loop {
        let WifiRequest::On { token } = WIFI.pop().await else {
            leave(&mut controller).await;
            continue;
        };
        let mut token = token;
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
            // The join can stop before the device is connected, so the task
            // disconnects also if `is_connected` is false.
            if let Err(error) = controller.disconnect_async().await {
                log::debug!("wifi: disconnect after a stopped join: {error:?}");
            }
            continue;
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

/// Leaves the network, if the device joined it.
async fn leave(controller: &mut WifiController<'static>) {
    if controller.is_connected() {
        // TODO(6): stop the radio to save power, not only disconnect.
        if let Err(error) = controller.disconnect_async().await {
            log::warn!("wifi: disconnect failed: {error:?}");
        }
        log::info!("wifi: off");
    }
}

/// Joins the network and waits for an address. Returns `true` if the device
/// has an address.
async fn join(controller: &mut WifiController<'static>, stack: Stack<'static>) -> bool {
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
