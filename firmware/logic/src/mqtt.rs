//! The bookkeeping of the MQTT task: which connection is open, and the end of
//! each stream.
//!
//! The device has one MQTT connection at a time. Each connection has the id
//! that the shell gave it. The stream of each connection ends with exactly one
//! `Disconnected` item:
//!
//! - when the task cannot open the connection,
//! - when the broker or the network closes the connection,
//! - when the core asks for a disconnect, or
//! - when a new connection replaces it.
//!
//! [`Streams`] makes the signals for the shell, and it makes sure that no
//! item follows the `Disconnected` item of a stream.
//!
//! [`supersede`] removes the queued work that a later job makes
//! useless. [`Outbox`] keeps the messages that wait to be published, and
//! decides when a failed publish ends the connection. [`Subacks`] counts the
//! acknowledgements of the subscribe packets, and gives the answer to each
//! `Subscribe` job. [`AckWatch`] finds sent data that waits too long for the
//! TCP ACK of the broker, which means that the link is dead.

use alloc::{collections::VecDeque, format, string::String, vec::Vec};
use core::time::Duration;

use shared::capabilities::mqtt::{Message, MqttEvent, Publish, SubscribeOutcome};

use crate::shell::{MqttOp, Signal};

/// The state of the connection of the MQTT task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streams {
    /// The id of the connection whose stream did not end.
    open: Option<u32>,
}

impl Streams {
    /// Returns the id of the connection whose stream did not end.
    #[must_use]
    pub const fn open(&self) -> Option<u32> {
        self.open
    }

    /// Returns `true` if subscribe, publish and disconnect for `connection`
    /// apply. They apply only to the open connection. The task ignores an
    /// operation for an older connection.
    #[must_use]
    pub fn accepts(&self, connection: u32) -> bool {
        self.open == Some(connection)
    }

    /// Starts the stream of a new connection. If another stream is open,
    /// returns its `Disconnected` signal. The task must then close the old
    /// connection with a disconnect packet.
    pub fn begin(&mut self, connection: u32) -> Option<Signal> {
        let old = self.open.replace(connection)?;
        (old != connection).then(|| disconnected(old, Some("replaced by a new connection".into())))
    }

    /// Returns the `Connected` signal when the broker accepted `connection`,
    /// or `None` if its stream ended.
    #[must_use]
    pub fn connected(&self, connection: u32) -> Option<Signal> {
        self.accepts(connection).then_some(Signal::Mqtt {
            connection,
            event: MqttEvent::Connected,
        })
    }

    /// Returns the signals that answer the `Subscribe` jobs of `connection`
    /// with `outcomes`, or none if its stream ended. If the stream ended,
    /// the shell already answered the jobs with `Failed`.
    #[must_use]
    pub fn subscribed(&self, connection: u32, outcomes: Vec<SubscribeOutcome>) -> Vec<Signal> {
        if !self.accepts(connection) {
            return Vec::new();
        }
        outcomes
            .into_iter()
            .map(|outcome| Signal::Subscribed {
                connection,
                outcome,
            })
            .collect()
    }

    /// Returns the signal for a message on `connection`, or `None` if its
    /// stream ended.
    #[must_use]
    pub fn message(&self, connection: u32, topic: &str, payload: &[u8]) -> Option<Signal> {
        self.accepts(connection).then(|| Signal::Mqtt {
            connection,
            event: MqttEvent::Message(Message {
                topic: topic.into(),
                payload: Vec::from(payload),
            }),
        })
    }

    /// Ends the stream of `connection`. Returns its `Disconnected` signal,
    /// or `None` if the stream already ended.
    pub fn end(&mut self, connection: u32, reason: Option<String>) -> Option<Signal> {
        if !self.accepts(connection) {
            return None;
        }
        self.open = None;
        Some(disconnected(connection, reason))
    }
}

const fn disconnected(connection: u32, reason: Option<String>) -> Signal {
    Signal::Mqtt {
        connection,
        event: MqttEvent::Disconnected { reason },
    }
}

/// Returns the time between two ping packets for a keep-alive interval in
/// seconds.
///
/// The client sends a ping after three quarters of the interval, so the
/// broker gets it in time. An interval of 0 turns keep-alive off, and the
/// function returns `None`.
#[must_use]
pub fn ping_interval(keep_alive_secs: u16) -> Option<Duration> {
    if keep_alive_secs == 0 {
        return None;
    }
    let millis = u64::from(keep_alive_secs) * 750;
    Some(Duration::from_millis(millis.max(500)))
}

/// Returns the longest silence that the client accepts from the broker for a
/// keep-alive interval in seconds.
///
/// The broker answers each ping, so it sends a packet at least once in each
/// interval. A silence of two intervals means that the link is dead. The
/// function uses an interval of at least 30 seconds. An interval of 0 turns
/// keep-alive off, and the function returns `None`.
#[must_use]
pub fn silence_limit(keep_alive_secs: u16) -> Option<Duration> {
    if keep_alive_secs == 0 {
        return None;
    }
    Some(Duration::from_secs(u64::from(keep_alive_secs.max(30)) * 2))
}

/// Returns `true` if the broker was silent for too long. `silent_for` is the
/// time since the last packet from the broker. The limit comes from
/// [`silence_limit`]. With keep-alive off, the function returns `false`.
#[must_use]
pub fn broker_silent(silent_for: Duration, keep_alive_secs: u16) -> bool {
    silence_limit(keep_alive_secs).is_some_and(|limit| silent_for >= limit)
}

/// Removes the queued jobs that a later job supersedes, and
/// returns the `Disconnected` signals for the connections that the task
/// never opens.
///
/// The task calls this function with all the queued jobs when it takes
/// a `Connect` from its queue. The rules:
///
/// - Only the last `Connect` in the queue opens a connection. Each earlier
///   `Connect`, and each job for its connection, is removed. Its stream
///   ends at once with `Disconnected`.
/// - If a `Disconnect` for the last `Connect` follows it, both are removed,
///   with all the jobs for that connection. Its stream ends at once.
/// - The other jobs stay, in their order.
#[must_use]
pub fn supersede(queue: Vec<(u32, MqttOp)>) -> (Vec<Signal>, Vec<(u32, MqttOp)>) {
    let connects: Vec<u32> = queue
        .iter()
        .filter(|(_, op)| matches!(op, MqttOp::Connect(_)))
        .map(|(id, _)| *id)
        .collect();
    let Some(&last) = connects.last() else {
        return (Vec::new(), queue);
    };
    let mut signals: Vec<Signal> = connects
        .iter()
        .filter(|id| **id != last)
        .map(|id| disconnected(*id, Some("superseded by a later connection".into())))
        .collect();
    let mut skipped: Vec<u32> = connects.iter().copied().filter(|id| *id != last).collect();
    let connect_at = queue
        .iter()
        .rposition(|(id, op)| *id == last && matches!(op, MqttOp::Connect(_)));
    let closed = queue.iter().enumerate().any(|(index, (id, op))| {
        *id == last && matches!(op, MqttOp::Disconnect(_)) && Some(index) > connect_at
    });
    if closed {
        signals.push(disconnected(last, None));
        skipped.push(last);
    }
    let kept = queue
        .into_iter()
        .filter(|(id, _)| !skipped.contains(id))
        .collect();
    (signals, kept)
}

/// The most messages that wait to be published.
pub const OUTBOX_LIMIT: usize = 16;
/// The most tries to publish one message while the client has no quota.
pub const MAX_TRIES: u32 = 20;
/// The wait before the next try, if no acknowledgement arrives first.
pub const RETRY_WAIT: Duration = Duration::from_millis(250);

/// The result of one try to publish a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// The client sent the message.
    Sent,
    /// The client has no quota now (too many messages in flight). A later
    /// try can work.
    Busy,
    /// The client cannot send the message, for this reason.
    Failed(String),
}

/// What the task does after a try.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Publish the next message, if there is one.
    Continue,
    /// Try the first message again after [`RETRY_WAIT`], or when an
    /// acknowledgement arrives.
    Wait,
    /// Close the connection, and end its stream with this reason. The core
    /// then reconnects.
    End(String),
}

/// The messages that wait to be published on the open connection, in order.
#[derive(Clone, Debug, Default)]
pub struct Outbox {
    queue: VecDeque<Publish>,
    /// The number of tries of the first message.
    tries: u32,
}

impl Outbox {
    /// Adds a message to the end of the queue.
    ///
    /// # Errors
    ///
    /// Returns the reason to close the connection if the queue is full.
    pub fn push(&mut self, publish: Publish) -> Result<(), String> {
        if self.queue.len() >= OUTBOX_LIMIT {
            return Err(format!(
                "{OUTBOX_LIMIT} messages wait to be published, so the connection is stuck"
            ));
        }
        self.queue.push_back(publish);
        Ok(())
    }

    /// Returns the message to publish next.
    #[must_use]
    pub fn front(&self) -> Option<&Publish> {
        self.queue.front()
    }

    /// Returns `true` if no message waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Records the result of a try to publish the first message, and returns
    /// what to do next.
    pub fn record(&mut self, attempt: Attempt) -> Next {
        match attempt {
            Attempt::Sent => {
                self.queue.pop_front();
                self.tries = 0;
                Next::Continue
            }
            Attempt::Busy => {
                self.tries += 1;
                if self.tries < MAX_TRIES {
                    return Next::Wait;
                }
                let topic = self.front().map_or("", |publish| publish.topic.as_str());
                Next::End(format!(
                    "no quota to publish to {topic} after {MAX_TRIES} tries"
                ))
            }
            Attempt::Failed(reason) => Next::End(reason),
        }
    }
}

/// The longest time for which sent data can wait for the TCP ACK of the
/// broker. After this time, the task closes the connection, because the
/// link is dead.
pub const ACK_LIMIT: Duration = Duration::from_secs(10);

/// Watches the progress of the TCP ACKs of the broker for the data in the
/// send buffer of the TCP socket.
///
/// A send does not wait for the ACK, so the task must find a dead link in a
/// different way. The task reports the send buffer with
/// [`AckWatch::observe`]: before and after each write, and at the deadline
/// that `observe` returns. Each report gives the number of bytes that the
/// task wrote to the socket since it opened, and the number of bytes in the
/// send buffer. The difference is the number of bytes that the broker
/// acknowledged.
///
/// The time starts when data waits and the broker acknowledged no new data.
/// Each report that shows new acknowledged data starts the time again, so a
/// long burst on a slow link does not end the connection while the ACKs
/// continue. If there is no progress for [`ACK_LIMIT`] while data waits,
/// [`AckWatch::expired`] is `true`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckWatch {
    /// The number of acknowledged bytes at the last report.
    acked: u64,
    /// The time of the last progress while data waits, or `None` if no data
    /// waits.
    since: Option<Duration>,
}

impl AckWatch {
    /// Records the send buffer at `now`. `written` is the number of bytes
    /// that the task wrote since the socket opened, and `queued` the number
    /// of bytes in the send buffer. Returns the time at which the task must
    /// report again, or `None` if no data waits.
    pub fn observe(&mut self, now: Duration, written: u64, queued: usize) -> Option<Duration> {
        let queued = u64::try_from(queued).unwrap_or(u64::MAX);
        let acked = written.saturating_sub(queued);
        let progress = acked > self.acked;
        self.acked = self.acked.max(acked);
        if queued == 0 {
            self.since = None;
        } else if progress || self.since.is_none() {
            self.since = Some(now);
        }
        self.since.map(|since| since + ACK_LIMIT)
    }

    /// Returns `true` if data waited with no new ACK for [`ACK_LIMIT`] or
    /// more at `now`.
    #[must_use]
    pub fn expired(&self, now: Duration) -> bool {
        self.since.is_some_and(|since| now >= since + ACK_LIMIT)
    }
}

/// The subscribe packets of one `Subscribe` job.
#[derive(Clone, Debug, Default)]
struct Batch {
    /// The packet identifiers that wait for an acknowledgement.
    waiting: Vec<u16>,
    /// The broker refused a topic filter of the job.
    refused: bool,
    /// The task sent all the packets of the job.
    sent_all: bool,
}

/// The acknowledgements (SUBACK packets) that the `Subscribe` jobs of one
/// connection wait for.
///
/// `rust-mqtt` sends one topic filter in each subscribe packet. A job is
/// `Done` when the broker acknowledged each of its packets with success. If
/// the broker refuses a filter, the job is `Failed`. The answers come in the
/// order of the jobs. If the connection closes first, the job gets no answer
/// here. The shell then answers it with `Failed` at the end of the stream.
#[derive(Clone, Debug, Default)]
pub struct Subacks {
    batches: VecDeque<Batch>,
}

impl Subacks {
    /// Starts a new `Subscribe` job.
    pub fn begin(&mut self) {
        self.batches.push_back(Batch::default());
    }

    /// Records that the task sent the subscribe packet with the identifier
    /// `packet` for the newest job.
    pub fn sent(&mut self, packet: u16) {
        if let Some(batch) = self.batches.back_mut() {
            batch.waiting.push(packet);
        }
    }

    /// Records that the task sent all the packets of the newest job.
    /// Returns the answers of the jobs that are complete now, in order.
    pub fn finish(&mut self) -> Vec<SubscribeOutcome> {
        if let Some(batch) = self.batches.back_mut() {
            batch.sent_all = true;
        }
        self.complete()
    }

    /// Records the acknowledgement of the packet `packet`. `success` is
    /// `false` if the reason code is an error. Returns the answers of the
    /// jobs that are complete now, in order.
    pub fn ack(&mut self, packet: u16, success: bool) -> Vec<SubscribeOutcome> {
        let batch = self
            .batches
            .iter_mut()
            .find(|batch| batch.waiting.contains(&packet));
        if let Some(batch) = batch {
            batch.waiting.retain(|waiting| *waiting != packet);
            batch.refused |= !success;
        }
        self.complete()
    }

    /// Removes the complete jobs at the front, and returns their answers.
    fn complete(&mut self) -> Vec<SubscribeOutcome> {
        let mut outcomes = Vec::new();
        while let Some(batch) = self.batches.front() {
            if !batch.sent_all || !batch.waiting.is_empty() {
                break;
            }
            outcomes.push(if batch.refused {
                SubscribeOutcome::Failed
            } else {
                SubscribeOutcome::Done
            });
            self.batches.pop_front();
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use shared::capabilities::mqtt::MqttEvent;

    use alloc::{vec, vec::Vec};

    use shared::capabilities::mqtt::Publish;

    use super::{
        ACK_LIMIT, AckWatch, Attempt, MAX_TRIES, Next, OUTBOX_LIMIT, Outbox, Streams, Subacks,
        broker_silent, ping_interval, silence_limit, supersede,
    };
    use shared::capabilities::mqtt::{
        Disconnect, Subscribe,
        SubscribeOutcome::{Done, Failed},
    };

    use crate::shell::MqttOp;
    use crate::shell::Signal;

    fn is_disconnected(signal: Option<&Signal>, id: u32) -> bool {
        matches!(
            signal,
            Some(Signal::Mqtt {
                connection,
                event: MqttEvent::Disconnected { .. }
            }) if *connection == id
        )
    }

    #[test]
    fn a_stream_ends_once() {
        let mut streams = Streams::default();
        assert_eq!(streams.begin(1), None);
        assert!(streams.connected(1).is_some());
        assert!(is_disconnected(
            streams.end(1, Some("network".into())).as_ref(),
            1
        ));
        assert_eq!(streams.end(1, None), None);
        assert_eq!(streams.connected(1), None);
        assert_eq!(streams.subscribed(1, vec![Done]), []);
        assert_eq!(streams.message(1, "a", b"b"), None);
        assert_eq!(streams.open(), None);
    }

    #[test]
    fn a_new_connection_ends_the_old_stream() {
        let mut streams = Streams::default();
        streams.begin(1);
        assert!(is_disconnected(streams.begin(2).as_ref(), 1));
        assert!(!streams.accepts(1));
        assert!(streams.accepts(2));
        // The old socket closes later. Its stream already ended.
        assert_eq!(streams.end(1, None), None);
        assert!(streams.accepts(2));
    }

    #[test]
    fn messages_go_to_the_open_connection() {
        let mut streams = Streams::default();
        streams.begin(3);
        let Some(Signal::Mqtt {
            connection: 3,
            event: MqttEvent::Message(message),
        }) = streams.message(3, "topic/a", b"on")
        else {
            panic!("no message");
        };
        assert_eq!(message.topic, "topic/a");
        assert_eq!(message.payload, b"on");
    }

    const fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn an_empty_send_buffer_has_no_deadline() {
        let mut watch = AckWatch::default();
        assert_eq!(watch.observe(secs(5), 100, 0), None);
        assert!(!watch.expired(secs(100)));
    }

    #[test]
    fn data_with_no_progress_expires() {
        let mut watch = AckWatch::default();
        assert_eq!(watch.observe(secs(5), 100, 100), Some(secs(5) + ACK_LIMIT));
        // A later write with no new ACK keeps the first time.
        assert_eq!(watch.observe(secs(8), 150, 150), Some(secs(5) + ACK_LIMIT));
        assert!(!watch.expired(secs(14)));
        assert_eq!(watch.observe(secs(15), 150, 150), Some(secs(15)));
        assert!(watch.expired(secs(15)));
    }

    #[test]
    fn a_long_burst_with_steady_progress_does_not_expire() {
        let mut watch = AckWatch::default();
        let mut written = 0;
        // For 60 s, the task writes 2000 bytes each second, and the broker
        // acknowledges all but the newest 1500 bytes. The buffer is never
        // empty.
        for second in 0..60 {
            written += 2000;
            let deadline = watch.observe(secs(second), written, 1500);
            assert_eq!(deadline, Some(secs(second) + ACK_LIMIT));
            assert!(!watch.expired(secs(second)));
        }
    }

    #[test]
    fn a_burst_that_stops_making_progress_expires() {
        let mut watch = AckWatch::default();
        watch.observe(secs(0), 2000, 1500);
        watch.observe(secs(1), 4000, 1500);
        // The ACKs stop. The buffer holds the same unacknowledged data.
        watch.observe(secs(2), 4000, 1500);
        watch.observe(secs(6), 4000, 1500);
        assert!(!watch.expired(secs(10)));
        assert!(watch.expired(secs(11)));
    }

    #[test]
    fn an_ack_clears_the_deadline() {
        let mut watch = AckWatch::default();
        watch.observe(secs(5), 100, 100);
        assert_eq!(watch.observe(secs(6), 100, 0), None);
        assert!(!watch.expired(secs(20)));
        // New data starts a new time.
        assert_eq!(watch.observe(secs(20), 150, 50), Some(secs(20) + ACK_LIMIT));
    }

    #[test]
    fn subscribed_answers_the_jobs_of_the_open_connection() {
        let mut streams = Streams::default();
        streams.begin(5);
        assert_eq!(
            streams.subscribed(5, vec![Done, Failed]),
            [
                Signal::Subscribed {
                    connection: 5,
                    outcome: Done
                },
                Signal::Subscribed {
                    connection: 5,
                    outcome: Failed
                },
            ]
        );
        assert_eq!(streams.subscribed(4, vec![Done]), []);
    }

    #[test]
    fn a_subscribe_is_done_when_each_packet_is_acknowledged() {
        let mut subacks = Subacks::default();
        subacks.begin();
        subacks.sent(1);
        subacks.sent(2);
        // An acknowledgement can arrive while the task still sends.
        assert_eq!(subacks.ack(1, true), []);
        subacks.sent(3);
        assert_eq!(subacks.finish(), []);
        assert_eq!(subacks.ack(3, true), []);
        assert_eq!(subacks.ack(2, true), [Done]);
        // The job is complete, so a late or unknown packet changes nothing.
        assert_eq!(subacks.ack(2, true), []);
    }

    #[test]
    fn a_subscribe_with_all_acknowledgements_before_the_end_is_done_at_the_end() {
        let mut subacks = Subacks::default();
        subacks.begin();
        subacks.sent(1);
        assert_eq!(subacks.ack(1, true), []);
        assert_eq!(subacks.finish(), [Done]);
    }

    #[test]
    fn a_subscribe_with_no_filters_is_done() {
        let mut subacks = Subacks::default();
        subacks.begin();
        assert_eq!(subacks.finish(), [Done]);
    }

    #[test]
    fn a_refused_filter_fails_the_subscribe() {
        let mut subacks = Subacks::default();
        subacks.begin();
        subacks.sent(1);
        subacks.sent(2);
        assert_eq!(subacks.finish(), []);
        assert_eq!(subacks.ack(1, false), []);
        assert_eq!(subacks.ack(2, true), [Failed]);
    }

    #[test]
    fn the_answers_come_in_the_order_of_the_jobs() {
        let mut subacks = Subacks::default();
        subacks.begin();
        subacks.sent(1);
        assert_eq!(subacks.finish(), []);
        subacks.begin();
        subacks.sent(2);
        assert_eq!(subacks.finish(), []);
        // The second job is complete first, but its answer waits.
        assert_eq!(subacks.ack(2, false), []);
        assert_eq!(subacks.ack(1, true), [Done, Failed]);
    }

    #[test]
    fn silence_limit_is_two_keep_alive_intervals() {
        assert_eq!(silence_limit(60), Some(Duration::from_secs(120)));
    }

    #[test]
    fn silence_limit_uses_at_least_30_seconds() {
        assert_eq!(silence_limit(5), Some(Duration::from_secs(60)));
    }

    #[test]
    fn silence_limit_is_off_without_keep_alive() {
        assert_eq!(silence_limit(0), None);
    }

    #[test]
    fn broker_is_not_silent_just_under_the_limit() {
        assert!(!broker_silent(Duration::from_millis(119_999), 60));
    }

    #[test]
    fn broker_is_silent_at_and_over_the_limit() {
        assert!(broker_silent(Duration::from_secs(120), 60));
        assert!(broker_silent(Duration::from_secs(500), 60));
    }

    #[test]
    fn broker_is_never_silent_without_keep_alive() {
        assert!(!broker_silent(Duration::from_secs(100_000), 0));
    }

    #[test]
    fn small_keep_alive_uses_the_clamped_limit() {
        assert!(!broker_silent(Duration::from_secs(59), 5));
        assert!(broker_silent(Duration::from_secs(60), 5));
    }

    #[test]
    fn ping_before_the_keep_alive_interval() {
        assert_eq!(ping_interval(0), None);
        assert_eq!(ping_interval(60), Some(Duration::from_secs(45)));
        assert_eq!(ping_interval(1), Some(Duration::from_millis(750)));
    }

    fn connect(id: u32) -> (u32, MqttOp) {
        let connect = shared::capabilities::mqtt::Connect {
            host: "broker".into(),
            port: 1883,
            client_id: "dashboard-test".into(),
            credentials: None,
            keep_alive_secs: 60,
            last_will: shared::capabilities::mqtt::LastWill {
                topic: "dashboard/test/status".into(),
                payload: b"offline".to_vec(),
                retain: true,
            },
        };
        (id, MqttOp::Connect(connect))
    }

    fn publish(topic: &str) -> Publish {
        Publish {
            topic: topic.into(),
            payload: b"on".to_vec(),
            qos: shared::capabilities::mqtt::Qos::AtLeastOnce,
            retain: false,
        }
    }

    fn ended(signals: &[Signal]) -> Vec<(u32, bool)> {
        signals
            .iter()
            .map(|signal| match signal {
                Signal::Mqtt {
                    connection,
                    event: MqttEvent::Disconnected { reason },
                } => (*connection, reason.is_some()),
                other => panic!("not a disconnect: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn only_the_last_connect_opens() {
        let queue = vec![
            connect(1),
            (
                1,
                MqttOp::Subscribe(Subscribe {
                    topics: vec!["a".into()],
                }),
            ),
            connect(2),
            (
                2,
                MqttOp::Subscribe(Subscribe {
                    topics: vec!["b".into()],
                }),
            ),
        ];
        let (signals, kept) = supersede(queue);
        assert_eq!(ended(&signals), [(1, true)]);
        assert_eq!(
            kept,
            [
                connect(2),
                (
                    2,
                    MqttOp::Subscribe(Subscribe {
                        topics: vec!["b".into()]
                    })
                )
            ]
        );
    }

    #[test]
    fn a_connect_with_a_disconnect_behind_it_does_not_open() {
        let queue = vec![
            (7, MqttOp::Disconnect(Disconnect)),
            connect(8),
            (8, MqttOp::Publish(publish("dashboard/test/status"))),
            (8, MqttOp::Disconnect(Disconnect)),
        ];
        let (signals, kept) = supersede(queue);
        // The stream of connection 8 ends with no reason, as for a disconnect
        // that the core asked for.
        assert_eq!(ended(&signals), [(8, false)]);
        // The disconnect of an older connection stays.
        assert_eq!(kept, [(7, MqttOp::Disconnect(Disconnect))]);
    }

    #[test]
    fn a_queue_with_no_connect_does_not_change() {
        let queue = vec![(3, MqttOp::Disconnect(Disconnect))];
        let (signals, kept) = supersede(queue.clone());
        assert_eq!(signals, Vec::new());
        assert_eq!(kept, queue);
    }

    #[test]
    fn a_sent_message_leaves_the_outbox() {
        let mut outbox = Outbox::default();
        outbox.push(publish("a")).unwrap();
        outbox.push(publish("b")).unwrap();
        assert_eq!(outbox.record(Attempt::Sent), Next::Continue);
        assert_eq!(outbox.front().unwrap().topic, "b");
        assert_eq!(outbox.record(Attempt::Sent), Next::Continue);
        assert!(outbox.is_empty());
    }

    #[test]
    fn a_busy_client_waits_then_ends_the_connection() {
        let mut outbox = Outbox::default();
        outbox.push(publish("dashboard/test/cmd")).unwrap();
        for _ in 1..MAX_TRIES {
            assert_eq!(outbox.record(Attempt::Busy), Next::Wait);
        }
        let Next::End(reason) = outbox.record(Attempt::Busy) else {
            panic!("the connection did not end");
        };
        assert!(reason.contains("dashboard/test/cmd"));
    }

    #[test]
    fn a_failed_publish_ends_the_connection() {
        let mut outbox = Outbox::default();
        outbox.push(publish("a")).unwrap();
        assert_eq!(
            outbox.record(Attempt::Failed("packet too large".into())),
            Next::End("packet too large".into())
        );
    }

    #[test]
    fn a_full_outbox_ends_the_connection() {
        let mut outbox = Outbox::default();
        for _ in 0..OUTBOX_LIMIT {
            outbox.push(publish("a")).unwrap();
        }
        assert!(outbox.push(publish("a")).is_err());
    }
}
