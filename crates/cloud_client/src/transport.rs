//! Transport abstraction for the cloud client.
//!
//! The reconnect / resume / event-streaming logic in [`crate::agent`] is
//! written against this trait so it can be exercised in unit and
//! integration tests without opening a real socket. The production impl
//! is [`TungsteniteTransport`]; tests use [`InMemoryTransport`].

use async_trait::async_trait;
use cloud_protocol::Envelope;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::error::ConnectError;

/// Errors a [`Transport`] can surface.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Failed to establish a connection.
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// The connection ended (cleanly or unexpectedly).
    #[error("transport closed")]
    Closed,
    /// A wire frame failed to encode/decode.
    #[error("encode/decode error: {0}")]
    Codec(String),
    /// The underlying socket reported an error.
    #[error("io: {0}")]
    Io(String),
}

/// Sink/stream pair for sending and receiving [`Envelope`]s.
///
/// Implementations must be `Send` so the agent can hand them off across
/// task boundaries.
#[async_trait]
pub trait Transport: Send {
    /// Send one frame. Returns `Err(Closed)` if the peer has gone away.
    async fn send(&mut self, env: Envelope) -> Result<(), TransportError>;
    /// Receive one frame. Returns `Ok(None)` when the peer closed cleanly,
    /// `Err(Closed)` for an abrupt close, and `Err(Codec)` for malformed
    /// frames.
    async fn recv(&mut self) -> Result<Option<Envelope>, TransportError>;
    /// Best-effort close.
    async fn close(&mut self) -> Result<(), TransportError>;
}

// ---------------------------------------------------------------------------
// Production: tokio-tungstenite
// ---------------------------------------------------------------------------

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as TungError, Message as WsMessage},
    MaybeTlsStream, WebSocketStream,
};

/// Production [`Transport`] backed by `tokio-tungstenite`.
///
/// All non-text frames (binary, ping, pong, close) are handled
/// transparently — only payload-bearing text frames are surfaced as
/// [`Envelope`]s.
pub struct TungsteniteTransport {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl TungsteniteTransport {
    /// Open a new connection to `url`.
    pub async fn connect(url: &str) -> Result<Self, ConnectError> {
        // Validate URL up-front so we surface a useful error before the
        // tungstenite library digs into TLS / DNS.
        let parsed =
            url::Url::parse(url).map_err(|e| ConnectError::InvalidUrl(e.to_string()))?;
        match parsed.scheme() {
            "ws" | "wss" => {}
            other => {
                return Err(ConnectError::InvalidUrl(format!(
                    "unsupported scheme: {other}"
                )))
            }
        }
        let (socket, _resp) = connect_async(url)
            .await
            .map_err(|e| ConnectError::Handshake(e.to_string()))?;
        Ok(Self { socket })
    }
}

#[async_trait]
impl Transport for TungsteniteTransport {
    async fn send(&mut self, env: Envelope) -> Result<(), TransportError> {
        let bytes = serde_json::to_string(&env).map_err(|e| TransportError::Codec(e.to_string()))?;
        self.socket
            .send(WsMessage::Text(bytes))
            .await
            .map_err(map_tung_err)
    }

    async fn recv(&mut self) -> Result<Option<Envelope>, TransportError> {
        loop {
            let msg = match self.socket.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(map_tung_err(e)),
                None => return Ok(None),
            };
            match msg {
                WsMessage::Text(t) => {
                    let env = cloud_protocol::parse_message(t.as_bytes())
                        .map_err(|e| TransportError::Codec(e.to_string()))?;
                    return Ok(Some(env));
                }
                WsMessage::Binary(b) => {
                    let env = cloud_protocol::parse_message(&b)
                        .map_err(|e| TransportError::Codec(e.to_string()))?;
                    return Ok(Some(env));
                }
                WsMessage::Close(_) => return Ok(None),
                // Tungstenite handles ping/pong automatically when configured
                // with default settings; ignore stray ones here for safety.
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.socket.close(None).await.map_err(map_tung_err)
    }
}

fn map_tung_err(err: TungError) -> TransportError {
    match err {
        TungError::ConnectionClosed | TungError::AlreadyClosed => TransportError::Closed,
        TungError::Io(io) => TransportError::Io(io.to_string()),
        other => TransportError::Io(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Test: in-memory pair
// ---------------------------------------------------------------------------

/// In-memory bidirectional transport for tests.
///
/// Use [`InMemoryTransport::pair`] to get two halves wired to each other.
/// Calling [`InMemoryTransport::close`] on one half is observable as a
/// clean EOF (`Ok(None)`) on the other half's next `recv`, simulating a
/// remote close.
pub struct InMemoryTransport {
    /// Wrapped in `Option` so `close()` can drop the sender, which
    /// causes the peer's `recv()` to return `Ok(None)`.
    tx: Option<mpsc::UnboundedSender<Envelope>>,
    rx: mpsc::UnboundedReceiver<Envelope>,
}

impl InMemoryTransport {
    /// Construct two transports wired such that frames sent on one are
    /// received on the other and vice versa.
    pub fn pair() -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::unbounded_channel();
        let (b_tx, a_rx) = mpsc::unbounded_channel();
        (
            Self {
                tx: Some(a_tx),
                rx: a_rx,
            },
            Self {
                tx: Some(b_tx),
                rx: b_rx,
            },
        )
    }
}

#[async_trait]
impl Transport for InMemoryTransport {
    async fn send(&mut self, env: Envelope) -> Result<(), TransportError> {
        match self.tx.as_ref() {
            Some(tx) => tx.send(env).map_err(|_| TransportError::Closed),
            None => Err(TransportError::Closed),
        }
    }

    async fn recv(&mut self) -> Result<Option<Envelope>, TransportError> {
        Ok(self.rx.recv().await)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        // Drop our send half: the peer's recv() will return None once the
        // queue drains, simulating a clean remote close.
        self.tx = None;
        // Also close our own recv: any pending in-flight frames get
        // delivered, then we return None.
        self.rx.close();
        Ok(())
    }
}
