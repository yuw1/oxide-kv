//! 2PC transaction builder.
//!
//! Mirrors the Python SDK's `Transaction` class: ops are buffered
//! client-side and only sent to the server on `commit()` / `abort()`.
//! Chains return `&mut Self` so calls can be fluent.

use serde_json::{json, Value};

use crate::client::Client;

/// Outcome of a committed transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxResult {
    pub decision: String, // "commit"
    pub begin_index: u64,
    pub decide_index: u64,
    pub tx_id: String,
}

/// A buffered 2PC transaction.
///
/// Constructed by `Client::begin_tx(tx_id)`. `set` / `delete` mutate
/// `self.ops` and return `&mut Self` for chaining.
pub struct Transaction<'a> {
    client: &'a mut Client,
    pub tx_id: String,
    pub ops: Vec<Value>,
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(client: &'a mut Client, tx_id: String) -> Self {
        Self {
            client,
            tx_id,
            ops: Vec::new(),
        }
    }

    /// Stage a `Set` op.
    pub fn set(&mut self, key: &str, value: &str) -> &mut Self {
        self.ops.push(json!({"put": {"key": key, "value": value}}));
        self
    }

    /// Stage a `Delete` op.
    pub fn delete(&mut self, key: &str) -> &mut Self {
        self.ops.push(json!({"delete": {"key": key}}));
        self
    }

    /// Submit BeginTx and drive DecideTx(Commit) on the server side.
    pub async fn commit(mut self) -> crate::error::Result<TxResult> {
        self.client
            .send_begin_tx(&self.tx_id, std::mem::take(&mut self.ops))
            .await
    }

    /// Send a manual DecideTx(Commit=false). The server treats this
    /// as a coordinator-driven abort.
    pub async fn abort(self) -> crate::error::Result<()> {
        self.client.send_decide_tx(&self.tx_id, false).await
    }
}