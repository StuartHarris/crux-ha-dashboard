//! The Wi-Fi settings for the driver.

use esp_hal::time::Duration;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, WifiError,
    scan::{ScanConfig, ScanTypeConfig},
    sta::{ScanMethod, StationConfig},
};
use logic::config::{self, WifiAuth};
use shared::capabilities::radio::Target;

/// Returns the configuration of the Wi-Fi driver for the build-time
/// settings.
///
/// `build.rs` checks the SSID and the password, so the conversion does not
/// fail. If it fails, the function writes the error to the log and returns
/// the default configuration, which joins no network. The device then shows
/// the dashboard offline.
#[must_use]
pub fn controller_config(settings: &config::Config<'_>) -> ControllerConfig {
    match station(settings) {
        Ok(station) => ControllerConfig::default().with_initial_config(Config::Station(station)),
        Err(error) => {
            log::error!("wifi: the settings are not valid: {error:?}");
            ControllerConfig::default()
        }
    }
}

/// Returns the station configuration for a join to `target`.
///
/// For [`Target::Strongest`], it is the configuration of
/// [`controller_config`]. For [`Target::Cached`], the driver does a fast
/// scan for the BSSID, which starts on the channel of the access point.
///
/// # Errors
///
/// Returns the error of the driver if the settings are not valid.
pub fn join_config(settings: &config::Config<'_>, target: Target) -> Result<Config, WifiError> {
    let station = station(settings)?;
    let station = match target {
        Target::Strongest => station,
        Target::Cached(access_point) => station
            .with_bssid(access_point.bssid)
            .with_channel(access_point.channel)
            .with_scan_method(ScanMethod::Fast),
    };
    Ok(Config::Station(station))
}

/// The shortest time of the active scan on each channel.
///
/// On each channel, the driver sends a probe request and waits at least
/// this time for the answers. An access point answers only after it gets
/// the channel, so on a busy channel its answer can arrive late. With the
/// default of `esp-radio` (10 ms), the scan found only 1 of the 3 access
/// points of the network.
const SCAN_MIN: Duration = Duration::from_millis(50);
/// The longest time of the active scan on each channel, if no answer
/// arrives. 120 ms is the default of ESP-IDF. The default of `esp-radio` is
/// 20 ms.
const SCAN_MAX: Duration = Duration::from_millis(120);

/// Returns the configuration of a scan for the access points of the network
/// (the SSID of the settings) on all the channels.
///
/// The scan is active, and it stays [`SCAN_MIN`] to [`SCAN_MAX`] on each
/// channel. For the 13 channels, that is about 0.7 s to 1.6 s. While the
/// device is associated, the driver also goes back to the channel of the
/// access point between the channels, for about 30 ms each time. Thus the
/// whole scan takes about 2 s at most. This is well inside the limit of the
/// radio task (10 s) and the limit of the core (15 s). Each channel stays
/// far below 1.5 s, which is the time at which ESP-IDF says that the access
/// point can disconnect the device.
///
/// # Errors
///
/// Returns the error of the driver if the SSID is not valid.
pub fn scan_config(settings: &config::Config<'_>) -> Result<ScanConfig, WifiError> {
    Ok(ScanConfig::default()
        .with_ssid(settings.wifi_ssid.try_into()?)
        .with_scan_type(ScanTypeConfig::Active {
            min: SCAN_MIN,
            max: SCAN_MAX,
        }))
}

fn station(settings: &config::Config<'_>) -> Result<StationConfig, WifiError> {
    let password = || settings.wifi_password.try_into();
    let authentication = match settings.wifi_auth {
        WifiAuth::Wpa2 => AuthenticationMethodConfig::Wpa2Personal(password()?),
        WifiAuth::WpaWpa2 => AuthenticationMethodConfig::WpaWpa2Personal(password()?),
        WifiAuth::Open => AuthenticationMethodConfig::Open,
    };
    Ok(StationConfig::default()
        .with_ssid(settings.wifi_ssid.try_into()?)
        .with_authentication(authentication)
        // A fast scan joins the first access point with the SSID. A scan of
        // all the channels joins the access point with the strongest signal.
        .with_scan_method(ScanMethod::AllChannels))
}
