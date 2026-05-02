//! Error types for the cloud client.

use thiserror::Error;

/// Anything that can go wrong opening or driving a cloud connection.
#[derive(Debug, Error)]
pub enum CloudClientError {
    /// Failed to open the underlying transport (network, TLS, handshake, …).
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// Failed to encode/decode a wire frame.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The transport ended cleanly without a terminal task event.
    #[error("transport closed before terminal event")]
    UnexpectedClose,
    /// All reconnect attempts have been exhausted (or are exhausted by policy).
    #[error("reconnect attempts exhausted")]
    ReconnectExhausted,
    /// Catch-all.
    #[error("{0}")]
    Other(String),
}

/// Specific failures from the connect phase.
#[derive(Debug, Error)]
pub enum ConnectError {
    /// The configured cloud URL was missing or malformed.
    #[error("invalid cloud url: {0}")]
    InvalidUrl(String),
    /// The handshake or TCP/TLS connect failed.
    #[error("transport handshake failed: {0}")]
    Handshake(String),
}
