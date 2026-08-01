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
    //
    // As of P6 (see `ROADMAP.md`), the coordinator is the leader and votes
    // travel on a side-channel RPC (`proto/coordination.proto` ->
    // `crate::coordination::VoteRequest`/`VoteResponse`), **not** through the
    // Raft log. The log therefore carries only `BeginTx` and `DecideTx`;
    // `Vote` is no longer a `Command` variant. The internal `Vote` enum
    // below still models an individual peer's decision inside
    // `StateMachine::pending_txs`, but it is populated out-of-band.
    BeginTx { tx_id: String, ops: Vec<TxOp> },
    DecideTx { tx_id: String, decision: TxDecision },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum TxOp {
    Put { key: String, value: String },
    Delete { key: String },
}

/// One peer's decision on a pending tx. Used internally by
/// `StateMachine` to track votes received via the side-channel RPC;
/// never written to the Raft log.
///
/// `Vote` was previously also a `Command` variant and showed up in WAL /
/// JSON client payloads. P6 removes the log-side variant but keeps the
/// in-memory representation: the state machine still needs to count Yes
/// vs No to drive `DecideTx`.
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