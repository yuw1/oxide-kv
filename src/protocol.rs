use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Command {
    Set { key: String, value: String },
    Get { key: String },
    Delete { key: String },
    Compact,
    // ---- Two-phase commit lifecycle (Raft thesis §6.4, simplified) ----
    BeginTx { tx_id: String, ops: Vec<TxOp> },
    Vote { tx_id: String, voter: String, vote: Vote },
    DecideTx { tx_id: String, decision: TxDecision },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum TxOp {
    Put { key: String, value: String },
    Delete { key: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Vote {
    Yes,
    No(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum TxDecision {
    Commit,
    Abort,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub(crate) term: u64,
    pub index: usize,
    pub(crate) command: Command,
}

/// A serialized state-machine snapshot at a known log position.
///
/// `last_included_index` / `last_included_term` identify the log entry whose
/// effect is fully captured by `data`. All log entries at indices
/// `<= last_included_index` can be discarded after the snapshot is installed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub last_included_index: u64,
    pub last_included_term: u64,
    pub data: HashMap<String, String>,
}

/// Token returned by `RaftNode::begin_read` and consumed by `confirm_read`.
///
/// `index` is the log position the read is anchored to; the read is safe to
/// serve once the leader has confirmed it remained leader at a time `>= issued_at`
/// and the state machine has applied all entries up to `index`.
///
/// Not serializable on purpose: it's an in-process token with a monotonic timestamp.
#[derive(Debug, Clone, Copy)]
pub struct ReadIndex {
    pub index: u64,
    pub issued_at: Instant,
}