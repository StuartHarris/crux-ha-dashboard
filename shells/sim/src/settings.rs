//! Reads the options of the simulator from the command line and the
//! environment.
//!
//! Do not print or log an [`Options`] value: its settings contain the MQTT
//! password.

use std::{
    io,
    path::{Path, PathBuf},
};

use shared::{PowerSource, Settings, capabilities::mqtt::Credentials};

/// The help text.
pub const USAGE: &str = "\
Usage: sim [OPTIONS]

Runs the dashboard core in a window, connected to the MQTT broker.

Modes:
  (none)              Open a window. Click to tap. Keys: S moves the device on
                      or off the stand, W wakes it, D shows the area of each
                      partial refresh, Left and Right turn the page, Q quits.
  --png <FILE>        Draw a page with fixed entity states to a PNG file.
                      Needs no window and no broker.
  --smoke <FILE>      Connect to the broker, wait for the entity states, draw
                      the page to a PNG file and stop. Sends no commands.

Options (each has an environment variable):
  --host <HOST>       The broker host (DASHBOARD_HOST, default 192.168.1.109)
  --port <PORT>       The broker port (DASHBOARD_PORT, default 1883)
  --user <USER>       The MQTT user (DASHBOARD_USER, default dashboard)
  --password-file <FILE>
                      The file with the MQTT password (DASHBOARD_PASSWORD_FILE,
                      default ~/.config/crux-ha-dashboard/mqtt)
  --device <ID>       The device id in MQTT topics (DASHBOARD_DEVICE, default sim)
  --battery           Start off the stand (DASHBOARD_POWER=battery)
  --page <PATH>       The page to show first, for example /room/kitchen
                      (DASHBOARD_PAGE)
  --read-only         Do not publish service calls, so a click changes no
                      device in the house (DASHBOARD_READ_ONLY=1). The
                      --smoke mode is always read-only.
  --help              Show this text
";

/// The default broker host.
pub const DEFAULT_HOST: &str = "192.168.1.109";
/// The default broker port.
pub const DEFAULT_PORT: u16 = 1883;
/// The default MQTT user.
pub const DEFAULT_USER: &str = "dashboard";
/// The default device id.
pub const DEFAULT_DEVICE: &str = "sim";
/// The default password file, relative to the home directory.
pub const DEFAULT_PASSWORD_FILE: &str = ".config/crux-ha-dashboard/mqtt";

/// What the simulator does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Open a window and connect to the broker.
    Window,
    /// Draw a page with fixed entity states to this PNG file.
    Png(PathBuf),
    /// Connect, draw the page with the live states to this PNG file, and stop.
    Smoke(PathBuf),
    /// Show the help text.
    Help,
}

/// The options of the simulator.
pub struct Options {
    /// What the simulator does.
    pub mode: Mode,
    /// The broker and device settings, with the password in the
    /// credentials. The `Png` mode reads no password, so it has no
    /// credentials.
    pub settings: Settings,
    /// The power source at the start.
    pub power: PowerSource,
    /// The path of the page to show first.
    pub path: Option<String>,
    /// If `true`, the shell does not publish service calls, so a click
    /// changes no device in the house.
    pub read_only: bool,
}

/// Reads the options from `args` (without the program name) and from the
/// environment variables that `env` gives.
///
/// `home` is the home directory. `read_password` reads the password file.
/// The `Png` and `Help` modes do not read it.
///
/// # Errors
///
/// Returns a message if an argument is not valid or if the password file
/// cannot be read. The message never contains the password.
pub fn parse(
    args: impl IntoIterator<Item = String>,
    env: impl Fn(&str) -> Option<String>,
    home: Option<&Path>,
    read_password: impl FnOnce(&Path) -> io::Result<String>,
) -> Result<Options, String> {
    let mut mode: Option<Mode> = None;
    let mut host = env("DASHBOARD_HOST").unwrap_or_else(|| DEFAULT_HOST.into());
    let mut port = env("DASHBOARD_PORT");
    let mut user = env("DASHBOARD_USER").unwrap_or_else(|| DEFAULT_USER.into());
    let mut device = env("DASHBOARD_DEVICE").unwrap_or_else(|| DEFAULT_DEVICE.into());
    let mut password_file = env("DASHBOARD_PASSWORD_FILE").map(PathBuf::from);
    let mut power = match env("DASHBOARD_POWER").as_deref() {
        None | Some("stand") => PowerSource::Stand,
        Some("battery") => PowerSource::Battery,
        Some(other) => {
            return Err(format!(
                "DASHBOARD_POWER must be `stand` or `battery`, not `{other}`"
            ));
        }
    };
    let mut path = env("DASHBOARD_PAGE");
    let mut read_only = match env("DASHBOARD_READ_ONLY").as_deref() {
        None | Some("0" | "false") => false,
        Some("1" | "true") => true,
        Some(other) => {
            return Err(format!(
                "DASHBOARD_READ_ONLY must be `1`, `true`, `0` or `false`, not `{other}`"
            ));
        }
    };

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        let new_mode = match arg.as_str() {
            "--png" => Some(Mode::Png(value()?.into())),
            "--smoke" => Some(Mode::Smoke(value()?.into())),
            "--help" | "-h" => Some(Mode::Help),
            _ => None,
        };
        if let Some(new_mode) = new_mode {
            if mode.is_some() {
                return Err(format!(
                    "`{arg}` cannot go with another of --png, --smoke and --help"
                ));
            }
            mode = Some(new_mode);
            continue;
        }
        match arg.as_str() {
            "--host" => host = value()?,
            "--port" => port = Some(value()?),
            "--user" => user = value()?,
            "--password-file" => password_file = Some(value()?.into()),
            "--device" => device = value()?,
            "--page" => path = Some(value()?),
            "--battery" => power = PowerSource::Battery,
            "--read-only" => read_only = true,
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }

    let port = match port {
        None => DEFAULT_PORT,
        Some(port) => port
            .parse()
            .map_err(|_| format!("the port must be a number from 1 to 65535, not `{port}`"))?,
    };

    let mode = mode.unwrap_or(Mode::Window);
    let credentials = match mode {
        Mode::Png(_) | Mode::Help => None,
        Mode::Window | Mode::Smoke(_) => {
            let file = password_file
                .or_else(|| home.map(|home| home.join(DEFAULT_PASSWORD_FILE)))
                .ok_or("the home directory is not known, so give --password-file")?;
            let password = read_password(&file).map_err(|error| {
                format!("cannot read the password file {}: {error}", file.display())
            })?;
            let password = password.trim_end_matches(['\r', '\n']).to_string();
            if password.is_empty() {
                return Err(format!("the password file {} is empty", file.display()));
            }
            Some(Credentials {
                username: user,
                password,
            })
        }
    };

    Ok(Options {
        mode,
        settings: Settings {
            device_id: device,
            host,
            port,
            credentials,
        },
        power,
        path,
        read_only,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        io,
        path::{Path, PathBuf},
    };

    use shared::PowerSource;

    use super::{Mode, Options, parse};

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(String::from).collect()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "the parser takes a reader that can fail"
    )]
    fn secret(_: &Path) -> io::Result<String> {
        Ok("s3cret\n".into())
    }

    fn parse_ok(text: &str, env: impl Fn(&str) -> Option<String>) -> Options {
        let Ok(options) = parse(args(text), env, Some(Path::new("/home/me")), secret) else {
            panic!("the arguments `{text}` are not valid");
        };
        options
    }

    #[test]
    fn the_defaults() {
        let read = Cell::new(None);
        let Ok(options) = parse(Vec::new(), no_env, Some(Path::new("/home/me")), |file| {
            read.set(Some(file.to_path_buf()));
            secret(file)
        }) else {
            panic!("no arguments are valid");
        };
        assert_eq!(options.mode, Mode::Window);
        assert_eq!(options.settings.host, "192.168.1.109");
        assert_eq!(options.settings.port, 1883);
        assert_eq!(options.settings.device_id, "sim");
        assert_eq!(options.power, PowerSource::Stand);
        assert_eq!(options.path, None);
        let credentials = options.settings.credentials.unwrap();
        assert_eq!(credentials.username, "dashboard");
        // The newline at the end of the file is not part of the password.
        assert_eq!(credentials.password, "s3cret");
        assert_eq!(
            read.take(),
            Some(PathBuf::from("/home/me/.config/crux-ha-dashboard/mqtt"))
        );
    }

    #[test]
    fn arguments_override_the_environment() {
        let env = |name: &str| match name {
            "DASHBOARD_HOST" => Some("env-host".into()),
            "DASHBOARD_PORT" => Some("1884".into()),
            "DASHBOARD_DEVICE" => Some("hall".into()),
            "DASHBOARD_POWER" => Some("battery".into()),
            _ => None,
        };
        let options = parse_ok("", env);
        assert_eq!(options.settings.host, "env-host");
        assert_eq!(options.settings.port, 1884);
        assert_eq!(options.settings.device_id, "hall");
        assert_eq!(options.power, PowerSource::Battery);

        let options = parse_ok("--host arg-host --port 1885 --page /room/kitchen", env);
        assert_eq!(options.settings.host, "arg-host");
        assert_eq!(options.settings.port, 1885);
        assert_eq!(options.path.as_deref(), Some("/room/kitchen"));
    }

    #[test]
    fn the_png_mode_reads_no_password() {
        let Ok(options) = parse(args("--png out.png"), no_env, None, |_| {
            panic!("the png mode must not read the password")
        }) else {
            panic!("--png is valid");
        };
        assert_eq!(options.mode, Mode::Png("out.png".into()));
        assert_eq!(options.settings.credentials, None);
    }

    #[test]
    fn the_smoke_mode_reads_the_password_file() {
        let options = parse_ok("--smoke out.png --password-file /tmp/pw", no_env);
        assert_eq!(options.mode, Mode::Smoke("out.png".into()));
        assert!(options.settings.credentials.is_some());
    }

    #[test]
    fn two_modes_are_an_error() {
        for text in [
            "--png a.png --help",
            "--help --png a.png",
            "--png a.png --smoke b.png",
            "--smoke b.png --smoke c.png",
        ] {
            let Err(message) = parse(args(text), no_env, None, secret) else {
                panic!("`{text}` must not be valid");
            };
            assert!(message.contains("cannot go with"), "{message}");
        }
    }

    #[test]
    fn a_value_cannot_be_an_option() {
        for text in ["--png --help", "--host --port 1883", "--page --battery"] {
            let Err(message) = parse(args(text), no_env, None, secret) else {
                panic!("`{text}` must not be valid");
            };
            assert!(message.contains("needs a value"), "{message}");
        }
    }

    #[test]
    fn read_only_from_the_argument_or_the_environment() {
        assert!(!parse_ok("", no_env).read_only);
        assert!(parse_ok("--read-only", no_env).read_only);
        let env = |name: &str| (name == "DASHBOARD_READ_ONLY").then(|| "1".to_string());
        assert!(parse_ok("", env).read_only);
        let env = |name: &str| (name == "DASHBOARD_READ_ONLY").then(|| "maybe".to_string());
        assert!(parse(Vec::new(), env, Some(Path::new("/h")), secret).is_err());
    }

    #[test]
    fn errors_do_not_show_the_password() {
        let errors = [
            parse(args("--port none"), no_env, None, secret),
            parse(args("--host"), no_env, None, secret),
            parse(args("--wat"), no_env, None, secret),
            parse(Vec::new(), no_env, None, secret),
            parse(Vec::new(), no_env, Some(Path::new("/h")), |_| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }),
            parse(Vec::new(), no_env, Some(Path::new("/h")), |_| {
                Ok("\n".into())
            }),
        ];
        for result in errors {
            let Err(message) = result else {
                panic!("the arguments must not be valid");
            };
            assert!(!message.contains("s3cret"), "{message}");
        }
    }
}
