//! Transport-neutral station admission.
//!
//! Listener owners establish the underlying connection and its security
//! policy, then hand the ready byte stream to the protocol server. Session
//! framing and lifecycle remain independent of the transport implementation.

use std::fmt;
use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use super::ServerError;
use super::qos::StationSocketQos;
use crate::types::{SignalingQos, StationTransport};

/// Bidirectional asynchronous byte stream accepted by a station session.
///
/// The protocol server owns the stream after admission and applies identical
/// framing, backpressure, registration, and shutdown behavior regardless of
/// the underlying transport. A transport adapter may implement this trait with
/// a plain socket, a decrypted secure stream, or an in-memory test stream.
pub trait StationIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> StationIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(super) type BoxedStationIo = Box<dyn StationIo>;

pub(super) struct AcceptedStation {
    pub stream: BoxedStationIo,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub transport: StationTransport,
    pub socket_qos: Option<Box<dyn StationSocketQos>>,
}

impl fmt::Debug for AcceptedStation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcceptedStation")
            .field("stream", &"<station I/O>")
            .field("peer", &self.peer)
            .field("local", &self.local)
            .field("transport", &self.transport)
            .field(
                "socket_qos",
                &self.socket_qos.as_ref().map(|_| "<socket QoS control>"),
            )
            .finish()
    }
}

#[derive(Clone, Debug)]
/// Cloneable admission endpoint returned by [`super::Server::with_ingress`].
///
/// A listener owner performs transport-specific setup first, then submits the
/// ready byte stream with its actual peer address, accepted local address, and
/// transport classification. Clones share one bounded queue, so awaiting
/// [`Self::accept`] propagates server backpressure instead of creating
/// unbounded session work.
pub struct ServerIngress {
    sender: mpsc::Sender<AcceptedStation>,
    signaling_qos: SignalingQos,
}

impl ServerIngress {
    pub(super) fn channel(
        capacity: usize,
        signaling_qos: SignalingQos,
    ) -> (Self, mpsc::Receiver<AcceptedStation>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                signaling_qos,
            },
            receiver,
        )
    }

    /// Transfer ownership of an accepted stream to the server run loop.
    ///
    /// `peer` identifies the remote station for events and address policy;
    /// `local` is the concrete local endpoint used for server-list responses.
    /// `transport` must describe the already-established stream because it is
    /// checked against the device definition during registration. For secure
    /// admission, complete the handshake and any certificate policy before
    /// calling this method.
    ///
    /// The method waits for capacity in the ingress queue. It returns
    /// [`ServerError::Stopped`] without starting a session if the run loop has
    /// ended.
    pub async fn accept<S>(
        &self,
        stream: S,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
    ) -> Result<(), ServerError>
    where
        S: StationIo + 'static,
    {
        self.admit(Box::new(stream), peer, local, transport, None)
            .await
    }

    /// Admit a stream while retaining control of its underlying TCP markings.
    ///
    /// The server reapplies the selected station's signaling policy after the
    /// registration message identifies it. Marking failures are logged while
    /// registration and subsequent protocol traffic continue normally.
    pub async fn accept_with_socket_qos<S, Q>(
        &self,
        stream: S,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
        socket_qos: Q,
    ) -> Result<(), ServerError>
    where
        S: StationIo + 'static,
        Q: StationSocketQos + 'static,
    {
        super::report_socket_qos(None, peer, socket_qos.apply(self.signaling_qos));
        self.admit(
            Box::new(stream),
            peer,
            local,
            transport,
            Some(Box::new(socket_qos)),
        )
        .await
    }

    async fn admit(
        &self,
        stream: BoxedStationIo,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
        socket_qos: Option<Box<dyn StationSocketQos>>,
    ) -> Result<(), ServerError> {
        self.sender
            .send(AcceptedStation {
                stream,
                peer,
                local,
                transport,
                socket_qos,
            })
            .await
            .map_err(|_| ServerError::Stopped)
    }
}
