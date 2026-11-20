//! The byte streams of the shell: one task on the tokio runtime for each
//! `Open` request of the core.
//!
//! The core runs the MQTT session. The shell only moves bytes. An [`Opener`]
//! makes the byte stream: a TCP stream in the app, and a stream in memory to
//! a fake broker in the tests.
//!
//! The task first waits for the opener. Then it reads and writes at the same
//! time:
//!
//! - The read loop gives each chunk of bytes to the shell as
//!   `StreamItem::Received`.
//! - The write loop takes the bytes of each `Write` request from a channel,
//!   in order. It answers each request with `Written::Sent` after it wrote
//!   all the bytes.
//!
//! The stream ends when the peer closes it, when a read or a write fails, or
//! when the shell closes it. The last item is always `Closed`. The shell
//! answers the `Write` requests that wait when it gets the `Closed` item.

use std::{future::Future, io, pin::Pin, time::Duration};

use shared::capabilities::stream::{StreamItem, Written};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::shell::{Notifier, Signal};

/// The reason in the `Closed` item after the shell closed the stream.
pub const CLOSED: &str = "closed";
/// The reason in the `Closed` item of a stream that a newer `Open` replaced.
pub const REPLACED: &str = "replaced";
/// The reason in the `Closed` item after the peer closed the stream.
pub const PEER_CLOSED: &str = "the broker closed the stream";
/// The longest time that a close waits for the bytes to go and for the peer
/// to close its side.
pub const FLUSH_LIMIT: Duration = Duration::from_secs(2);
/// The size of the buffer of the read loop, in bytes.
const READ_BUFFER: usize = 8 * 1024;

/// A byte stream in both directions.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// The future that opens a byte stream.
pub type Connecting = Pin<Box<dyn Future<Output = io::Result<Box<dyn Io>>> + Send>>;

/// Makes the future that opens a byte stream to a host and a port.
pub type Opener = Box<dyn FnMut(&str, u16) -> Connecting>;

/// Returns an opener that opens a TCP stream. The future finds the address
/// of the host (DNS) and then connects.
pub fn tcp_opener() -> Opener {
    Box::new(|host, port| {
        let host = host.to_string();
        Box::pin(async move {
            let stream = TcpStream::connect((host.as_str(), port)).await?;
            // MQTT sends small packets, and the core waits for each answer.
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as Box<dyn Io>)
        })
    })
}

/// The task of one stream, as the shell sees it.
pub struct Task {
    writes: mpsc::UnboundedSender<Vec<u8>>,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl Task {
    /// Starts the task of the stream with the token `token` on `runtime`.
    /// The task waits for `connecting`, and then sends the items and the
    /// answers of the stream with `notifier`. A close waits at most
    /// `flush_limit`.
    pub fn spawn(
        runtime: &Handle,
        connecting: Connecting,
        token: u64,
        notifier: Notifier,
        flush_limit: Duration,
    ) -> Self {
        let (writes, receiver) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let handle = runtime.spawn(run(
            connecting,
            Channels {
                token,
                notifier,
                writes: receiver,
                stop: stopped,
            },
            flush_limit,
        ));
        Self {
            writes,
            stop: Some(stop),
            handle,
        }
    }

    /// Gives the bytes of a `Write` request to the write loop. If the task
    /// ended, does nothing. The `Closed` item of the task then answers the
    /// request.
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.writes.send(bytes);
    }

    /// Tells the task to close the stream. The task stops the write loop,
    /// sends a TCP FIN, waits for the peer to close its side for at most
    /// the flush limit, and then sends `Closed`.
    pub fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Stops the task at once. The task sends nothing more.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Returns `true` if the task ended.
    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

/// The channels of a task.
struct Channels {
    token: u64,
    notifier: Notifier,
    writes: mpsc::UnboundedReceiver<Vec<u8>>,
    stop: oneshot::Receiver<()>,
}

impl Channels {
    fn item(&self, item: StreamItem) {
        self.notifier.send(Signal::Stream {
            token: self.token,
            item,
        });
    }
}

/// The body of the task of a stream.
async fn run(connecting: Connecting, mut channels: Channels, flush_limit: Duration) {
    let io = tokio::select! {
        biased;
        _ = &mut channels.stop => {
            channels.item(StreamItem::Closed(CLOSED.into()));
            return;
        }
        result = connecting => match result {
            Ok(io) => io,
            Err(error) => {
                channels.item(StreamItem::Closed(error.to_string()));
                return;
            }
        },
    };
    channels.item(StreamItem::Opened);
    let (mut reader, mut writer) = tokio::io::split(io);
    let Channels {
        token,
        notifier,
        writes,
        stop,
    } = &mut channels;
    // A stop drops the write loop, so a `Write` that waits does not finish.
    let failure = tokio::select! {
        biased;
        _ = stop => None,
        reason = read_loop(&mut reader, *token, notifier) => Some(reason),
        reason = write_loop(&mut writer, writes, *token, notifier) => Some(reason),
    };
    let reason = if let Some(reason) = failure {
        reason
    } else {
        close(reader, writer, flush_limit).await;
        CLOSED.into()
    };
    channels.item(StreamItem::Closed(reason));
}

/// Gives each chunk of bytes from the peer to the shell. Returns the reason
/// when the stream ends.
async fn read_loop(reader: &mut ReadHalf<Box<dyn Io>>, token: u64, notifier: &Notifier) -> String {
    let mut buffer = vec![0; READ_BUFFER];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return PEER_CLOSED.into(),
            Ok(length) => notifier.send(Signal::Stream {
                token,
                item: StreamItem::Received(buffer[..length].to_vec()),
            }),
            Err(error) => return error.to_string(),
        }
    }
}

/// Writes the bytes of each `Write` request in order, and answers each one
/// with `Sent`. Returns the reason if a write fails.
async fn write_loop(
    writer: &mut WriteHalf<Box<dyn Io>>,
    queue: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    token: u64,
    notifier: &Notifier,
) -> String {
    while let Some(bytes) = queue.recv().await {
        if let Err(error) = writer.write_all(&bytes).await {
            return error.to_string();
        }
        notifier.send(Signal::Written {
            token,
            outcome: Written::Sent,
        });
    }
    // The shell dropped the channel. Only a stop or the read loop ends the
    // task now.
    std::future::pending().await
}

/// Sends a TCP FIN, and then reads and discards bytes until the peer
/// closes its side, for at most `limit`. The caller then drops the stream.
///
/// The read empties the socket. If a socket closes with bytes that it did
/// not read, the system can send a reset, and the broker then publishes the
/// last will.
async fn close(
    mut reader: ReadHalf<Box<dyn Io>>,
    mut writer: WriteHalf<Box<dyn Io>>,
    limit: Duration,
) {
    let _ = tokio::time::timeout(limit, async {
        if writer.shutdown().await.is_err() {
            return;
        }
        let mut buffer = vec![0; READ_BUFFER];
        while matches!(reader.read(&mut buffer).await, Ok(length) if length > 0) {}
    })
    .await;
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };

    use shared::capabilities::stream::{StreamItem, Written};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        runtime::Runtime,
    };

    use super::{CLOSED, Connecting, Io, PEER_CLOSED, Task};
    use crate::shell::{Notifier, Signal};

    /// The longest time that a test waits for a signal.
    const WAIT: Duration = Duration::from_secs(2);

    fn runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Returns a future that gives `io` at once.
    fn ready(io: tokio::io::DuplexStream) -> Connecting {
        Box::pin(async move { Ok(Box::new(io) as Box<dyn Io>) })
    }

    /// Starts a task with the token 1 and returns it with its signals.
    fn spawn(
        runtime: &Runtime,
        connecting: Connecting,
        flush_limit: Duration,
    ) -> (Task, mpsc::Receiver<Signal>) {
        let (sender, signals) = mpsc::channel();
        let notifier = Notifier::new(sender, Arc::new(|| {}));
        let task = Task::spawn(runtime.handle(), connecting, 1, notifier, flush_limit);
        (task, signals)
    }

    /// Returns the next signal, as text.
    fn next(signals: &mpsc::Receiver<Signal>) -> String {
        match signals.recv_timeout(WAIT).expect("a signal") {
            Signal::Stream { token, item } => format!("{token} {item:?}"),
            Signal::Written { token, outcome } => format!("{token} {outcome:?}"),
            Signal::Timer { .. } => panic!("a timer signal from a stream task"),
        }
    }

    #[test]
    fn the_task_gives_the_bytes_in_both_directions() {
        let runtime = runtime();
        let (client, mut peer) = tokio::io::duplex(64);
        let (task, signals) = spawn(&runtime, ready(client), WAIT);
        assert_eq!(next(&signals), "1 Opened");

        task.write(b"ping".to_vec());
        assert_eq!(next(&signals), format!("1 {:?}", Written::Sent));
        let mut buffer = [0; 4];
        runtime.block_on(peer.read_exact(&mut buffer)).unwrap();
        assert_eq!(&buffer, b"ping");

        runtime.block_on(peer.write_all(b"pong")).unwrap();
        let Ok(Signal::Stream {
            item: StreamItem::Received(bytes),
            ..
        }) = signals.recv_timeout(WAIT)
        else {
            panic!("the bytes from the peer");
        };
        assert_eq!(bytes, b"pong");
    }

    #[test]
    fn a_peer_that_closes_ends_the_stream() {
        let runtime = runtime();
        let (client, peer) = tokio::io::duplex(64);
        let (_task, signals) = spawn(&runtime, ready(client), WAIT);
        assert_eq!(next(&signals), "1 Opened");

        drop(peer);
        assert_eq!(
            next(&signals),
            format!("1 {:?}", StreamItem::Closed(PEER_CLOSED.into()))
        );
    }

    #[test]
    fn a_close_cancels_a_write_that_waits() {
        let runtime = runtime();
        // The peer never reads, so a write of more than 8 bytes waits.
        let (client, _peer) = tokio::io::duplex(8);
        let (mut task, signals) = spawn(&runtime, ready(client), Duration::from_millis(50));
        assert_eq!(next(&signals), "1 Opened");

        task.write(vec![0; 64]);
        assert!(signals.recv_timeout(Duration::from_millis(50)).is_err());
        task.close();
        // The write gets no answer from the task. The `Closed` item tells
        // the shell to answer it.
        assert_eq!(
            next(&signals),
            format!("1 {:?}", StreamItem::Closed(CLOSED.into()))
        );
    }

    #[test]
    fn a_close_sends_a_fin_and_waits_for_the_peer() {
        let runtime = runtime();
        let (client, mut peer) = tokio::io::duplex(64);
        let (mut task, signals) = spawn(&runtime, ready(client), WAIT);
        assert_eq!(next(&signals), "1 Opened");

        task.write(b"bye".to_vec());
        assert_eq!(next(&signals), format!("1 {:?}", Written::Sent));
        task.close();
        // The peer reads the bytes and then the end of the stream.
        let mut bytes = Vec::new();
        runtime.block_on(peer.read_to_end(&mut bytes)).unwrap();
        assert_eq!(bytes, b"bye");
        assert!(signals.recv_timeout(Duration::from_millis(50)).is_err());

        drop(peer);
        assert_eq!(
            next(&signals),
            format!("1 {:?}", StreamItem::Closed(CLOSED.into()))
        );
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
        assert!(task.is_finished());
    }

    #[test]
    fn a_close_while_the_stream_opens_ends_the_stream() {
        let runtime = runtime();
        let (mut task, signals) = spawn(&runtime, Box::pin(std::future::pending()), WAIT);
        assert!(signals.recv_timeout(Duration::from_millis(50)).is_err());

        task.close();
        assert_eq!(
            next(&signals),
            format!("1 {:?}", StreamItem::Closed(CLOSED.into()))
        );
    }

    #[test]
    fn a_stream_that_does_not_open_ends_with_the_error() {
        let runtime = runtime();
        let refused: Connecting = Box::pin(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            ))
        });
        let (_task, signals) = spawn(&runtime, refused, WAIT);
        assert_eq!(
            next(&signals),
            format!("1 {:?}", StreamItem::Closed("connection refused".into()))
        );
    }
}
