//! The logs feature: the core keeps its own lines of the log, each with the
//! time from the shell.
//!
//! - Each line goes to the shell as a [`Log`](crate::capabilities::log::Log)
//!   request. The shell writes the line and answers with its monotonic time
//!   ([`Uptime`]). The core then keeps the line with that time.
//! - The core keeps the lines in a ring of [`CAPACITY`] lines. If the ring
//!   is full, a new line removes the oldest line, and the core counts the
//!   removed lines ([`State::dropped`]).
//! - The ring keeps at most [`MAX_MESSAGE`] bytes of each message. The shell
//!   still writes the full message.
//! - The ring is in the model, so it keeps its lines through a light sleep,
//!   which keeps the RAM, and while the device is offline.
//! - The ring is only in RAM, so a chip restart loses its lines. This
//!   includes the lines that tell why the chip restarts, for example the
//!   restart of the Wi-Fi policy after 3 joins with no new address. A later
//!   PR can keep the lines through a restart.
//!
//! The core does not send the lines to Home Assistant yet.

use alloc::{collections::VecDeque, string::String};
use core::fmt;

use crate::{
    Cmd, Model,
    capabilities::log::{self, Area, Level, Uptime},
};

/// The number of lines that the ring keeps.
pub const CAPACITY: usize = 64;

/// The maximum length of a message in the ring, in bytes.
pub const MAX_MESSAGE: usize = 200;

/// The mark at the end of a message that the ring cut.
pub const CUT_MARK: &str = "…";

/// The internal events of the logs feature. The core makes them from the
/// answers to its `Log` requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The shell wrote the line, and answered with the time of the line.
    Stamped(Line),
}

impl From<Event> for crate::Event {
    fn from(event: Event) -> Self {
        Self::Logs(event)
    }
}

/// Handles an internal event of the logs feature.
pub fn update(event: Event, model: &mut Model) -> Cmd {
    match event {
        Event::Stamped(line) => model.logs.push(line),
    }
    Cmd::done()
}

/// Writes `message` to the log of the shell at the level `Info`, and keeps
/// the line with its time.
pub fn info(area: Area, message: impl Into<String>) -> Cmd {
    write(Level::Info, area, message.into())
}

/// Writes `message` to the log of the shell at the level `Warn`, and keeps
/// the line with its time.
pub fn warn(area: Area, message: impl Into<String>) -> Cmd {
    write(Level::Warn, area, message.into())
}

fn write(level: Level, area: Area, message: String) -> Cmd {
    let kept = Message::new(&message);
    log::write(level, area, message).then_send(move |at| {
        Event::Stamped(Line {
            at,
            level,
            area,
            message: kept,
        })
        .into()
    })
}

/// A line of the log that the core keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The time on the monotonic clock of the shell when the shell wrote
    /// the line.
    pub at: Uptime,
    /// How important the line is.
    pub level: Level,
    /// The part of the core that wrote the line.
    pub area: Area,
    /// The text of the line, with at most [`MAX_MESSAGE`] bytes.
    pub message: Message,
}

/// The text of a line in the ring, with at most [`MAX_MESSAGE`] bytes.
///
/// If a text is longer, [`Message::new`] keeps its start and adds
/// [`CUT_MARK`]. The cut is on a character boundary, so the message is
/// always valid UTF-8, and the mark is in the [`MAX_MESSAGE`] bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message(String);

impl Message {
    /// Makes a message from `text`, and cuts it if it is longer than
    /// [`MAX_MESSAGE`] bytes.
    #[must_use]
    pub fn new(text: &str) -> Self {
        if text.len() <= MAX_MESSAGE {
            return Self(text.into());
        }
        let end = text.floor_char_boundary(MAX_MESSAGE - CUT_MARK.len());
        let mut message = String::with_capacity(end + CUT_MARK.len());
        message.push_str(&text[..end]);
        message.push_str(CUT_MARK);
        Self(message)
    }

    /// Returns the text of the message.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The lines of the log that the core keeps: a ring of at most
/// [`CAPACITY`] lines, from the oldest, and the number of lines that the
/// ring removed.
#[derive(Debug, Default)]
pub struct State {
    lines: VecDeque<Line>,
    dropped: u32,
}

impl State {
    /// Adds `line` at the end of the ring. If the ring is full, first
    /// removes the oldest line and adds 1 to [`State::dropped`].
    pub fn push(&mut self, line: Line) {
        if self.lines.len() == CAPACITY {
            self.lines.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.lines.push_back(line);
    }

    /// Returns the lines in the ring, from the oldest.
    pub fn lines(&self) -> impl ExactSizeIterator<Item = &Line> + DoubleEndedIterator {
        self.lines.iter()
    }

    /// Returns the number of lines that the ring removed because it was
    /// full. The count stops at `u32::MAX`.
    #[must_use]
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the logs feature: the time of a line, the ring and the cut
    //! of a long message.

    use alloc::{format, string::String, vec::Vec};
    use core::time::Duration;

    use crux_core::App;

    use super::{CAPACITY, CUT_MARK, Line, MAX_MESSAGE, Message, State, info, warn};
    use crate::{
        Dashboard, EffectTestExt, Event, Model, PowerSource,
        capabilities::{
            log::{Area, Level, Log, Uptime},
            power::Wake,
        },
        features::power::IDLE_TIMEOUT,
        tests::{flows::house, start},
    };

    fn line(at: u64, message: &str) -> Line {
        Line {
            at: Uptime(at),
            level: Level::Info,
            area: Area::Wifi,
            message: Message::new(message),
        }
    }

    fn texts(state: &State) -> Vec<&str> {
        state.lines().map(|line| line.message.as_str()).collect()
    }

    /// Returns the time and the text of each line in the ring of `model`
    /// with the area `power`.
    fn power_lines(model: &Model) -> Vec<(u64, String)> {
        model
            .logs
            .lines()
            .filter(|line| line.area == Area::Power)
            .map(|line| (line.at.0, String::from(line.message.as_str())))
            .collect()
    }

    #[test]
    fn a_line_has_the_time_of_the_answer_of_the_shell() {
        let app = Dashboard;
        let mut model = Model::default();

        let mut cmd = info(Area::Wifi, "the scan found no better access point");
        let event = cmd
            .resolve_log(|log| {
                assert_eq!(
                    *log,
                    Log {
                        level: Level::Info,
                        area: Area::Wifi,
                        message: "the scan found no better access point".into(),
                    }
                );
                Uptime(1_000)
            })
            .expect_one_event();
        cmd.expect_done();
        assert!(model.logs.lines().next().is_none());
        app.update(event, &mut model).expect_done();

        // The clock of the shell moves on before the next line.
        let mut cmd = warn(Area::Mqtt, "the broker refused one or more topic filters");
        let event = cmd.resolve_log(|_| Uptime(2_500)).expect_one_event();
        app.update(event, &mut model).expect_done();

        let lines: Vec<_> = model
            .logs
            .lines()
            .map(|line| (line.at, line.level, line.area))
            .collect();
        assert_eq!(
            lines,
            [
                (Uptime(1_000), Level::Info, Area::Wifi),
                (Uptime(2_500), Level::Warn, Area::Mqtt),
            ]
        );
        assert_eq!(
            texts(&model.logs),
            [
                "the scan found no better access point",
                "the broker refused one or more topic filters"
            ]
        );
        assert_eq!(model.logs.dropped(), 0);
    }

    #[test]
    fn a_line_waits_for_the_answer_of_the_shell() {
        let mut cmd = info(Area::Power, "sleep 1 closes the connection");
        let effect = cmd.expect_one_effect();

        // The command waits while the shell keeps the request.
        cmd.expect_no_effect_or_events();
        assert!(!cmd.is_done());

        // The test helpers drop a `Log` request with no answer. The command
        // then ends with no event, so the core keeps no line.
        drop(effect);
        cmd.expect_no_events();
        cmd.expect_done();
    }

    #[test]
    fn a_full_ring_drops_the_oldest_line_and_counts_it() {
        let mut state = State::default();
        for at in 0..CAPACITY as u64 {
            state.push(line(at, &format!("line {at}")));
        }
        assert_eq!(state.lines().len(), CAPACITY);
        assert_eq!(state.dropped(), 0);

        state.push(line(64, "line 64"));
        state.push(line(65, "line 65"));

        assert_eq!(state.lines().len(), CAPACITY);
        assert_eq!(state.dropped(), 2);
        let first = state.lines().next().expect("a line");
        assert_eq!(first.message.as_str(), "line 2");
        let last = state.lines().next_back().expect("a line");
        assert_eq!(last.message.as_str(), "line 65");
    }

    #[test]
    fn a_long_message_is_cut_on_a_character_boundary() {
        // `é` has 2 bytes. It starts at the byte 196, so the cut for the
        // mark (3 bytes) at the byte 197 is in the character.
        let text = format!("{}é{}", "a".repeat(196), "b".repeat(20));
        assert!(!text.is_char_boundary(MAX_MESSAGE - CUT_MARK.len()));

        let message = Message::new(&text);

        assert_eq!(message.as_str(), format!("{}{CUT_MARK}", "a".repeat(196)));
        assert!(message.as_str().len() <= MAX_MESSAGE);
    }

    #[test]
    fn a_message_that_fits_is_not_cut() {
        let text = "a".repeat(MAX_MESSAGE);
        assert_eq!(Message::new(&text).as_str(), text);
        assert_eq!(Message::new("").as_str(), "");
    }

    #[test]
    fn the_shell_writes_the_whole_message_and_the_ring_keeps_the_cut_message() {
        let app = Dashboard;
        let mut model = Model::default();
        let text = "x".repeat(MAX_MESSAGE + 50);

        let mut cmd = warn(Area::Wifi, text.clone());
        let event = cmd
            .resolve_log(|log| {
                assert_eq!(log.message, text);
                Uptime(7)
            })
            .expect_one_event();
        app.update(event, &mut model).expect_done();

        let kept = model.logs.lines().next().expect("a line");
        assert_eq!(kept.message, Message::new(&text));
        assert_eq!(kept.message.as_str().len(), MAX_MESSAGE);
    }

    #[test]
    fn the_ring_keeps_its_lines_through_a_battery_sleep_and_a_wake() {
        let mut shell = house();
        shell.send(start(PowerSource::Stand));
        shell.advance(Duration::from_secs(5));

        // Off the stand, the device sleeps after the idle timeout. Each step
        // of the sleep writes a line with the time of the clock of the shell.
        shell.send(Event::PowerSourceChanged(PowerSource::Battery));
        shell.advance(IDLE_TIMEOUT);
        assert!(shell.sleep.is_some());
        let slept_at = u64::try_from(shell.now().as_millis()).unwrap();
        let first = power_lines(&shell.model);
        assert!(!first.is_empty(), "the steps of the sleep wrote lines");
        assert!(
            first
                .iter()
                .all(|(at, text)| (5_000..=slept_at).contains(at) && text.starts_with("sleep ")),
            "{first:?}"
        );
        assert!(
            first
                .iter()
                .any(|(_, text)| text.ends_with("draws the last image and sleeps"))
        );

        // An hour later, a touch wakes the device. It sleeps again after the
        // idle timeout.
        shell.advance(Duration::from_secs(3_600));
        shell.wake(Wake::Touch(None));
        assert!(shell.sleep.is_none());
        shell.advance(IDLE_TIMEOUT);
        assert!(shell.sleep.is_some());

        // The ring still has the lines of the first sleep, and the lines of
        // the second sleep come after them, with later times.
        let all = power_lines(&shell.model);
        assert_eq!(all[..first.len()], first[..]);
        let second = &all[first.len()..];
        assert!(!second.is_empty(), "the second sleep wrote lines");
        assert!(
            second.iter().all(|(at, _)| *at >= slept_at + 3_600_000),
            "{second:?}"
        );
        let times: Vec<u64> = all.iter().map(|(at, _)| *at).collect();
        assert!(times.is_sorted(), "{times:?}");
        assert_eq!(shell.model.logs.dropped(), 0);
    }
}
