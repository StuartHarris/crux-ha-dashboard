//! The log capability: the decisions of the core, as lines for the log of
//! the shell.
//!
//! The core does not write to a log itself. It sends a [`Log`] request, so a
//! test sees each line as an effect. The shell writes the line as
//! `{area}: {message}` (the [`Display`](core::fmt::Display) of [`Log`]) at
//! its [`Level`]. Then the shell answers at once with its monotonic time
//! ([`Uptime`]). The core keeps the line with that time. Refer to
//! [`crate::features::logs`].
//!
//! The core logs a decision of the Wi-Fi policy, a failure, and the start
//! of each step of a sleep. A start, a join and a connect give no lines.

use alloc::string::String;
use core::fmt;

use crux_core::{Command, Request, command::RequestBuilder, macros::Operation};

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

/// A time on the monotonic clock of the shell, in milliseconds.
///
/// The clock starts when the shell starts, and it never goes back. It must
/// keep counting while the device sleeps, so the times of two lines on each
/// side of a sleep show the length of the sleep. The clock is not a wall
/// clock. The core does not know the date or the time of day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Uptime(pub u64);

/// Writes a line to the log of the shell, and asks for the time of the
/// line.
///
/// The shell writes the line as `{area}: {message}` at `level`. Then it
/// answers with its monotonic time ([`Uptime`]) at the moment that it wrote
/// the line. The shell answers the requests in the order of the lines,
/// because the core keeps the lines in the order of the answers.
#[derive(Operation, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = Uptime)]
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

/// Writes `message` to the log of the shell at `level`. The request ends
/// with the time of the line.
///
/// The app uses [`crate::features::logs::info`] and
/// [`crate::features::logs::warn`], which also keep the line.
#[must_use]
pub fn write<Effect, Event>(
    level: Level,
    area: Area,
    message: String,
) -> RequestBuilder<Effect, Event, impl Future<Output = Uptime>>
where
    Effect: From<Request<Log>> + Send + 'static,
    Event: Send + 'static,
{
    Command::request_from_shell(Log {
        level,
        area,
        message,
    })
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec::Vec};

    use crux_core::{Command, Request};

    use super::{Area, Level, Log, Uptime, write};

    /// The one effect of the log capability, for the tests only.
    struct Effect(Request<Log>);

    impl From<Request<Log>> for Effect {
        fn from(request: Request<Log>) -> Self {
            Self(request)
        }
    }

    #[test]
    fn a_line_is_a_request_that_ends_with_the_time_of_the_shell() {
        let mut command: Command<Effect, Uptime> =
            write(Level::Warn, Area::Mqtt, "refused".into()).then_send(|at| at);

        let mut effects: Vec<Effect> = command.effects().collect();
        assert_eq!(effects.len(), 1, "one effect");
        let mut request = effects.remove(0).0;
        assert_eq!(
            request.operation,
            Log {
                level: Level::Warn,
                area: Area::Mqtt,
                message: "refused".into(),
            }
        );
        assert!(!command.is_done(), "the request waits for its answer");

        request.resolve(Uptime(1_500)).expect("one answer");
        assert_eq!(command.events().collect::<Vec<_>>(), [Uptime(1_500)]);
        assert!(command.is_done());
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
