//! Async Rust client for Oxide-KV.
//!
//! Independent of the server crate — talks the JSON line protocol
//! over a plain TCP socket. Usable from CLI / Desktop apps / embedded
//! agents / future PyO3 bindings without pulling in the server
//! implementation.
//!
//! Wire format is one JSON object per line, `\n`-terminated.
//! Matches `rust/oxide-kv/src/client.rs` (the server's `ClientHandler`).
//!
//! Quick start:
//! ```no_run
//! use oxide_kv_client::Client;
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let mut c = Client::connect("127.0.0.1", 9101).await?;
//! c.set("hello", "world").await?;
//! assert_eq!(c.get("hello").await?, Some("world".to_owned()));
//! # Ok(()) }
//! ```

pub mod client;
pub mod connection;
pub mod error;
pub mod transaction;

// Re-exports — the public API surface callers should reach for.
pub use client::Client;
pub use error::{Error, Result};
pub use transaction::{Transaction, TxResult};
