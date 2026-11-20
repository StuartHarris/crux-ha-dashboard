//! The log capability: the decisions of the core, as lines for the log of
//! the shell.
//!
//! The core does not write to a log itself. It sends a [`Log`]
//! notification, so a test sees each line as an effect. The shell writes
//! the line as `{area}: {message}` (the [`Display`](core::fmt::Display) of
//! [`Log`]) at its [`Level`].
//!
//! The core logs a decision of the Wi-Fi policy, a failure, and the start
//! of each step of a sleep. A start, a join and a connect give no lines.

use alloc::string::String;
use core::fmt;

use crux_core::{Command, Request, macros::Operation};

/// How important a line of the log is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// A decision of the core.
    Info,
    /// A failure, or a decision because of a failure.
    Warn,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Info => "info",
            Self::Warn => "warn",
        })
    }
}

/// The part of the core that writes a line of the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// The Wi-Fi policy. Refer to [`crate::features::wifi`].
    Wifi,
    /// The MQTT session. Refer to [`crate::features::connection`].
    Mqtt,
    /// The power policy and the steps of a sleep. Refer to
    /// [`crate::features::power`].
    Power,
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wifi => "wifi",
            Self::Mqtt => "mqtt",
            Self::Power => "power",
        })
    }
}

/// Writes a line to the log of the shell.
///
/// The shell writes the line as `{area}: {message}` at `level`. A
/// notification has no answer.
#[derive(Operation, Clone, Debug, PartialEq, Eq)]
#[operation(notify)]
pub struct Log {
    /// How important the line is.
    pub level: Level,
    /// The part of the core that writes the line.
    pub area: Area,
    /// The text of the line.
    pub message: String,
}

impl fmt::Display for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.area, self.message)
    }
}

/// Writes `message` to the log of the shell at the level `Info`.
pub fn info<Effect, Event>(area: Area, message: impl Into<String>) -> Command<Effect, Event>
where
    Effect: From<Request<Log>> + Send + 'static,
    Event: Send + 'static,
{
    line(Level::Info, area, message.into())
}

/// Writes `message` to the log of the shell at the level `Warn`.
pub fn warn<Effect, Event>(area: Area, message: impl Into<String>) -> Command<Effect, Event>
where
    Effect: From<Request<Log>> + Send + 'static,
    Event: Send + 'static,
{
    line(Level::Warn, area, message.into())
}

fn line<Effect, Event>(level: Level, area: Area, message: String) -> Command<Effect, Event>
where
    Effect: From<Request<Log>> + Send + 'static,
    Event: Send + 'static,
{
    super::notify(Log {
        level,
        area,
        message,
    })
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec::Vec};

    use crux_core::{Command, Request};

    use super::{Area, Level, Log, info, warn};

    /// The one effect of the log capability, for the tests only.
    struct Effect(Request<Log>);

    impl From<Request<Log>> for Effect {
        fn from(request: Request<Log>) -> Self {
            Self(request)
        }
    }

    /// Returns the operation of the one effect of `command`, and checks that
    /// the command then ends with no event.
    fn logged(mut command: Command<Effect, ()>) -> Log {
        let mut effects: Vec<Effect> = command.effects().collect();
        assert_eq!(effects.len(), 1, "one effect");
        assert!(command.events().next().is_none());
        assert!(command.is_done());
        effects.remove(0).0.operation
    }

    #[test]
    fn info_and_warn_are_notifications_with_their_level() {
        assert_eq!(
            logged(info(Area::Wifi, "stay")),
            Log {
                level: Level::Info,
                area: Area::Wifi,
                message: "stay".into(),
            }
        );
        assert_eq!(
            logged(warn(Area::Mqtt, "refused")),
            Log {
                level: Level::Warn,
                area: Area::Mqtt,
                message: "refused".into(),
            }
        );
    }

    #[test]
    fn a_line_shows_its_area_and_its_message() {
        let log = Log {
            level: Level::Warn,
            area: Area::Wifi,
            message: "the scan failed, so stay".into(),
        };
        assert_eq!(log.to_string(), "wifi: the scan failed, so stay");
        assert_eq!(Area::Mqtt.to_string(), "mqtt");
        assert_eq!(Level::Info.to_string(), "info");
        assert_eq!(Level::Warn.to_string(), "warn");
    }
}
