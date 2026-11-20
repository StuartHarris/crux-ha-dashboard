//! The Wi-Fi settings for the driver.

use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, WifiError,
    scan::ScanConfig,
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

/// Returns the configuration of a scan for the access points of the network
/// (the SSID of the settings) on all the channels.
///
/// # Errors
///
/// Returns the error of the driver if the SSID is not valid.
pub fn scan_config(settings: &config::Config<'_>) -> Result<ScanConfig, WifiError> {
    Ok(ScanConfig::default().with_ssid(settings.wifi_ssid.try_into()?))
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
