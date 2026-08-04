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
    // ---- Cluster membership change (Raft thesis §6) ----
    //
    // `AddNode` / `RemoveNode` are the *client-facing* membership commands.
    // When the leader receives one, it translates it into a sequence of
    // two `InstallConfiguration` log entries (one `Joint`, then one
    // `Simple`) so the safety properties of joint consensus are
    // preserved even though the client sees a single round-trip.
    //
    // These two variants themselves never appear in the replicated log;
    // they are consumed by the leader's `MembershipCoordinator` and
    // replaced with `InstallConfiguration` entries before append.
    AddNode { server: ServerId },
    RemoveNode { node_id: String },
    /// Internal log entry: install the given configuration. Produced
    /// only by the leader's `MembershipCoordinator`; followers install
    /// it during `apply_logs` / `replay_logs`. The variant is wire-
    /// serializable (prost encodes it via a separate message type)
    /// but never appears as a client-facing command.
    InstallConfiguration { config: Configuration },
}

/// Stable identity of a server in the cluster.
///
/// `node_id` is the logical name (e.g. `"n1"`) used everywhere inside Raft
/// for `peers` lists, match_index keys, and vote bookkeeping. `addr` is
/// the network endpoint used to dial that server's Raft RPC port. Both
/// are required: the `node_id` is what gets committed to the log
/// (machine address changes don't change cluster identity), while `addr`
/// is what the leader uses to dial.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerId {
    pub node_id: String,
    pub addr: String,
}

/// Active cluster configuration. Committed to the log; once a `Configuration`
/// entry is replicated to a quorum (under the *previous* configuration's
/// rules), every server installs it as its new view of the cluster.
///
/// `Joint` is the safety-preserving intermediate state used by Raft thesis
/// §6. During Joint, commits require a majority of **both** `old` and
/// `new`, guaranteeing that any new-majority overlaps with any old-majority.
/// This prevents the disjoint-majorities bug that single-step membership
/// changes allow.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Configuration {
    /// Steady state: a single set of servers; simple majority quorum.
    Simple(Vec<ServerId>),
    /// Transitional state during a single-server membership change.
    /// `old` is the configuration that was active when this entry was
    /// proposed; `new` is the configuration that takes over once the
    /// subsequent `Simple(new)` entry commits.
    Joint { old: Vec<ServerId>, new: Vec<ServerId> },
}

impl Configuration {
    /// All servers that should receive `AppendEntries` in this config.
    /// During `Joint` we replicate to both `old` and `new` so that
    /// leaving servers stay caught up (in case the leader has to roll
    /// back) and joining servers start receiving log entries.
    pub fn all_servers(&self) -> Vec<ServerId> {
        match self {
            Configuration::Simple(s) => s.clone(),
            Configuration::Joint { old, new } => {
                let mut combined: Vec<ServerId> = old.clone();
                for s in new {
                    if !combined.iter().any(|x| x.node_id == s.node_id) {
                        combined.push(s.clone());
                    }
                }
                combined
            }
        }
    }

    /// The "effective" set we use for matching vote-for bookkeeping.
    /// Same as `all_servers` for now; could diverge later if we add
    /// server weighting.
    pub fn effective_servers(&self) -> Vec<ServerId> {
        self.all_servers()
    }

    /// Server count (for `is_single_node` etc.).
    pub fn size(&self) -> usize {
        match self {
            Configuration::Simple(s) => s.len(),
            Configuration::Joint { old, new } => old.len().max(new.len()),
        }
    }

    /// Look up a server's address by node_id (if present).
    pub fn addr_of(&self, node_id: &str) -> Option<String> {
        self.all_servers()
            .iter()
            .find(|s| s.node_id == node_id)
            .map(|s| s.addr.clone())
    }

    /// Check whether `node_id` is a member of this configuration.
    pub fn contains(&self, node_id: &str) -> bool {
        self.all_servers().iter().any(|s| s.node_id == node_id)
    }
}

/// Compute the majority commit quorum for the *current* configuration
/// when committing an entry replicated under the *previous* configuration.
///
/// **Raft thesis §6, Figure 4 / Figure 5.** The quorum rule depends on the
/// configuration that was active when the entry was proposed, not the
/// current configuration. This is the rule that closes the
/// disjoint-majorities bug.
///
/// Returns `true` if `match_index` (per-server highest known replicated
/// index) shows a quorum replicating `index` under `config`.
///
/// - `Simple(servers)`: majority of `servers`.
/// - `Joint { old, new }`: majority of `old` AND majority of `new`.
pub fn config_quorum_reached(
    config: &Configuration,
    match_index: &HashMap<String, u64>,
    self_node_id: &str,
    self_index: u64,
    index: u64,
) -> bool {
    // Single-source-of-truth quorum predicate. Inlined here so we
    // don't infinite-recurse through `config_quorum_reached_index`,
    // which uses this function inside its binary search.
    match config {
        Configuration::Simple(servers) => {
            let mut count = 0usize;
            for s in servers {
                let idx = if s.node_id == self_node_id {
                    self_index
                } else {
                    match_index.get(&s.node_id).copied().unwrap_or(0)
                };
                if idx >= index {
                    count += 1;
                }
            }
            count > servers.len() / 2
        }
        Configuration::Joint { old, new } => {
            let old_ok = {
                let mut count = 0usize;
                for s in old {
                    let idx = if s.node_id == self_node_id {
                        self_index
                    } else {
                        match_index.get(&s.node_id).copied().unwrap_or(0)
                    };
                    if idx >= index {
                        count += 1;
                    }
                }
                count > old.len() / 2
            };
            let new_ok = {
                let mut count = 0usize;
                for s in new {
                    let idx = if s.node_id == self_node_id {
                        self_index
                    } else {
                        match_index.get(&s.node_id).copied().unwrap_or(0)
                    };
                    if idx >= index {
                        count += 1;
                    }
                }
                count > new.len() / 2
            };
            old_ok && new_ok
        }
    }
}

/// Like `config_quorum_reached`, but returns the highest index that
/// satisfies the quorum rule under the given configuration. The leader
/// walks backward from `self_index` and returns the largest index at
/// which a quorum is reached.
///
/// Used by `RaftNode::maybe_commit` to advance `commit_index` to the
/// highest quorum-satisfying entry.
pub fn config_quorum_reached_index(
    config: &Configuration,
    match_index: &HashMap<String, u64>,
    self_node_id: &str,
    self_index: u64,
) -> u64 {
    // Count the number of servers whose match_index (or self_index)
    // is >= `target`. We return the highest target where both
    // majorities (old AND new for Joint) are simultaneously met.
    //
    // Naive O(N log N) implementation: binary-search the largest
    // target satisfying the rule. With small N (typical cluster
    // size 3-7) this is fine.
    let mut lo: u64 = 1;
    let mut hi: u64 = self_index;
    let mut best: u64 = 0;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        if config_quorum_reached(config, match_index, self_node_id, self_index, mid) {
            best = mid;
            if mid == hi {
                break;
            }
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }
    best
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
    pub term: u64,
    pub index: usize,
    pub command: Command,
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

/// P8 PR 6 (Raft thesis §6): errors returned by `propose_add_node`
/// / `propose_remove_node` so the JSON client layer can translate
/// them to a structured response.
#[derive(Debug, Clone, PartialEq)]
pub enum MembershipError {
    /// The local node is not the leader. Client should retry on the
    /// current leader.
    NotLeader,
    /// The given `node_id` is already in the cluster.
    AlreadyMember(String),
    /// The given `node_id` is not in the cluster.
    NotMember(String),
    /// Refusing to remove ourselves; the client should call
    /// `RemoveNode` on a different node to shrink the cluster.
    CannotRemoveSelf,
    /// Refusing to remove the last remaining server, which would
    /// leave the cluster unable to make progress.
    CannotRemoveLastServer,
    /// Storage layer error (WAL append failed).
    StorageError(String),
}