//! The forward of the warn and error lines of the firmware to the core.
//!
//! The logger of the firmware prints each line to the serial port. It also
//! gives each warn or error line to the core, as
//! [`Event::ShellLogged`], so that the line goes to Home Assistant with the
//! lines of the core. This module has the parts of the logger that do not
//! touch the hardware:
//!
//! - [`Filter`] reads `ESP_LOG` as the logger of esp-println does, so the
//!   serial log prints the same lines.
//! - [`forwarded`] decides which lines go to the core.
//! - [`route`] decides for each line if it prints, if it goes to the core,
//!   or both. The two are separate: a warn line that `ESP_LOG` does not
//!   print still goes to the core. [`max_level`] is the level for the `log`
//!   facade that lets both through.
//! - [`Line::new`] writes a line into fixed buffers, and [`split`] finds
//!   its source.
//! - [`Pending`] keeps at most [`PENDING`] lines that the core task did
//!   not take, and counts the lines that it lost.
//!
//! The logger can run in any task, also while the heap is not ready. So
//! [`Line`] and [`Pending`] do not allocate. The core task takes each line
//! and makes the event, which allocates, in its own task.

use core::fmt::{self, Write};

use log::LevelFilter;
use shared::{
    Event,
    capabilities::log::{Level, Uptime},
    features::logs::{MAX_MESSAGE, MAX_SOURCE},
};

/// The log target of the lines of the core. The core task writes each line
/// of a `Log` request with this target. The logger does not forward these
/// lines, because the core keeps them already.
pub const CORE_TARGET: &str = "core";

/// The largest number of lines that wait for the core task. If more lines
/// come, the logger drops the new lines and counts them.
pub const PENDING: usize = 16;

/// The longest word at the start of a line that can be its source, in
/// bytes. Refer to [`split`].
pub const MAX_WORD: usize = 16;

/// The crates of the firmware: the `dashboard` binary and the library.
///
/// The `bringup` binary has no core. Only a line of these crates can name
/// its source with a word at its start. Refer to [`split`].
pub const OWN_CRATES: [&str; 2] = ["dashboard", "dashboard_firmware"];

/// The bytes that a [`Line`] keeps of a message. A message that is longer
/// than [`MAX_MESSAGE`] keeps at least one more character, so the core sees
/// that it is too long, and cuts it with its mark.
const MESSAGE: usize = MAX_MESSAGE + 4;

/// The bytes that a [`Line`] keeps of a source, with one more character as
/// for [`MESSAGE`].
const SOURCE: usize = MAX_SOURCE + 4;

/// The bytes of a line before the split: a source word, `": "` and a
/// message.
const TEXT: usize = MAX_WORD + 2 + MESSAGE;

/// Returns the level for the core if the line goes to the core, or `None`.
///
/// A line goes to the core if its level is `Warn` or `Error`, and its
/// target is not [`CORE_TARGET`]. The logger writes no lines of its own, so
/// no other target needs a check.
#[must_use]
pub fn forwarded(level: log::Level, target: &str) -> Option<Level> {
    if target == CORE_TARGET {
        return None;
    }
    match level {
        log::Level::Error => Some(Level::Error),
        log::Level::Warn => Some(Level::Warn),
        log::Level::Info | log::Level::Debug | log::Level::Trace => None,
    }
}

/// What the logger does with a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// The line prints on the serial port.
    pub print: bool,
    /// The line goes to the core at this level.
    pub forward: Option<Level>,
}

/// Returns what the logger does with a line with `level` and `target`.
///
/// - The line prints if `ESP_LOG` prints it ([`Filter::prints`]).
/// - If `forwarding` is `true`, the line goes to the core if
///   [`forwarded`] accepts it, also if it does not print.
#[must_use]
pub fn route(filter: &Filter<'_>, forwarding: bool, level: log::Level, target: &str) -> Route {
    Route {
        print: filter.prints(level, target),
        forward: if forwarding {
            forwarded(level, target)
        } else {
            None
        },
    }
}

/// Returns the maximum level for the `log` facade. It is the highest level
/// of `ESP_LOG`, and at least `Warn` if `forwarding` is `true`, so that
/// each line for the core comes to the logger.
#[must_use]
pub fn max_level(filter: &Filter<'_>, forwarding: bool) -> LevelFilter {
    let printed = filter.max_level();
    if forwarding {
        printed.max(LevelFilter::Warn)
    } else {
        printed
    }
}

/// Returns the source and the message of the line `text` with the log
/// target `target`.
///
/// - If the crate of the target (the part before the first `::`) is one of
///   [`OWN_CRATES`], and `text` starts with a word and `": "`, the word is
///   the source, and the text after `": "` is the message. A word has 1 to
///   [`MAX_WORD`] bytes. It starts with a lowercase ASCII letter, and has
///   only lowercase ASCII letters, digits and `_`. For example,
///   `stream: 10 failed` gives `stream` and `10 failed`.
/// - If not, the source is the crate of the target, and the message is all
///   of `text`. For example, a line of `esp_radio::wifi::os_adapter` gives
///   `esp_radio`. The crate name tells a line of the Wi-Fi driver from a
///   `wifi` line of the core.
#[must_use]
pub fn split<'a>(text: &'a str, target: &'a str) -> (&'a str, &'a str) {
    let krate = target.split("::").next().unwrap_or(target);
    if OWN_CRATES.contains(&krate)
        && let Some((word, message)) = text.split_once(": ")
        && is_word(word)
    {
        return (word, message);
    }
    (krate, text)
}

/// Returns `true` if `word` can be the source at the start of a line.
fn is_word(word: &str) -> bool {
    let mut bytes = word.bytes();
    word.len() <= MAX_WORD
        && bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// A text of at most `N` bytes in a fixed buffer.
///
/// As a [`fmt::Write`], it keeps the start of the text that fits, cut on a
/// character boundary, and drops the rest. It never fails, so a long
/// argument does not stop the format of the line.
#[derive(Clone, Copy)]
pub struct Text<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    /// An empty text.
    pub const EMPTY: Self = Self {
        bytes: [0; N],
        len: 0,
    };

    /// Makes a text from the start of `text` that fits.
    #[must_use]
    pub fn of(text: &str) -> Self {
        let mut kept = Self::EMPTY;
        kept.push(text);
        kept
    }

    /// Returns the text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // `push` copies only whole characters of a `str`, so the bytes are
        // always valid UTF-8.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }

    /// Adds the start of `text` that fits.
    fn push(&mut self, text: &str) {
        let room = N - self.len;
        let end = text.floor_char_boundary(room);
        self.bytes[self.len..self.len + end].copy_from_slice(&text.as_bytes()[..end]);
        self.len += end;
    }
}

impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push(text);
        Ok(())
    }
}

impl<const N: usize> fmt::Debug for Text<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// A line of the firmware for the core, in fixed buffers.
#[derive(Clone, Copy, Debug)]
pub struct Line {
    /// The time of the line on the monotonic clock of the device.
    pub at: Uptime,
    /// How important the line is.
    pub level: Level,
    /// The source of the line. Refer to [`split`].
    pub source: Text<SOURCE>,
    /// The message of the line.
    pub message: Text<MESSAGE>,
}

impl Line {
    /// A line with no text, to fill the places of [`Pending`].
    const EMPTY: Self = Self {
        at: Uptime(0),
        level: Level::Warn,
        source: Text::EMPTY,
        message: Text::EMPTY,
    };

    /// Writes the line `args` with the log target `target`, and splits it
    /// into its source and its message (refer to [`split`]).
    ///
    /// The line keeps one more character of a source or a message that is
    /// too long, so the core cuts it with its mark.
    #[must_use]
    pub fn new(at: Uptime, level: Level, target: &str, args: fmt::Arguments<'_>) -> Self {
        let mut text = Text::<TEXT>::EMPTY;
        // `Text` never fails. An error from a `Display` of an argument ends
        // the line early, and the line keeps what it has.
        let _ = text.write_fmt(args);
        let (source, message) = split(text.as_str(), target);
        Self {
            at,
            level,
            source: Text::of(source),
            message: Text::of(message),
        }
    }

    /// Returns the event for the core, with `dropped` lines that the
    /// firmware lost before this line.
    #[must_use]
    pub fn event(&self, dropped: u32) -> Event {
        Event::ShellLogged {
            at: self.at,
            level: self.level,
            source: self.source.as_str().into(),
            message: self.message.as_str().into(),
            dropped,
        }
    }
}

/// The lines that wait for the core task: at most [`PENDING`] lines, from
/// the oldest, and the number of lines that the logger lost.
///
/// If the queue is full, the logger drops the new line, and keeps the
/// older lines. The first lines of a burst often tell its cause.
///
/// The lost lines came after the lines in the queue. So [`Pending::pop`]
/// gives the count with the last line in the queue, and the core adds the
/// count after that line.
pub struct Pending {
    lines: [Line; PENDING],
    /// The index of the oldest line.
    head: usize,
    /// The number of lines that wait.
    len: usize,
    /// The number of lines that the logger lost since the last count that
    /// [`Pending::pop`] gave.
    lost: u32,
}

impl Default for Pending {
    fn default() -> Self {
        Self::new()
    }
}

impl Pending {
    /// Makes an empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lines: [Line::EMPTY; PENDING],
            head: 0,
            len: 0,
            lost: 0,
        }
    }

    /// Adds `line` at the end of the queue. If the queue is full, drops
    /// `line` and counts it.
    pub const fn push(&mut self, line: Line) {
        if self.len == PENDING {
            self.lose(1);
            return;
        }
        self.lines[(self.head + self.len) % PENDING] = line;
        self.len += 1;
    }

    /// Counts `count` lines that the logger lost. The count stops at
    /// `u32::MAX`.
    pub const fn lose(&mut self, count: u32) {
        self.lost = self.lost.saturating_add(count);
    }

    /// Removes the oldest line, or returns `None` if no line waits.
    ///
    /// With the last line in the queue, it also returns the number of lines
    /// that the logger lost. With each other line, the count is 0. A count
    /// with no line stays for the next line.
    pub const fn pop(&mut self) -> Option<(Line, u32)> {
        if self.len == 0 {
            return None;
        }
        let line = self.lines[self.head];
        self.head = (self.head + 1) % PENDING;
        self.len -= 1;
        let lost = if self.len == 0 {
            let lost = self.lost;
            self.lost = 0;
            lost
        } else {
            0
        };
        Some((line, lost))
    }

    /// Returns the number of lines that wait.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no line waits.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The levels of the log for each target, from the `ESP_LOG` text at build
/// time.
///
/// The rules are the rules of the logger of esp-println 0.18, which reads
/// the same text:
///
/// - The text is a list of directives with `,` between them. The text
///   after a `/` does not count.
/// - `<level>` is the level of all targets. `<target>=<level>` is the level
///   of each target that starts with `<target>`. A `<target>` alone, or
///   `<target>=`, has the level `trace`.
/// - The directive with the longest `<target>` that matches is the one that
///   counts. If no `<target>` matches, the level of all targets counts. If
///   the text has no such level, the line does not print.
/// - With no `ESP_LOG`, the maximum level is `off`, so no line prints.
///
/// The build of esp-println stops on a directive that is not valid. This
/// type ignores such a directive.
#[derive(Clone, Copy, Debug)]
pub struct Filter<'a> {
    spec: Option<&'a str>,
}

impl<'a> Filter<'a> {
    /// Makes the filter for the `ESP_LOG` text `spec`.
    #[must_use]
    pub const fn new(spec: Option<&'a str>) -> Self {
        Self { spec }
    }

    /// Returns the highest level of a directive, for
    /// [`log::set_max_level`].
    #[must_use]
    pub fn max_level(&self) -> LevelFilter {
        self.spec.map_or(LevelFilter::Off, |spec| {
            directives(spec)
                .map(|(_, level)| level)
                .max()
                .unwrap_or(LevelFilter::Off)
        })
    }

    /// Returns `true` if a line with `level` and `target` prints: its level
    /// is not above [`Filter::max_level`], and [`Filter::enabled`] lets it
    /// through.
    #[must_use]
    pub fn prints(&self, level: log::Level, target: &str) -> bool {
        level <= self.max_level() && self.enabled(level, target)
    }

    /// Returns `true` if the directives let a line with `level` and `target`
    /// through. With no `ESP_LOG`, this is `true` for each line, as in
    /// esp-println, which relies on the maximum level `off`. Use
    /// [`Filter::prints`] to know if a line prints.
    #[must_use]
    pub fn enabled(&self, level: log::Level, target: &str) -> bool {
        let Some(spec) = self.spec else {
            return true;
        };
        let mut best: Option<(usize, LevelFilter)> = None;
        let mut global = None;
        for (name, filter) in directives(spec) {
            match name {
                Some(name) if target.starts_with(name) => {
                    if best.is_none_or(|(len, _)| name.len() > len) {
                        best = Some((name.len(), filter));
                    }
                }
                Some(_) => {}
                None => global = global.or(Some(filter)),
            }
        }
        best.map(|(_, filter)| filter)
            .or(global)
            .is_some_and(|filter| level <= filter)
    }
}

/// Returns the valid directives of `spec`: the target, if any, and the
/// level of each one.
fn directives(spec: &str) -> impl Iterator<Item = (Option<&str>, LevelFilter)> {
    let mods = spec.split('/').next().unwrap_or_default();
    mods.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let mut parts = part.split('=');
            match (parts.next(), parts.next().map(str::trim), parts.next()) {
                (Some(part), None, None) => Some(
                    part.parse()
                        .map_or_else(|_| (Some(part), LevelFilter::max()), |level| (None, level)),
                ),
                (Some(name), Some(""), None) => Some((Some(name), LevelFilter::max())),
                (Some(name), Some(level), None) => {
                    level.parse().ok().map(|level| (Some(name), level))
                }
                _ => None,
            }
        })
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::String};

    use log::LevelFilter;
    use shared::{
        Event,
        capabilities::log::{Level, Uptime},
        features::logs::{MAX_MESSAGE, MAX_SOURCE},
    };

    use super::{
        CORE_TARGET, Filter, Line, PENDING, Pending, Route, Text, forwarded, max_level, route,
        split,
    };

    fn line(message: &str) -> Line {
        Line::new(
            Uptime(1_000),
            Level::Warn,
            "dashboard::stream",
            format_args!("{message}"),
        )
    }

    // The filter of levels.

    #[test]
    fn warn_and_error_lines_go_to_the_core() {
        assert_eq!(
            forwarded(log::Level::Warn, "dashboard::stream"),
            Some(Level::Warn)
        );
        assert_eq!(
            forwarded(log::Level::Error, "esp_radio::wifi"),
            Some(Level::Error)
        );
    }

    #[test]
    fn info_debug_and_trace_lines_do_not_go_to_the_core() {
        for level in [log::Level::Info, log::Level::Debug, log::Level::Trace] {
            assert_eq!(forwarded(level, "dashboard::stream"), None, "{level}");
        }
    }

    #[test]
    fn a_line_of_the_core_does_not_go_back_to_the_core() {
        assert_eq!(forwarded(log::Level::Warn, CORE_TARGET), None);
        assert_eq!(forwarded(log::Level::Error, CORE_TARGET), None);
        // Only the exact target is the core.
        assert_eq!(
            forwarded(log::Level::Warn, "dashboard::core_task"),
            Some(Level::Warn)
        );
    }

    // The source and the message.

    #[test]
    fn a_word_and_a_colon_at_the_start_are_the_source() {
        assert_eq!(
            split(
                "stream: 10 failed: TCP connect failed: ConnectionReset (by the host)",
                "dashboard::stream"
            ),
            (
                "stream",
                "10 failed: TCP connect failed: ConnectionReset (by the host)"
            )
        );
        assert_eq!(
            split("bq_27220: no answer", "dashboard"),
            ("bq_27220", "no answer")
        );
        assert_eq!(
            split("light: cannot start", "dashboard_firmware::light"),
            ("light", "cannot start")
        );
    }

    #[test]
    fn a_line_of_another_crate_has_the_crate_as_its_source_and_its_whole_text() {
        assert_eq!(
            split("wifi: not started", "esp_radio::wifi"),
            ("esp_radio", "wifi: not started")
        );
        assert_eq!(split("time: 12", "esp_hal"), ("esp_hal", "time: 12"));
        // A crate whose name only starts with `dashboard` is another crate.
        assert_eq!(
            split("stream: x", "dashboards::a"),
            ("dashboards", "stream: x")
        );
    }

    #[test]
    fn with_no_word_at_the_start_the_crate_of_the_target_is_the_source() {
        assert_eq!(
            split(
                "Lost event - consider increasing the capacity",
                "esp_radio::wifi::os_adapter"
            ),
            ("esp_radio", "Lost event - consider increasing the capacity")
        );
        // Words with spaces, capitals or no space after the colon are not a
        // source.
        assert_eq!(
            split("touch read failed: Nack", "dashboard::display"),
            ("dashboard", "touch read failed: Nack")
        );
        assert_eq!(
            split("Invalid core number: 3", "dashboard"),
            ("dashboard", "Invalid core number: 3")
        );
        assert_eq!(split("time:12", "dashboard"), ("dashboard", "time:12"));
        assert_eq!(split("2nd: x", "dashboard"), ("dashboard", "2nd: x"));
        let long = "a".repeat(super::MAX_WORD + 1);
        let text = format!("{long}: x");
        assert_eq!(split(&text, "dashboard"), ("dashboard", text.as_str()));
    }

    #[test]
    fn a_line_has_its_source_message_level_and_time_in_the_event() {
        let line = line("stream: 10 failed: timeout");

        assert_eq!(
            line.event(3),
            Event::ShellLogged {
                at: Uptime(1_000),
                level: Level::Warn,
                source: "stream".into(),
                message: "10 failed: timeout".into(),
                dropped: 3,
            }
        );
    }

    #[test]
    fn a_long_message_keeps_one_more_character_so_the_core_cuts_it() {
        // Two bytes for each character, so the cut is on a boundary.
        let text = "é".repeat(MAX_MESSAGE);
        let line = line(&format!("stream: {text}"));

        let message = line.message.as_str();
        assert!(message.len() > MAX_MESSAGE, "{}", message.len());
        assert!(message.len() <= MAX_MESSAGE + 4, "{}", message.len());
        assert!(text.starts_with(message));
    }

    #[test]
    fn a_long_line_with_no_word_keeps_one_more_character_too() {
        let text = "x".repeat(MAX_MESSAGE * 2);
        let line = line(&text);

        assert_eq!(line.source.as_str(), "dashboard");
        assert_eq!(line.message.as_str().len(), MAX_MESSAGE + 4);
    }

    #[test]
    fn a_long_target_keeps_one_more_character_of_its_crate() {
        let target = "c".repeat(MAX_SOURCE * 2);
        let line = Line::new(Uptime(0), Level::Error, &target, format_args!("Oops"));

        assert_eq!(line.source.as_str().len(), MAX_SOURCE + 4);
        assert_eq!(line.message.as_str(), "Oops");
    }

    #[test]
    fn a_text_keeps_only_whole_characters() {
        let mut text = Text::<5>::EMPTY;
        let _ = core::fmt::Write::write_str(&mut text, "ab");
        let _ = core::fmt::Write::write_str(&mut text, "€x");
        assert_eq!(text.as_str(), "ab€");
        let _ = core::fmt::Write::write_str(&mut text, "y");
        assert_eq!(text.as_str(), "ab€");
    }

    // The queue.

    #[test]
    fn the_core_task_takes_the_lines_in_order() {
        let mut pending = Pending::new();
        pending.push(line("one"));
        pending.push(line("two"));

        let texts: alloc::vec::Vec<(String, u32)> = core::iter::from_fn(|| pending.pop())
            .map(|(line, lost)| (line.message.as_str().into(), lost))
            .collect();
        assert_eq!(texts, [("one".into(), 0), ("two".into(), 0)]);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_full_queue_drops_the_new_line_and_the_last_line_tells_the_count() {
        let mut pending = Pending::new();
        for index in 0..PENDING + 3 {
            pending.push(line(&format!("line {index}")));
        }
        assert_eq!(pending.len(), PENDING);

        // The 3 dropped lines came after the lines in the queue, so only the
        // last line in the queue carries their count.
        let taken: alloc::vec::Vec<(String, u32)> = core::iter::from_fn(|| pending.pop())
            .map(|(line, lost)| (line.message.as_str().into(), lost))
            .collect();
        let expected: alloc::vec::Vec<(String, u32)> = (0..PENDING)
            .map(|index| {
                let lost = if index == PENDING - 1 { 3 } else { 0 };
                (format!("line {index}"), lost)
            })
            .collect();
        assert_eq!(taken, expected);

        // The queue wraps around, and the count starts again at 0.
        pending.push(line("after"));
        assert_eq!(
            pending
                .pop()
                .map(|(line, lost)| (line.message, lost))
                .map(|(message, lost)| (String::from(message.as_str()), lost)),
            Some(("after".into(), 0))
        );
        assert_eq!(pending.pop().map(|(_, lost)| lost), None);
    }

    #[test]
    fn a_line_lost_while_lines_wait_goes_with_the_last_of_them() {
        let mut pending = Pending::new();
        pending.push(line("one"));
        pending.push(line("two"));
        // The logger was busy, so it lost a line after `two`.
        pending.lose(1);

        assert_eq!(pending.pop().map(|(_, lost)| lost), Some(0));
        pending.push(line("three"));
        assert_eq!(pending.pop().map(|(_, lost)| lost), Some(0));
        assert_eq!(pending.pop().map(|(_, lost)| lost), Some(1));
    }

    #[test]
    fn a_count_with_no_line_waits_for_the_next_line() {
        let mut pending = Pending::new();
        pending.lose(2);
        assert_eq!(pending.pop().map(|(_, lost)| lost), None);

        pending.push(line("next"));
        assert_eq!(pending.pop().map(|(_, lost)| lost), Some(2));
    }

    // The route of a line.

    #[test]
    fn a_line_prints_as_esp_log_says_and_goes_to_the_core_if_forwarded() {
        let filter = Filter::new(Some("error,dashboard=info"));
        let both = |level, target| route(&filter, true, level, target);

        // Both: an error line of esp-radio.
        assert_eq!(
            both(log::Level::Error, "esp_radio::wifi"),
            Route {
                print: true,
                forward: Some(Level::Error)
            }
        );
        // Forward only: `ESP_LOG` does not print a warn line of esp-radio.
        assert_eq!(
            both(log::Level::Warn, "esp_radio::wifi"),
            Route {
                print: false,
                forward: Some(Level::Warn)
            }
        );
        // Print only: an info line, and a warn line of the core.
        assert_eq!(
            both(log::Level::Info, "dashboard::stream"),
            Route {
                print: true,
                forward: None
            }
        );
        assert_eq!(
            both(log::Level::Warn, CORE_TARGET),
            Route {
                print: false,
                forward: None
            }
        );
        // Neither: a debug line.
        assert_eq!(
            both(log::Level::Debug, "dashboard::stream"),
            Route {
                print: false,
                forward: None
            }
        );
    }

    #[test]
    fn before_the_forward_starts_a_line_only_prints() {
        let filter = Filter::new(Some("info"));

        assert_eq!(
            route(&filter, false, log::Level::Error, "dashboard"),
            Route {
                print: true,
                forward: None
            }
        );
        assert_eq!(max_level(&filter, false), LevelFilter::Info);
    }

    #[test]
    fn with_the_forward_the_maximum_level_is_at_least_warn() {
        let quiet = Filter::new(Some("error"));
        assert_eq!(max_level(&quiet, false), LevelFilter::Error);
        assert_eq!(max_level(&quiet, true), LevelFilter::Warn);
        let loud = Filter::new(Some("debug"));
        assert_eq!(max_level(&loud, true), LevelFilter::Debug);

        // With no `ESP_LOG`, a warn line goes to the core but does not
        // print.
        let none = Filter::new(None);
        assert_eq!(max_level(&none, true), LevelFilter::Warn);
        assert_eq!(
            route(&none, true, log::Level::Warn, "dashboard"),
            Route {
                print: false,
                forward: Some(Level::Warn)
            }
        );
    }

    // The filter of `ESP_LOG`.

    #[test]
    fn a_global_level_prints_that_level_and_higher() {
        let filter = Filter::new(Some("info"));

        assert_eq!(filter.max_level(), LevelFilter::Info);
        assert!(filter.enabled(log::Level::Warn, "esp_radio::wifi"));
        assert!(filter.enabled(log::Level::Info, "dashboard"));
        assert!(!filter.enabled(log::Level::Debug, "dashboard"));
    }

    #[test]
    fn the_longest_target_that_matches_counts() {
        let filter = Filter::new(Some(
            "warn, esp_radio=error, esp_radio::wifi=debug, dashboard",
        ));

        assert_eq!(filter.max_level(), LevelFilter::Trace);
        assert!(filter.enabled(log::Level::Debug, "esp_radio::wifi::os_adapter"));
        assert!(!filter.enabled(log::Level::Warn, "esp_radio::common_adapter"));
        assert!(filter.enabled(log::Level::Error, "esp_radio::common_adapter"));
        // A target alone has the level `trace`.
        assert!(filter.enabled(log::Level::Trace, "dashboard::stream"));
        // Other targets have the global level.
        assert!(filter.enabled(log::Level::Warn, "embassy_net"));
        assert!(!filter.enabled(log::Level::Info, "embassy_net"));
    }

    #[test]
    fn with_no_global_level_a_target_that_does_not_match_does_not_print() {
        let filter = Filter::new(Some("dashboard=info/some regex"));

        assert_eq!(filter.max_level(), LevelFilter::Info);
        assert!(filter.enabled(log::Level::Info, "dashboard"));
        assert!(!filter.enabled(log::Level::Error, "esp_hal"));
    }

    #[test]
    fn with_no_esp_log_no_line_prints() {
        let filter = Filter::new(None);

        assert_eq!(filter.max_level(), LevelFilter::Off);
        // The `log` facade stops each line at the maximum level, so the
        // filter itself lets each line through, as esp-println does.
        assert!(filter.enabled(log::Level::Error, "dashboard"));
    }

    #[test]
    fn a_directive_that_is_not_valid_does_not_count() {
        let filter = Filter::new(Some("info,dashboard=loud,a=b=c"));

        assert_eq!(filter.max_level(), LevelFilter::Info);
        assert!(!filter.enabled(log::Level::Debug, "dashboard"));
    }
}
