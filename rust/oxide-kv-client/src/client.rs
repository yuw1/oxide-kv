//! High-level Oxide-KV client.
//!
//! `Client` wraps a single `Connection` and exposes raw KV ops
//! (`set` / `get` / `delete`) plus the 2PC transaction builder
//! (`begin_tx` → `Transaction::commit` / `abort`).
//!
//! Example:
//! ```no_run
//! use oxide_kv_client::Client;
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let mut c = Client::connect("127.0.0.1", 9101).await?;
//! c.set("hello", "world").await?;
//! let v = c.get("hello").await?;
//! assert_eq!(v.as_deref(), Some("world"));
//! # Ok(()) }
//! ```

use serde_json::{Value, json};

use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::transaction::{Transaction, TxResult};

/// Async Oxide-KV client. Owns a single TCP connection; not
/// thread-safe. Open one `Client` per task or wrap calls in a lock.
pub struct Client {
    conn: Connection,
}

impl Client {
    /// Open a new TCP connection to `host:port`.
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        Ok(Self {
            conn: Connection::connect(host, port).await?,
        })
    }

    /// Propose a Set. Returns the assigned log index on success.
    ///
    /// Bubbles up `Error::NotLeader` if the contacted node is a
    /// follower; callers can use `Client::discover` to handle that.
    pub async fn set(&mut self, key: &str, value: &str) -> Result<u64> {
        let resp = self
            .conn
            .send_request(&json!({"Set": {"key": key, "value": value}}))
            .await?;
        self.parse_index_response(&resp)
    }

    /// Linearizable read. Returns `None` if the key doesn't exist.
    pub async fn get(&mut self, key: &str) -> Result<Option<String>> {
        let resp = self
            .conn
            .send_request(&json!({"Get": {"key": key}}))
            .await?;
        Self::check_not_leader(&resp)?;
        match resp.get("status").and_then(Value::as_str) {
            Some("ok") => Ok(resp.get("data").and_then(Value::as_str).map(str::to_owned)),
            Some("not_found") => Ok(None),
            _ => Err(Error::Server(format!("unexpected Get response: {resp}"))),
        }
    }

    /// Propose a Delete. Returns the assigned log index.
    pub async fn delete(&mut self, key: &str) -> Result<u64> {
        let resp = self
            .conn
            .send_request(&json!({"Delete": {"key": key}}))
            .await?;
        self.parse_index_response(&resp)
    }

    /// Start a buffered 2PC transaction. The returned `Transaction`
    /// borrows `self`; ops are sent only on commit / abort.
    pub fn begin_tx(&mut self, tx_id: impl Into<String>) -> Transaction<'_> {
        Transaction::new(self, tx_id.into())
    }

    // ----- internals exposed to `Transaction` -----

    pub(crate) async fn send_begin_tx(&mut self, tx_id: &str, ops: Vec<Value>) -> Result<TxResult> {
        let resp = self
            .conn
            .send_request(&json!({"BeginTx": {"tx_id": tx_id, "ops": ops}}))
            .await?;
        Self::check_not_leader(&resp)?;
        match resp.get("status").and_then(Value::as_str) {
            Some("ok") => Ok(TxResult {
                decision: resp
                    .get("decision")
                    .and_then(Value::as_str)
                    .unwrap_or("commit")
                    .to_owned(),
                begin_index: resp
                    .get("begin_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::Server(format!("missing begin_index: {resp}")))?,
                decide_index: resp
                    .get("decide_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Error::Server(format!("missing decide_index: {resp}")))?,
                tx_id: tx_id.to_owned(),
            }),
            Some("aborted") => Err(Error::TxAborted {
                tx_id: tx_id.to_owned(),
                reason: resp
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            Some("error") => Err(Error::Server(
                resp.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("server error")
                    .to_owned(),
            )),
            _ => Err(Error::Server(format!(
                "unexpected BeginTx response: {resp}"
            ))),
        }
    }

    pub(crate) async fn send_decide_tx(&mut self, tx_id: &str, commit: bool) -> Result<()> {
        // The Rust side's `Command::DecideTx { decision: TxDecision }`
        // serializes `TxDecision` with serde's default externally-tagged
        // representation, so the wire shape is
        // `{"DecideTx": {"tx_id": "...", "decision": {"Commit": null}}}`
        // for commit / `{"Abort": null}` for abort.
        let decision = if commit {
            json!({"Commit": null})
        } else {
            json!({"Abort": null})
        };
        let resp = self
            .conn
            .send_request(&json!({"DecideTx": {"tx_id": tx_id, "decision": decision}}))
            .await?;
        self.parse_index_response(&resp).map(|_| ())
    }

    // ----- response helpers -----

    fn parse_index_response(&self, resp: &Value) -> Result<u64> {
        Self::check_not_leader(resp)?;
        match resp.get("status").and_then(Value::as_str) {
            Some("ok") => resp
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::Server(format!("missing index: {resp}"))),
            Some("error") => Err(Error::Server(
                resp.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("server error")
                    .to_owned(),
            )),
            _ => Err(Error::Server(format!("unexpected response: {resp}"))),
        }
    }

    /// Server replies to a mutation sent to a follower with
    /// `{"error": "Not a leader. ..."}` (see
    /// `rust/oxide-kv/src/client.rs`); surface that as a typed error.
    fn check_not_leader(resp: &Value) -> Result<()> {
        if let Some(msg) = resp.get("error").and_then(Value::as_str) {
            if msg.starts_with("Not a leader") {
                return Err(Error::NotLeader(msg.to_owned()));
            }
            return Err(Error::Server(msg.to_owned()));
        }
        Ok(())
    }
}
