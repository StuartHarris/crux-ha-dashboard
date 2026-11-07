//! The Wi-Fi settings for the driver.

use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, WifiError, sta::StationConfig,
};
use logic::config::{self, WifiAuth};

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

fn station(settings: &config::Config<'_>) -> Result<StationConfig, WifiError> {
    let password = || settings.wifi_password.try_into();
    let authentication = match settings.wifi_auth {
        WifiAuth::Wpa2 => AuthenticationMethodConfig::Wpa2Personal(password()?),
        WifiAuth::WpaWpa2 => AuthenticationMethodConfig::WpaWpa2Personal(password()?),
        WifiAuth::Open => AuthenticationMethodConfig::Open,
    };
    Ok(StationConfig::default()
        .with_ssid(settings.wifi_ssid.try_into()?)
        .with_authentication(authentication))
}
