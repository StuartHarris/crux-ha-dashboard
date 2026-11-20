//! The stream capability: a TCP byte stream to the broker, and the commands
//! that make its operations.
//!
//! The core runs the MQTT session and gives the shell only bytes. Each
//! operation carries the `connection` id of the core, so the core can find
//! and ignore the answers for an old connection.
//!
//! - [`Open`] is a stream. The shell answers with [`StreamItem`] values:
//!   `Opened`, a `Received` for each chunk of bytes, and one `Closed` as the
//!   last item.
//! - [`Write`] is a request. The shell answers with [`Written`] when it sent
//!   the bytes, or when it cannot send them.
//! - [`Close`] is a request. The shell answers when the stream is closed.
//!
//! The shell must obey these rules:
//!
//! - The shell finds the address of the host (DNS).
//! - A new `Open` replaces the stream that is open. The old stream ends with
//!   `Closed("replaced")`.
//! - When a stream ends, the shell answers each `Write` that waits on it
//!   with `Failed`.
//! - A `Close` cancels a `Write` that waits. The shell then sends a TCP FIN,
//!   sends the bytes that wait for no more than 2 seconds, and aborts the
//!   socket. The stream ends with `Closed`, and then the shell answers the
//!   `Close`.
//! - If no stream has the `connection` id of a `Write` or a `Close`, the
//!   shell answers at once: `Failed` for a `Write`, `()` for a `Close`.

use alloc::{string::String, vec::Vec};
use core::fmt;

use crux_core::{
    Command, Request,
    command::{RequestBuilder, StreamBuilder},
    macros::Operation,
};
use facet::Facet;
use serde::{Deserialize, Serialize};

/// Opens a TCP stream to a host.
///
/// The shell answers with a stream of [`StreamItem`] values. The last item is
/// always `Closed`, also when the stream does not open. The shell does not
/// open the stream again by itself. The core decides when to try again.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(stream, output = StreamItem)]
pub struct Open {
    /// The id of the connection in the core.
    pub connection: u32,
    /// The host name or IP address.
    pub host: String,
    /// The TCP port.
    pub port: u16,
}

/// An item in the stream of an [`Open`] request.
///
/// The `Debug` output of `Received` shows only the number of bytes.
#[derive(Facet, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[repr(C)]
pub enum StreamItem {
    /// The stream is open.
    Opened,
    /// Bytes arrived. A chunk can hold any part of the byte stream.
    Received(Vec<u8>),
    /// The stream closed or did not open, for this reason. This is the last
    /// item.
    Closed(String),
}

impl fmt::Debug for StreamItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Opened => f.write_str("Opened"),
            Self::Received(bytes) => f
                .debug_tuple("Received")
                .field(&format_args!("{} bytes", bytes.len()))
                .finish(),
            Self::Closed(reason) => f.debug_tuple("Closed").field(reason).finish(),
        }
    }
}

/// Sends bytes on the stream of a connection.
///
/// The shell sends the bytes in the order of the `Write` requests. The
/// `Debug` output shows only the number of bytes, because the CONNECT packet
/// holds the password.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[operation(request, output = Written)]
pub struct Write {
    /// The id of the connection in the core.
    pub connection: u32,
    /// The bytes to send.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Write {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Write")
            .field("connection", &self.connection)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .finish()
    }
}

/// The answer to a [`Write`] request.
#[derive(Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub enum Written {
    /// The shell sent all the bytes.
    Sent,
    /// The shell did not send all the bytes, for this reason.
    Failed(String),
}

/// Closes the stream of a connection.
///
/// The stream ends with `Closed`, and then the shell answers this request.
#[derive(Operation, Facet, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[operation(request, output = ())]
pub struct Close {
    /// The id of the connection in the core.
    pub connection: u32,
    /// If `true`, the session ended with a disconnect. If `false`, the
    /// session failed, and the shell can treat the link as bad.
    pub clean: bool,
    /// Why the session ended, for the log of the shell.
    pub reason: String,
}

/// Makes the commands for the stream operations.
pub struct Stream;

impl Stream {
    /// Opens a stream to `host` and `port` for the connection `connection`.
    /// The stream gives the items of the connection.
    #[must_use]
    pub fn open<Effect, Event>(
        connection: u32,
        host: String,
        port: u16,
    ) -> StreamBuilder<Effect, Event, impl futures::Stream<Item = StreamItem>>
    where
        Effect: From<Request<Open>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::stream_from_shell(Open {
            connection,
            host,
            port,
        })
    }

    /// Sends `bytes` on the stream of the connection `connection`. The
    /// request gives the result.
    #[must_use]
    pub fn write<Effect, Event>(
        connection: u32,
        bytes: Vec<u8>,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = Written>>
    where
        Effect: From<Request<Write>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Write { connection, bytes })
    }

    /// Closes the stream of the connection `connection`. The request ends
    /// when the stream is closed.
    #[must_use]
    pub fn close<Effect, Event>(
        connection: u32,
        clean: bool,
        reason: String,
    ) -> RequestBuilder<Effect, Event, impl Future<Output = ()>>
    where
        Effect: From<Request<Close>> + Send + 'static,
        Event: Send + 'static,
    {
        Command::request_from_shell(Close {
            connection,
            clean,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::String, vec, vec::Vec};

    use crux_core::{Command, Request};

    use super::{Close, Open, Stream, StreamItem, Write, Written};

    /// The effects of the stream capability, for the tests only.
    enum Effect {
        Open(Request<Open>),
        Write(Request<Write>),
        Close(Request<Close>),
    }

    impl From<Request<Open>> for Effect {
        fn from(request: Request<Open>) -> Self {
            Self::Open(request)
        }
    }

    impl From<Request<Write>> for Effect {
        fn from(request: Request<Write>) -> Self {
            Self::Write(request)
        }
    }

    impl From<Request<Close>> for Effect {
        fn from(request: Request<Close>) -> Self {
            Self::Close(request)
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Item(StreamItem),
        Written(Written),
        Closed,
    }

    /// Returns the one effect of `command`.
    fn effect(command: &mut Command<Effect, Event>) -> Effect {
        let mut effects: Vec<Effect> = command.effects().collect();
        assert_eq!(effects.len(), 1, "one effect");
        effects.remove(0)
    }

    #[test]
    fn open_gives_each_item_of_the_stream() {
        let mut command = Stream::open(7, "broker.local".into(), 1883).then_send(Event::Item);
        let Effect::Open(mut request) = effect(&mut command) else {
            panic!("an Open request");
        };
        assert_eq!(
            request.operation,
            Open {
                connection: 7,
                host: "broker.local".into(),
                port: 1883,
            }
        );
        let items = [
            StreamItem::Opened,
            StreamItem::Received(vec![0x20, 0x02, 0x00, 0x00]),
            StreamItem::Closed("connection lost".into()),
        ];
        for item in items.clone() {
            request
                .resolve(item)
                .expect("the core listens to the stream");
        }
        let events: Vec<Event> = command.events().collect();
        assert_eq!(events, items.map(Event::Item));
    }

    #[test]
    fn write_gives_the_result() {
        for written in [Written::Sent, Written::Failed("reset by peer".into())] {
            let mut command = Stream::write(7, vec![0xC0, 0x00]).then_send(Event::Written);
            let Effect::Write(mut request) = effect(&mut command) else {
                panic!("a Write request");
            };
            assert_eq!(
                request.operation,
                Write {
                    connection: 7,
                    bytes: vec![0xC0, 0x00],
                }
            );
            request
                .resolve(written.clone())
                .expect("the core waits for the write");
            let events: Vec<Event> = command.events().collect();
            assert_eq!(events, [Event::Written(written)]);
            assert!(command.is_done());
        }
    }

    #[test]
    fn close_ends_when_the_shell_answers() {
        let mut command =
            Stream::close(7, true, String::from("clean")).then_send(|()| Event::Closed);
        let Effect::Close(mut request) = effect(&mut command) else {
            panic!("a Close request");
        };
        assert_eq!(
            request.operation,
            Close {
                connection: 7,
                clean: true,
                reason: "clean".into(),
            }
        );
        request.resolve(()).expect("the core waits for the close");
        let events: Vec<Event> = command.events().collect();
        assert_eq!(events, [Event::Closed]);
        assert!(command.is_done());
    }

    #[test]
    fn debug_shows_only_the_length_of_the_bytes() {
        let secret = b"secret-password".to_vec();
        let write = Write {
            connection: 7,
            bytes: secret.clone(),
        };
        let received = StreamItem::Received(secret);
        assert_eq!(
            format!("{write:?}"),
            "Write { connection: 7, bytes: 15 bytes }"
        );
        assert_eq!(format!("{received:?}"), "Received(15 bytes)");
    }
}
