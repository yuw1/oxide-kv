//! Error types for the Oxide-KV client.
//!
//! Layered: every variant wraps either a transport failure (socket
//! closed, parse error) or a logical error from the server (not
//! leader, tx aborted, malformed response). `Error` is the catch-all
//! the public API returns via `Result<T>`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The TCP connection died or refused.
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),

    /// The server reply could not be parsed as JSON.
    #[error("malformed response: {0}")]
    MalformedResponse(#[from] serde_json::Error),

    /// The contacted node is not the current leader.
    #[error("not a leader (server says: {0})")]
    NotLeader(String),

    /// The coordinator aborted the transaction with the given reason.
    #[error("transaction {tx_id:?} aborted: {reason}")]
    TxAborted { tx_id: String, reason: String },

    /// Server returned an error JSON object with a `message` field.
    #[error("server error: {0}")]
    Server(String),

    /// Connection got closed mid-stream.
    #[error("connection closed by server")]
    ConnectionClosed,
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;