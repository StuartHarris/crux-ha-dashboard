//! Reads the build-time configuration of the firmware from the text of
//! `cfg.toml`.
//!
//! The file has the Wi-Fi password and the broker password, so it is not in
//! git. `firmware/build.rs` reads it with [`Config::parse`] and writes the
//! values into the firmware as constants. This module uses only `core`,
//! because `build.rs` includes this file as a module of its own.
//!
//! The parser reads a small part of TOML: comments, `[section]` lines, and
//! `key = value` lines. A value is a string, an integer or a boolean. A string
//! in double quotes must not contain a backslash. For a value with a
//! backslash, use a literal string in single quotes.
//!
//! An unknown section or key is an error, so a mistake in a name does not
//! go into the firmware with no sign.

use core::fmt;

/// The sections of the file and their keys.
const KEYS: &[(&str, &[&str])] = &[
    ("wifi", &["ssid", "password", "auth"]),
    ("mqtt", &["host", "port", "username", "password"]),
    ("device", &["id", "read_only", "force_battery"]),
];

/// A key that the firmware used before, and why it is not necessary now.
const REMOVED: &[(&str, &str, &str)] = &[
    (
        "device",
        "utc_offset_minutes",
        "the core calculates UK time, with summer time",
    ),
    ("device", "ntp_server", "the firmware does not set a clock"),
];

/// The authentication of the Wi-Fi network (`[wifi] auth`).
///
/// The Wi-Fi driver (`esp-radio` 1.0.0-beta.1) cannot join a WPA3 network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WifiAuth {
    /// WPA2 Personal (`"wpa2"`, the default).
    Wpa2,
    /// WPA or WPA2 Personal (`"wpa_wpa2"`).
    WpaWpa2,
    /// An open network with no password (`"open"`).
    Open,
}

impl WifiAuth {
    /// Returns the authentication for its name in the file.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "wpa2" => Some(Self::Wpa2),
            "wpa_wpa2" => Some(Self::WpaWpa2),
            "open" => Some(Self::Open),
            _ => None,
        }
    }
}

/// The settings that the firmware gets at build time.
///
/// `parse` checks the Wi-Fi settings, so the firmware can use them with no
/// check at the start. The `Debug` output does not show the passwords.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Config<'a> {
    /// The name of the Wi-Fi network (`[wifi] ssid`), 1 to 32 bytes.
    pub wifi_ssid: &'a str,
    /// The password of the Wi-Fi network (`[wifi] password`). It is empty for
    /// an open network, and 8 to 63 bytes for the other networks.
    pub wifi_password: &'a str,
    /// The authentication of the Wi-Fi network (`[wifi] auth`).
    pub wifi_auth: WifiAuth,
    /// The host name or IP address of the broker (`[mqtt] host`).
    pub mqtt_host: &'a str,
    /// The TCP port of the broker (`[mqtt] port`, 1883 if not set).
    pub mqtt_port: u16,
    /// The user name for the broker (`[mqtt] username`, empty for no
    /// credentials).
    pub mqtt_username: &'a str,
    /// The password for the broker (`[mqtt] password`).
    pub mqtt_password: &'a str,
    /// The id of this dashboard in MQTT topics (`[device] id`).
    pub device_id: &'a str,
    /// If `true`, the core does a dry run and does not publish service calls
    /// (`[device] read_only`, `true` if not set). The firmware gives it to
    /// the core in `Settings::read_only`.
    pub read_only: bool,
    /// If `true`, the firmware tells the core that the device is on battery,
    /// also on USB power (`[device] force_battery`, `false` if not set). The
    /// power poll then sends no `PowerSourceChanged`. Use it to test the
    /// sleep with the serial log on USB.
    pub force_battery: bool,
}

impl fmt::Debug for Config<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hidden = |password: &str| if password.is_empty() { "" } else { "***" };
        f.debug_struct("Config")
            .field("wifi_ssid", &self.wifi_ssid)
            .field("wifi_password", &hidden(self.wifi_password))
            .field("wifi_auth", &self.wifi_auth)
            .field("mqtt_host", &self.mqtt_host)
            .field("mqtt_port", &self.mqtt_port)
            .field("mqtt_username", &self.mqtt_username)
            .field("mqtt_password", &hidden(self.mqtt_password))
            .field("device_id", &self.device_id)
            .field("read_only", &self.read_only)
            .field("force_battery", &self.force_battery)
            .finish()
    }
}

/// A problem in the configuration file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<'a> {
    /// A line is not a comment, a section or a `key = value` pair.
    Syntax {
        /// The number of the line, from 1.
        line: usize,
    },
    /// A value is not a string, an integer or a boolean, or it is not the
    /// type that its key needs.
    Value {
        /// The number of the line, from 1.
        line: usize,
        /// The key, with its section, for example `mqtt.port`.
        key: &'a str,
    },
    /// The file does not have a key that the firmware needs.
    Missing(&'static str),
    /// A `[section]` line names a section that the firmware does not know.
    UnknownSection {
        /// The number of the line, from 1.
        line: usize,
        /// The name of the section.
        section: &'a str,
    },
    /// A key is not a key of its section, or it is not in a section.
    UnknownKey {
        /// The number of the line, from 1.
        line: usize,
        /// The section of the key, or an empty string before the first
        /// section.
        section: &'a str,
        /// The key.
        key: &'a str,
    },
    /// A value has the right type, but the firmware cannot use it.
    Invalid {
        /// The key, with its section, for example `wifi.ssid`.
        key: &'static str,
        /// What the value must be.
        rule: &'static str,
    },
}

impl fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { line } => write!(f, "line {line}: expected `key = value`"),
            Self::Value { line, key } => write!(
                f,
                "line {line}: the value of `{key}` is not valid. Use a string in \
                 quotes, an integer or `true` or `false`. If a string has a \
                 backslash, put it in single quotes"
            ),
            Self::Missing(key) => write!(f, "the key `{key}` is missing"),
            Self::UnknownSection { line, section } => write!(
                f,
                "line {line}: the section `[{section}]` is not known. The sections are \
                 `[wifi]`, `[mqtt]` and `[device]`"
            ),
            Self::UnknownKey { line, section, key } => {
                write!(
                    f,
                    "line {line}: the key `{key}` is not known in `[{section}]`"
                )?;
                match REMOVED
                    .iter()
                    .find(|(old_section, old_key, _)| *old_section == *section && *old_key == *key)
                {
                    Some((_, _, reason)) => write!(f, ". Remove it, because {reason}"),
                    None => write!(f, ". Refer to cfg.toml.example for the keys"),
                }
            }
            Self::Invalid { key, rule } => {
                write!(f, "the value of `{key}` is not valid: it must {rule}")
            }
        }
    }
}

/// A value in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value<'a> {
    Text(&'a str),
    Integer(i64),
    Boolean(bool),
}

impl<'a> Config<'a> {
    /// Reads the configuration from the text of `cfg.toml`.
    ///
    /// # Errors
    ///
    /// Returns an error if a line is not valid, if a value has the wrong
    /// type, or if a key that the firmware needs is missing. The error does
    /// not contain the values, so it is safe to print.
    pub fn parse(text: &'a str) -> Result<Self, Error<'a>> {
        let mut config = Partial::default();
        let mut section = "";
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = strip_comment(raw).trim();
            if content.is_empty() {
                continue;
            }
            if let Some(name) = content
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                section = name.trim();
                if !KEYS.iter().any(|(known, _)| *known == section) {
                    return Err(Error::UnknownSection { line, section });
                }
                continue;
            }
            let (key, value) = content.split_once('=').ok_or(Error::Syntax { line })?;
            let key = key.trim();
            let known = KEYS
                .iter()
                .any(|(known, keys)| *known == section && keys.contains(&key));
            if !known {
                return Err(Error::UnknownKey { line, section, key });
            }
            let value = parse_value(value.trim()).ok_or(Error::Value { line, key })?;
            config
                .set(section, key, value)
                .ok_or(Error::Value { line, key })?;
        }
        config.finish()
    }
}

/// The values that the parser found so far.
#[derive(Default)]
struct Partial<'a> {
    wifi_ssid: Option<&'a str>,
    wifi_password: Option<&'a str>,
    wifi_auth: Option<&'a str>,
    mqtt_host: Option<&'a str>,
    mqtt_port: Option<u16>,
    mqtt_username: Option<&'a str>,
    mqtt_password: Option<&'a str>,
    device_id: Option<&'a str>,
    read_only: Option<bool>,
    force_battery: Option<bool>,
}

impl<'a> Partial<'a> {
    /// Keeps a value. Returns `None` if the value has the wrong type. The
    /// parser checks the key with [`KEYS`] first.
    fn set(&mut self, section: &str, key: &str, value: Value<'a>) -> Option<()> {
        let text = || match value {
            Value::Text(text) => Some(text),
            _ => None,
        };
        let integer = || match value {
            Value::Integer(number) => Some(number),
            _ => None,
        };
        match (section, key) {
            ("wifi", "ssid") => self.wifi_ssid = Some(text()?),
            ("wifi", "password") => self.wifi_password = Some(text()?),
            ("wifi", "auth") => self.wifi_auth = Some(text()?),
            ("mqtt", "host") => self.mqtt_host = Some(text()?),
            ("mqtt", "port") => self.mqtt_port = Some(u16::try_from(integer()?).ok()?),
            ("mqtt", "username") => self.mqtt_username = Some(text()?),
            ("mqtt", "password") => self.mqtt_password = Some(text()?),
            ("device", "id") => self.device_id = Some(text()?),
            ("device", "read_only") => match value {
                Value::Boolean(flag) => self.read_only = Some(flag),
                _ => return None,
            },
            ("device", "force_battery") => match value {
                Value::Boolean(flag) => self.force_battery = Some(flag),
                _ => return None,
            },
            _ => {}
        }
        Some(())
    }

    fn finish(self) -> Result<Config<'a>, Error<'a>> {
        let wifi_ssid = self.wifi_ssid.ok_or(Error::Missing("wifi.ssid"))?;
        if !(1..=32).contains(&wifi_ssid.len()) {
            return Err(Error::Invalid {
                key: "wifi.ssid",
                rule: "have 1 to 32 bytes",
            });
        }
        let wifi_auth = match self.wifi_auth {
            None => WifiAuth::Wpa2,
            Some(name) => WifiAuth::from_name(name).ok_or(Error::Invalid {
                key: "wifi.auth",
                rule: "be \"wpa2\", \"wpa_wpa2\" or \"open\"",
            })?,
        };
        let wifi_password = self.wifi_password.unwrap_or("");
        let password_rule = match wifi_auth {
            WifiAuth::Open if !wifi_password.is_empty() => Some("be empty for an open network"),
            WifiAuth::Wpa2 | WifiAuth::WpaWpa2 if !(8..=63).contains(&wifi_password.len()) => {
                Some("have 8 to 63 bytes")
            }
            _ => None,
        };
        if let Some(rule) = password_rule {
            return Err(Error::Invalid {
                key: "wifi.password",
                rule,
            });
        }
        Ok(Config {
            wifi_ssid,
            wifi_password,
            wifi_auth,
            mqtt_host: self.mqtt_host.ok_or(Error::Missing("mqtt.host"))?,
            mqtt_port: self.mqtt_port.unwrap_or(1883),
            mqtt_username: self.mqtt_username.unwrap_or(""),
            mqtt_password: self.mqtt_password.unwrap_or(""),
            device_id: self.device_id.ok_or(Error::Missing("device.id"))?,
            read_only: self.read_only.unwrap_or(true),
            force_battery: self.force_battery.unwrap_or(false),
        })
    }
}

/// Removes a comment from a line. A `#` in a string is not a comment.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if open == character => quote = None,
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

fn parse_value(text: &str) -> Option<Value<'_>> {
    if let Some(inner) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return (!inner.contains(['"', '\\'])).then_some(Value::Text(inner));
    }
    if let Some(inner) = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return (!inner.contains('\'')).then_some(Value::Text(inner));
    }
    match text {
        "true" => Some(Value::Boolean(true)),
        "false" => Some(Value::Boolean(false)),
        _ => text.parse().ok().map(Value::Integer),
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Error, WifiAuth};

    const EXAMPLE: &str = include_str!("../../cfg.toml.example");

    #[test]
    fn the_example_file_is_valid() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.mqtt_port, 1883);
        assert_eq!(config.device_id, "hall");
        assert!(config.read_only);
        assert!(!config.force_battery);
    }

    #[test]
    fn a_full_file() {
        let text = r#"
            # The network.
            [wifi]
            ssid = "Harris # Home"   # A hash in a string is not a comment.
            password = 'pa\ss"word'
            auth = "wpa_wpa2"

            [mqtt]
            host = "192.168.1.109"
            port = 1884
            username = "dashboard"
            password = "secret"

            [device]
            id = "kitchen"
            read_only = false
            force_battery = true
        "#;
        assert_eq!(
            Config::parse(text),
            Ok(Config {
                wifi_ssid: "Harris # Home",
                wifi_password: r#"pa\ss"word"#,
                wifi_auth: WifiAuth::WpaWpa2,
                mqtt_host: "192.168.1.109",
                mqtt_port: 1884,
                mqtt_username: "dashboard",
                mqtt_password: "secret",
                device_id: "kitchen",
                read_only: false,
                force_battery: true,
            })
        );
    }

    #[test]
    fn optional_keys_have_defaults() {
        let text = "[wifi]\nssid = \"a\"\npassword = \"password\"\n[mqtt]\nhost = \"h\"\n[device]\nid = \"d\"\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(config.mqtt_port, 1883);
        assert_eq!(config.mqtt_username, "");
        assert!(config.read_only);
        assert!(!config.force_battery);
    }

    #[test]
    fn errors_give_the_line_and_the_key() {
        assert_eq!(
            Config::parse("[wifi]\nssid \"a\""),
            Err(Error::Syntax { line: 2 })
        );
        assert_eq!(
            Config::parse("[mqtt]\nport = 70000"),
            Err(Error::Value {
                line: 2,
                key: "port"
            })
        );
        assert_eq!(
            Config::parse("[wifi]\npassword = \"a\\b\""),
            Err(Error::Value {
                line: 2,
                key: "password"
            })
        );
        assert_eq!(
            Config::parse("[wifi]\nssid = 'a'\npassword = '12345678'\n[device]\nid = 'd'"),
            Err(Error::Missing("mqtt.host"))
        );
    }

    #[test]
    fn an_unknown_key_is_an_error_that_names_it() {
        let text = "[wifi]\nssid = 'a'\npassword = '12345678'\ncolour = 'red'\n";
        let error = Config::parse(text).unwrap_err();
        assert_eq!(
            error,
            Error::UnknownKey {
                line: 4,
                section: "wifi",
                key: "colour"
            }
        );
        let message = alloc::format!("{error}");
        assert!(
            message.contains("colour") && message.contains("[wifi]"),
            "{message}"
        );

        // A key before the first section.
        assert!(matches!(
            Config::parse("ssid = 'a'"),
            Err(Error::UnknownKey {
                line: 1,
                section: "",
                key: "ssid"
            })
        ));
        // A known key in the wrong section.
        assert!(matches!(
            Config::parse("[device]\nhost = 'h'"),
            Err(Error::UnknownKey { key: "host", .. })
        ));
    }

    #[test]
    fn an_unknown_section_is_an_error_that_names_it() {
        let error = Config::parse("[wifi]\nssid = 'a'\n[mqqt]\nhost = 'h'").unwrap_err();
        assert_eq!(
            error,
            Error::UnknownSection {
                line: 3,
                section: "mqqt"
            }
        );
        assert!(alloc::format!("{error}").contains("[mqqt]"));
    }

    #[test]
    fn force_battery_must_be_a_boolean() {
        let text = "[device]\nid = 'd'\nforce_battery = 'yes'\n";
        assert_eq!(
            Config::parse(text),
            Err(Error::Value {
                line: 3,
                key: "force_battery"
            })
        );
    }

    #[test]
    fn the_old_utc_offset_key_is_an_error_with_a_hint() {
        let text = "[device]\nid = 'd'\nutc_offset_minutes = 60\n";
        let error = Config::parse(text).unwrap_err();
        let message = alloc::format!("{error}");
        assert!(message.contains("utc_offset_minutes"), "{message}");
        assert!(message.contains("UK time"), "{message}");
        assert!(message.contains("Remove it"), "{message}");
    }

    #[test]
    fn an_error_does_not_show_the_value() {
        let error = Config::parse("[wifi]\npassword = secret-value").unwrap_err();
        let text = alloc::format!("{error}");
        assert!(!text.contains("secret-value"));
        assert!(text.contains("password"));
    }

    /// A file with valid settings and this `[wifi]` section.
    fn with_wifi(wifi: &str) -> alloc::string::String {
        alloc::format!("[wifi]\n{wifi}\n[mqtt]\nhost = 'h'\n[device]\nid = 'd'\n")
    }

    fn is_invalid(result: Result<Config<'_>, Error<'_>>, key: &str) -> bool {
        matches!(result, Err(Error::Invalid { key: bad, .. }) if bad == key)
    }

    #[test]
    fn the_ssid_has_1_to_32_bytes() {
        let empty = with_wifi("ssid = ''\npassword = 'password'");
        assert!(is_invalid(Config::parse(&empty), "wifi.ssid"));
        let long = with_wifi(&alloc::format!(
            "ssid = '{}'\npassword = 'password'",
            "x".repeat(33)
        ));
        assert!(is_invalid(Config::parse(&long), "wifi.ssid"));
        let longest = with_wifi(&alloc::format!(
            "ssid = '{}'\npassword = 'password'",
            "x".repeat(32)
        ));
        assert!(Config::parse(&longest).is_ok());
    }

    #[test]
    fn a_wpa_password_has_8_to_63_bytes() {
        let short = with_wifi("ssid = 'a'\npassword = 'short'");
        assert!(is_invalid(Config::parse(&short), "wifi.password"));
        let missing = with_wifi("ssid = 'a'");
        assert!(is_invalid(Config::parse(&missing), "wifi.password"));
        let long = with_wifi(&alloc::format!(
            "ssid = 'a'\npassword = '{}'",
            "x".repeat(64)
        ));
        assert!(is_invalid(Config::parse(&long), "wifi.password"));
        let good = with_wifi("ssid = 'a'\npassword = '12345678'");
        assert_eq!(Config::parse(&good).unwrap().wifi_auth, WifiAuth::Wpa2);
    }

    #[test]
    fn an_open_network_has_no_password() {
        let open = with_wifi("ssid = 'cafe'\nauth = 'open'");
        let config = Config::parse(&open).unwrap();
        assert_eq!(config.wifi_auth, WifiAuth::Open);
        assert_eq!(config.wifi_password, "");
        let with_password = with_wifi("ssid = 'cafe'\nauth = 'open'\npassword = '12345678'");
        assert!(is_invalid(Config::parse(&with_password), "wifi.password"));
    }

    #[test]
    fn an_unknown_auth_is_an_error_that_names_the_key() {
        let text = with_wifi("ssid = 'a'\npassword = '12345678'\nauth = 'wpa3'");
        let result = Config::parse(&text);
        assert!(is_invalid(result, "wifi.auth"));
        let message = alloc::format!("{}", result.unwrap_err());
        assert!(message.contains("wifi.auth"), "{message}");
    }

    #[test]
    fn debug_does_not_show_the_passwords() {
        let config = Config::parse(EXAMPLE).unwrap();
        let text = alloc::format!("{config:?}");
        assert!(!text.contains(config.wifi_password), "{text}");
        assert!(!text.contains(config.mqtt_password), "{text}");
        assert!(text.contains(config.wifi_ssid), "{text}");
    }
}
