//! Reads the build-time configuration from `cfg.toml` and writes it into the
//! firmware as a constant.
//!
//! The constant goes to `$OUT_DIR/config.rs`, which is in the `target`
//! directory. The build never prints a value from the file.

use std::{env, fs, path::PathBuf, process};

#[path = "logic/src/config.rs"]
mod config;

/// The environment variable that gives another configuration file, for
/// example `cfg.toml.example` in CI.
const PATH_VARIABLE: &str = "DASHBOARD_CONFIG";

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    // A relative path is relative to the `firmware` directory.
    let path = manifest.join(env::var_os(PATH_VARIABLE).unwrap_or_else(|| "cfg.toml".into()));
    println!("cargo:rerun-if-env-changed={PATH_VARIABLE}");
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed=logic/src/config.rs");

    let Ok(text) = fs::read_to_string(&path) else {
        eprintln!(
            "error: cannot read {}. Copy cfg.toml.example to cfg.toml and put \
             your Wi-Fi and broker settings in it.",
            path.display()
        );
        process::exit(1);
    };
    let config = match config::Config::parse(&text) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {}: {error}", path.display());
            process::exit(1);
        }
    };

    let config::Config {
        wifi_ssid,
        wifi_password,
        wifi_auth,
        mqtt_host,
        mqtt_port,
        mqtt_username,
        mqtt_password,
        device_id,
        read_only,
        ntp_server,
    } = config;
    let source = format!(
        "/// The build-time configuration, from `cfg.toml`.\n\
         pub const CONFIG: logic::config::Config<'static> = logic::config::Config {{\n\
         wifi_ssid: {wifi_ssid:?},\n\
         wifi_password: {wifi_password:?},\n\
         wifi_auth: logic::config::WifiAuth::{wifi_auth:?},\n\
         mqtt_host: {mqtt_host:?},\n\
         mqtt_port: {mqtt_port},\n\
         mqtt_username: {mqtt_username:?},\n\
         mqtt_password: {mqtt_password:?},\n\
         device_id: {device_id:?},\n\
         read_only: {read_only},\n\
         ntp_server: {ntp_server:?},\n\
         }};\n"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo"));
    fs::write(out.join("config.rs"), source).expect("write the configuration");
}
