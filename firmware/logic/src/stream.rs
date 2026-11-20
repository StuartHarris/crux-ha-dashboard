//! The bookkeeping of the stream task: which stream is open, and the end of
//! each stream. This module also counts the time of each update of the core.
//!
//! The device has one stream to the broker at a time. Each stream has the
//! connection id of the core. Each stream ends with exactly one `Closed`
//! item:
//!
//! - when the task cannot open the stream,
//! - when the broker or the network closes the stream,
//! - when the core closes the stream, or
//! - when a new stream replaces it.
//!
//! [`Streams`] makes the signals for the shell, and it makes sure that no
//! item follows the `Closed` item of a stream.
//!
//! [`supersede`] removes the queued jobs that a later job makes useless.
//!
//! [`UpdateStats`] counts the updates of the core and their time.

use alloc::{string::String, vec::Vec};
use core::time::Duration;

use shared::capabilities::stream::{StreamItem, Written};

use crate::shell::{Signal, StreamOp};

/// The reason in the `Closed` item of a stream that a new stream replaces.
pub const REPLACED: &str = "replaced";
/// The reason in the `Closed` item of a stream that the core closed before
/// the task opened it.
pub const CLOSED: &str = "closed";

/// The state of the stream of the stream task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streams {
    /// The connection id of the stream that did not end.
    open: Option<u32>,
}

impl Streams {
    /// Returns the connection id of the stream that did not end.
    #[must_use]
    pub const fn open(&self) -> Option<u32> {
        self.open
    }

    /// Returns `true` if a `Write` or a `Close` for `connection` applies.
    /// They apply only to the open stream. The task ignores a job for an
    /// older stream, because the shell answered its requests at the end of
    /// that stream.
    #[must_use]
    pub fn accepts(&self, connection: u32) -> bool {
        self.open == Some(connection)
    }

    /// Starts the stream of `connection`. If another stream is open, returns
    /// its `Closed` signal. The task must then close the old socket.
    pub fn begin(&mut self, connection: u32) -> Option<Signal> {
        let old = self.open.replace(connection)?;
        (old != connection).then(|| closed(old, REPLACED.into()))
    }

    /// Returns the `Opened` signal when the socket of `connection` is
    /// connected, or `None` if its stream ended.
    #[must_use]
    pub fn opened(&self, connection: u32) -> Option<Signal> {
        self.item(connection, StreamItem::Opened)
    }

    /// Returns the signal for the bytes that arrived on `connection`, or
    /// `None` if its stream ended.
    #[must_use]
    pub fn received(&self, connection: u32, bytes: &[u8]) -> Option<Signal> {
        if !self.accepts(connection) {
            return None;
        }
        self.item(connection, StreamItem::Received(Vec::from(bytes)))
    }

    /// Returns the answer to the oldest `Write` job of `connection`, or
    /// `None` if its stream ended. At the end of a stream, the shell answers
    /// each `Write` request of the stream with `Failed`.
    #[must_use]
    pub fn written(&self, connection: u32, outcome: Written) -> Option<Signal> {
        self.accepts(connection).then_some(Signal::Written {
            connection,
            outcome,
        })
    }

    /// Ends the stream of `connection`. Returns its `Closed` signal, or
    /// `None` if the stream already ended.
    pub fn end(&mut self, connection: u32, reason: String) -> Option<Signal> {
        if !self.accepts(connection) {
            return None;
        }
        self.open = None;
        Some(closed(connection, reason))
    }

    fn item(self, connection: u32, item: StreamItem) -> Option<Signal> {
        self.accepts(connection)
            .then_some(Signal::Stream { connection, item })
    }
}

const fn closed(connection: u32, reason: String) -> Signal {
    Signal::Stream {
        connection,
        item: StreamItem::Closed(reason),
    }
}

/// Removes the queued jobs that a later job supersedes, and returns the
/// `Closed` signals for the streams that the task never opens.
///
/// The task calls this function with all the queued jobs when it takes an
/// `Open` from its queue. The rules are as follows:
///
/// - Only the last `Open` in the queue opens a stream. Each earlier `Open`,
///   and each job for its connection, is removed. Its stream ends at once
///   with `Closed`.
/// - If a `Close` for the last `Open` follows it, both are removed, with all
///   the jobs for that connection. Its stream ends at once.
/// - The other jobs stay, in their order.
#[must_use]
pub fn supersede(queue: Vec<(u32, StreamOp)>) -> (Vec<Signal>, Vec<(u32, StreamOp)>) {
    let opens: Vec<u32> = queue
        .iter()
        .filter(|(_, op)| matches!(op, StreamOp::Open(_)))
        .map(|(id, _)| *id)
        .collect();
    let Some(&last) = opens.last() else {
        return (Vec::new(), queue);
    };
    let mut skipped: Vec<u32> = opens.iter().copied().filter(|id| *id != last).collect();
    let mut signals: Vec<Signal> = skipped
        .iter()
        .map(|id| closed(*id, REPLACED.into()))
        .collect();
    let open_at = queue
        .iter()
        .rposition(|(id, op)| *id == last && matches!(op, StreamOp::Open(_)));
    let closed_after = queue.iter().enumerate().any(|(index, (id, op))| {
        *id == last && matches!(op, StreamOp::Close(_)) && Some(index) > open_at
    });
    if closed_after {
        signals.push(closed(last, CLOSED.into()));
        skipped.push(last);
    }
    let kept = queue
        .into_iter()
        .filter(|(id, _)| !skipped.contains(id))
        .collect();
    (signals, kept)
}

/// An update of the core that takes longer than this time is slow. The task
/// of the shell logs a warning for it.
pub const SLOW_UPDATE: Duration = Duration::from_millis(50);

/// The number of updates of the core, and the time that they took.
///
/// The task of the shell records each update. It starts new statistics when
/// the core opens a stream, and logs them when the link is quiet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// The number of updates.
    pub count: u32,
    /// The total time of the updates.
    pub total: Duration,
    /// The time of the longest update.
    pub max: Duration,
}

impl UpdateStats {
    /// Records an update that took `took`. Returns `true` if the update was
    /// slow, which is longer than [`SLOW_UPDATE`].
    pub fn record(&mut self, took: Duration) -> bool {
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(took);
        self.max = self.max.max(took);
        took > SLOW_UPDATE
    }

    /// Returns the statistics, and starts new statistics.
    #[must_use]
    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec, vec::Vec};
    use core::time::Duration;

    use shared::capabilities::stream::{Close, Open, StreamItem, Write, Written};

    use super::{CLOSED, REPLACED, SLOW_UPDATE, Streams, UpdateStats, supersede};
    use crate::shell::{Signal, StreamOp};

    /// Returns the connection id and the reason of a `Closed` signal, or
    /// `None` for another signal.
    fn closed(signal: Option<&Signal>) -> Option<(u32, &str)> {
        match signal {
            Some(Signal::Stream {
                connection,
                item: StreamItem::Closed(reason),
            }) => Some((*connection, reason.as_str())),
            _ => None,
        }
    }

    #[test]
    fn a_stream_ends_once() {
        let mut streams = Streams::default();
        assert_eq!(streams.begin(1), None);
        assert!(streams.opened(1).is_some());
        let signal = streams.end(1, "reset by peer".into());
        assert_eq!(closed(signal.as_ref()), Some((1, "reset by peer")));
        assert_eq!(streams.end(1, "again".into()), None);
        assert_eq!(streams.opened(1), None);
        assert_eq!(streams.received(1, b"a"), None);
        assert_eq!(streams.written(1, Written::Sent), None);
        assert_eq!(streams.open(), None);
    }

    #[test]
    fn a_new_stream_ends_the_old_stream() {
        let mut streams = Streams::default();
        streams.begin(1);
        assert_eq!(closed(streams.begin(2).as_ref()), Some((1, REPLACED)));
        assert!(!streams.accepts(1));
        assert!(streams.accepts(2));
        // The old socket closes later. Its stream already ended.
        assert_eq!(streams.end(1, "closed".into()), None);
        assert!(streams.accepts(2));
    }

    #[test]
    fn the_same_connection_again_does_not_end_its_stream() {
        let mut streams = Streams::default();
        streams.begin(4);
        assert_eq!(streams.begin(4), None);
        assert!(streams.accepts(4));
    }

    #[test]
    fn bytes_and_answers_go_to_the_open_stream() {
        let mut streams = Streams::default();
        streams.begin(3);
        assert_eq!(
            streams.received(3, &[0x20, 0x02, 0x00, 0x00]),
            Some(Signal::Stream {
                connection: 3,
                item: StreamItem::Received(vec![0x20, 0x02, 0x00, 0x00]),
            })
        );
        let failed = Written::Failed("timeout".into());
        assert_eq!(
            streams.written(3, failed.clone()),
            Some(Signal::Written {
                connection: 3,
                outcome: failed,
            })
        );
        assert_eq!(streams.received(2, b"a"), None);
    }

    fn open(id: u32) -> (u32, StreamOp) {
        let open = Open {
            connection: id,
            host: "broker".into(),
            port: 1883,
        };
        (id, StreamOp::Open(open))
    }

    fn write(id: u32) -> (u32, StreamOp) {
        let write = Write {
            connection: id,
            bytes: vec![0xC0, 0x00],
        };
        (id, StreamOp::Write(write))
    }

    fn close(id: u32) -> (u32, StreamOp) {
        let close = Close {
            connection: id,
            clean: true,
            reason: String::from("clean"),
        };
        (id, StreamOp::Close(close))
    }

    fn ended(signals: &[Signal]) -> Vec<(u32, &str)> {
        signals
            .iter()
            .map(|signal| closed(Some(signal)).expect("a Closed signal"))
            .collect()
    }

    #[test]
    fn only_the_last_open_opens() {
        let queue = vec![open(1), write(1), close(1), open(2), write(2)];
        let (signals, kept) = supersede(queue);
        assert_eq!(ended(&signals), [(1, REPLACED)]);
        assert_eq!(kept, [open(2), write(2)]);
    }

    #[test]
    fn an_open_with_a_close_behind_it_does_not_open() {
        let queue = vec![close(7), open(8), write(8), close(8)];
        let (signals, kept) = supersede(queue);
        assert_eq!(ended(&signals), [(8, CLOSED)]);
        // The close of an older stream stays.
        assert_eq!(kept, [close(7)]);
    }

    #[test]
    fn a_close_before_the_last_open_does_not_stop_it() {
        let queue = vec![close(8), open(8), write(8)];
        let (signals, kept) = supersede(queue.clone());
        assert_eq!(signals, Vec::new());
        assert_eq!(kept, queue);
    }

    #[test]
    fn a_queue_with_no_open_does_not_change() {
        let queue = vec![write(3), close(3)];
        let (signals, kept) = supersede(queue.clone());
        assert_eq!(signals, Vec::new());
        assert_eq!(kept, queue);
    }

    #[test]
    fn update_stats_count_the_updates_and_keep_the_longest() {
        let mut stats = UpdateStats::default();
        assert!(!stats.record(Duration::from_millis(3)));
        assert!(!stats.record(Duration::from_millis(12)));
        assert!(!stats.record(Duration::from_millis(5)));
        assert_eq!(
            stats,
            UpdateStats {
                count: 3,
                total: Duration::from_millis(20),
                max: Duration::from_millis(12),
            }
        );
    }

    #[test]
    fn an_update_over_the_limit_is_slow() {
        let mut stats = UpdateStats::default();
        assert!(!stats.record(SLOW_UPDATE));
        assert!(stats.record(SLOW_UPDATE + Duration::from_micros(1)));
    }

    #[test]
    fn take_starts_new_update_stats() {
        let mut stats = UpdateStats::default();
        stats.record(Duration::from_millis(7));
        let taken = stats.take();
        assert_eq!(taken.count, 1);
        assert_eq!(stats, UpdateStats::default());
    }
}
