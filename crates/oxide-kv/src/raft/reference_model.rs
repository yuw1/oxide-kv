//! Sequential reference model for cross-checking the Raft
//! cluster against a deterministic, fault-free oracle.
//!
//! # Purpose
//!
//! The simulated Raft cluster has many fault-injection knobs
//! (drop, delay, reorder, duplicate, partition, restart) and
//! runs concurrently across 3 nodes. To catch *safety*
//! violations — not just invariant violations — we cross-check
//! the cluster's reads against a sequential reference model
//! that runs the *exact same op sequence* in a single-threaded
//! HashMap.
//!
//! # What it covers
//!
//! Raft guarantees linearizability for the state-machine
//! commands (`Set`, `Delete`) that get committed, and 2PC
//! atomicity for transactions (`BeginTx` / `DecideTx`). The
//! reference model models both: plain KV ops apply immediately,
//! while a transaction's ops stay staged (invisible) until a
//! matching `DecideTx(Commit)` applies them atomically — an
//! `Abort` discards them. `Compact` is ignored (a Raft-internal
//! marker, not a state-machine mutation).
//!
//! The cross-check is: after every `submit_set` /
//! `submit_delete` / `submit_tx`, the cluster's `commit_index`
//! advances on a quorum. The reference model applies each
//! committed entry in order. Every `cluster.read(node, key)`
//! should return the same value the reference model would
//! produce *at the same committed-index prefix*. If not, the
//! cluster is serving a stale or anomalous state.
//!
//! # What it does NOT cover
//!
//! - **Liveness / latency**: the reference model is
//!   synchronous and never fails, so it can't catch "Get took
//!   too long". That's the invariant checker's job (e.g.
//!   `ReadIndex` consistency) or a separate timeout budget.
//! - **Idempotency under `Duplicate`**: the reference model
//!   applies each entry exactly once. The cluster might
//!   observe the same op twice (duplicate RPC) and that's
//!   fine as long as the state machine is idempotent — which
//!   Raft's log-replay guarantees.
//! - **Snapshot / log-compaction correctness**: out of scope
//!   for P7. The integration tests in P1 / P4 exercise the
//!   snapshot path.
//!
//! # API
//!
//! ```ignore
//! let mut rm = ReferenceModel::new();
//! // ... run cluster scenarios ...
//! rm.apply_committed_entry(&cluster, log_index);
//! let value = rm.get(key);   // reference model's view
//! // Compare with `cluster.read(node, key)`.
//! ```

use std::collections::HashMap;

use crate::protocol::{Command, TxDecision, TxOp};

use super::sim_harness::SimCluster;

/// Sequential reference model. Single-threaded HashMap that
/// applies committed `Set` / `Delete` commands in log-index
/// order.
///
/// # 2PC modelling
///
/// The model also tracks two-phase-commit transactions so the
/// fuzz harness can cross-check committed tx effects. A
/// `BeginTx` stages its ops in `pending` (invisible to reads);
/// a `DecideTx(Commit)` applies them atomically; a
/// `DecideTx(Abort)` discards them. A `DecideTx` for an
/// unknown `tx_id` is a no-op (the coordinator may abort a tx
/// whose `BeginTx` never committed, e.g. after a leader
/// step-down). This mirrors `StateMachine::begin_tx` /
/// `decide_tx` exactly, so the model and the real state
/// machine agree on every committed prefix.
#[derive(Debug, Default, Clone)]
pub struct ReferenceModel {
    state: HashMap<String, String>,
    /// Staged-but-undecided 2PC transactions, keyed by tx_id.
    /// Populated by `BeginTx`, drained by `DecideTx`.
    pending: HashMap<String, Vec<TxOp>>,
    /// The log index of the *next* entry to apply.
    /// Raft log indices start at 1 (per the thesis), so
    /// a freshly-created model has `next_index = 1`.
    next_index: u64,
}

impl ReferenceModel {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
            pending: HashMap::new(),
            next_index: 1,
        }
    }

    /// Apply a single committed log entry from the cluster
    /// to the reference model.
    ///
    /// `index` is the cluster's log index for this entry.
    /// Entries are expected to be applied in strictly
    /// ascending order; out-of-order or duplicate calls are
    /// silently ignored (they would be a no-op against a
    /// log that's already at this index).
    ///
    /// Returns `true` if the entry was applied; `false` if
    /// it was skipped (already applied or out of order).
    pub fn apply(&mut self, index: u64, command: &Command) -> bool {
        if index < self.next_index {
            // Already applied (or this is a duplicate call
            // with a stale index). Skip.
            return false;
        }
        if index > self.next_index {
            // Gap. The caller should drain the log in order.
            // We surface this as `false` so the test can
            // decide whether to retry once more entries
            // have committed.
            return false;
        }
        self.next_index += 1;
        match command {
            Command::Set { key, value } => {
                self.state.insert(key.clone(), value.clone());
            }
            Command::Delete { key } => {
                self.state.remove(key);
            }
            // Compact: Raft-internal marker, no
            // state-machine effect.
            Command::Compact => {}
            // 2PC: stage ops on BeginTx; apply or discard
            // atomically on DecideTx. Mirrors
            // `StateMachine::begin_tx` / `decide_tx` so the
            // model agrees with the real state machine on
            // every committed prefix.
            Command::BeginTx { tx_id, ops } => {
                self.pending.insert(tx_id.clone(), ops.clone());
            }
            Command::DecideTx { tx_id, decision } => {
                if let Some(ops) = self.pending.remove(tx_id)
                    && matches!(decision, TxDecision::Commit)
                {
                    for op in ops {
                        match op {
                            TxOp::Put { key, value } => {
                                self.state.insert(key, value);
                            }
                            TxOp::Delete { key } => {
                                self.state.remove(&key);
                            }
                        }
                    }
                }
                // Abort (or unknown tx_id): dropping the staged
                // ops is the whole effect.
            }
            // Get: a read op that shouldn't appear in the
            // log (it's handled by the client layer). If
            // we see one, ignore it — the production
            // client never writes Get to the log.
            Command::Get { .. } => {}
            // `AddNode` / `RemoveNode`: client-facing
            // membership commands. The leader replaces them
            // with `Configuration` log entries before
            // replication, so the reference model never sees
            // them either. If it does, treat as no-op (the
            // reference model only needs to agree with the
            // state-machine on observable effects; membership
            // is in `Configuration`).
            Command::AddNode { .. } | Command::RemoveNode { .. } => {}
            // `InstallConfiguration`: leader-internal log
            // entry. The reference model doesn't model
            // membership state (the state machine doesn't
            // care), so this is a no-op for the model. The
            // invariants layer checks membership separately.
            Command::InstallConfiguration { .. } => {}
            // P8 PR 7: admin-driven abort. Like
            // AddNode/RemoveNode, the leader intercepts and
            // translates to `DecideTx(Abort)` before
            // replication. The reference model never sees
            // an AbortTx entry on the wire; treat as
            // no-op for the same reason as the membership
            // commands above.
            Command::AbortTx { .. } => {}
        }
        true
    }

    /// Apply every committed entry up to and including
    /// `commit_index` for the cluster's current leader.
    /// Returns the number of entries applied (excluding
    /// skips).
    ///
    /// Convenience wrapper that the test harness calls
    /// after every `wait_for_replication`. Drains the
    /// leader's log from `next_index` up to
    /// `commit_index`, applying each entry in order.
    pub fn drain_to(&mut self, cluster: &SimCluster, commit_index: u64) -> usize {
        let leader_idx = match cluster.leader_index() {
            Some(idx) => idx,
            None => return 0,
        };
        let mut applied = 0;
        while self.next_index <= commit_index {
            let entry = {
                let node = cluster.nodes[leader_idx].raft.read().unwrap();
                match node.get_log_entry(self.next_index) {
                    Some(e) => e,
                    None => return applied,
                }
            };
            if self.apply(self.next_index, &entry.command) {
                applied += 1;
            } else {
                break;
            }
        }
        applied
    }

    /// Reference model's view of `key`. Returns `None` for
    /// missing keys.
    pub fn get(&self, key: &str) -> Option<&String> {
        self.state.get(key)
    }

    /// Snapshot the reference model's full state. Useful
    /// for diagnostic dumps in cross-check failures.
    pub fn snapshot(&self) -> &HashMap<String, String> {
        &self.state
    }

    /// Highest log index the reference model has applied.
    pub fn applied_index(&self) -> u64 {
        self.next_index.saturating_sub(1)
    }

    /// Reset the reference model to a fresh state. Used
    /// to recompute the model from a fresh leader's log
    /// at cross-check time — guards against the
    /// intermediate-drain race where the cluster's
    /// leader changes mid-scenario and the reference
    /// model accumulates ops that the new leader's log
    /// no longer reflects.
    pub fn reset(&mut self) {
        self.state.clear();
        self.pending.clear();
        self.next_index = 1;
    }

    /// Replay the leader's log from index 1 up to
    /// `commit_index`, applying each entry in order.
    /// This recomputes the reference model from scratch
    /// (after a `reset()`) — useful at the end of a
    /// fuzz scenario to ensure the reference model is
    /// in sync with whatever the current leader
    /// considers committed, regardless of any
    /// intermediate drains that may have used a
    /// previous leader's log.
    pub fn replay_from_leader(
        &mut self,
        cluster: &SimCluster,
        commit_index: u64,
    ) -> usize {
        let leader_idx = match cluster.leader_index() {
            Some(idx) => idx,
            None => {
                self.reset();
                return 0;
            }
        };
        self.replay_from_node(cluster, leader_idx, commit_index)
    }

    /// Replay `cluster.nodes[leader_idx].log` from index 1
    /// up to `commit_index`, applying each entry. Caller
    /// picks the leader explicitly — useful when the
    /// caller wants to skip a killed node that still has
    /// state=Leader (its serve loop is stopped, so it
    /// can't actually replicate, but `cluster.leader_index`
    /// might still return it).
    pub fn replay_from_node(
        &mut self,
        cluster: &SimCluster,
        leader_idx: usize,
        commit_index: u64,
    ) -> usize {
        self.reset();
        let mut applied = 0;
        while self.next_index <= commit_index {
            let entry = {
                let node = cluster.nodes[leader_idx].raft.read().unwrap();
                match node.get_log_entry(self.next_index) {
                    Some(e) => e,
                    None => return applied,
                }
            };
            if self.apply(self.next_index, &entry.command) {
                applied += 1;
            } else {
                break;
            }
        }
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Command;

    #[test]
    fn reference_model_set_then_get() {
        let mut rm = ReferenceModel::new();
        assert!(rm.apply(1, &Command::Set { key: "k".into(), value: "v".into() }));
        assert_eq!(rm.get("k"), Some(&"v".to_string()));
        assert_eq!(rm.applied_index(), 1);
    }

    #[test]
    fn reference_model_delete_removes_key() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "k".into(), value: "v".into() });
        rm.apply(2, &Command::Delete { key: "k".into() });
        assert_eq!(rm.get("k"), None);
        assert_eq!(rm.applied_index(), 2);
    }

    #[test]
    fn reference_model_out_of_order_apply_returns_false() {
        let mut rm = ReferenceModel::new();
        // Index 3 before index 1 — gap, returns false.
        assert!(!rm.apply(3, &Command::Set { key: "k".into(), value: "v".into() }));
        assert_eq!(rm.applied_index(), 0);
        // Now apply in order.
        assert!(rm.apply(1, &Command::Set { key: "k".into(), value: "v".into() }));
        assert_eq!(rm.applied_index(), 1);
    }

    #[test]
    fn reference_model_stale_index_skipped() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "k".into(), value: "v".into() });
        // Re-applying index 1 is a no-op.
        assert!(!rm.apply(1, &Command::Set { key: "k".into(), value: "other".into() }));
        assert_eq!(rm.get("k"), Some(&"v".to_string()));
    }

    #[test]
    fn reference_model_compact_is_no_op() {
        let mut rm = ReferenceModel::new();
        assert!(rm.apply(1, &Command::Compact));
        assert_eq!(rm.applied_index(), 1);
        assert_eq!(rm.snapshot().len(), 0);
    }

    #[test]
    fn reference_model_overwrite_takes_latest() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "k".into(), value: "v1".into() });
        rm.apply(2, &Command::Set { key: "k".into(), value: "v2".into() });
        rm.apply(3, &Command::Set { key: "k".into(), value: "v3".into() });
        assert_eq!(rm.get("k"), Some(&"v3".to_string()));
    }

    #[test]
    fn reference_model_snapshot_returns_full_state() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "a".into(), value: "1".into() });
        rm.apply(2, &Command::Set { key: "b".into(), value: "2".into() });
        let snap = rm.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.get("a"), Some(&"1".to_string()));
        assert_eq!(snap.get("b"), Some(&"2".to_string()));
    }

    // =================== 2PC modelling ===================

    #[test]
    fn reference_model_begin_tx_stages_ops_invisibly() {
        let mut rm = ReferenceModel::new();
        // BeginTx stages ops but they must NOT be readable yet.
        assert!(rm.apply(
            1,
            &Command::BeginTx {
                tx_id: "t1".into(),
                ops: vec![TxOp::Put { key: "a".into(), value: "1".into() }],
            }
        ));
        assert_eq!(rm.get("a"), None, "staged ops must be invisible pre-commit");
        assert_eq!(rm.applied_index(), 1);
    }

    #[test]
    fn reference_model_decide_commit_applies_atomically() {
        let mut rm = ReferenceModel::new();
        rm.apply(
            1,
            &Command::BeginTx {
                tx_id: "t1".into(),
                ops: vec![
                    TxOp::Put { key: "a".into(), value: "1".into() },
                    TxOp::Put { key: "b".into(), value: "2".into() },
                ],
            },
        );
        assert!(rm.apply(
            2,
            &Command::DecideTx { tx_id: "t1".into(), decision: TxDecision::Commit }
        ));
        assert_eq!(rm.get("a"), Some(&"1".to_string()));
        assert_eq!(rm.get("b"), Some(&"2".to_string()));
    }

    #[test]
    fn reference_model_decide_abort_discards_ops() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "a".into(), value: "pre".into() });
        rm.apply(
            2,
            &Command::BeginTx {
                tx_id: "t1".into(),
                ops: vec![TxOp::Put { key: "a".into(), value: "new".into() }],
            },
        );
        assert!(rm.apply(
            3,
            &Command::DecideTx { tx_id: "t1".into(), decision: TxDecision::Abort }
        ));
        // Abort must leave the pre-tx value intact.
        assert_eq!(rm.get("a"), Some(&"pre".to_string()));
    }

    #[test]
    fn reference_model_decide_delete_op_applies() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "a".into(), value: "1".into() });
        rm.apply(
            2,
            &Command::BeginTx {
                tx_id: "t1".into(),
                ops: vec![TxOp::Delete { key: "a".into() }],
            },
        );
        rm.apply(
            3,
            &Command::DecideTx { tx_id: "t1".into(), decision: TxDecision::Commit },
        );
        assert_eq!(rm.get("a"), None);
    }

    #[test]
    fn reference_model_decide_unknown_tx_is_no_op() {
        let mut rm = ReferenceModel::new();
        rm.apply(1, &Command::Set { key: "a".into(), value: "1".into() });
        // DecideTx for a tx that was never begun: no-op, no panic.
        assert!(rm.apply(
            2,
            &Command::DecideTx { tx_id: "ghost".into(), decision: TxDecision::Commit }
        ));
        assert_eq!(rm.get("a"), Some(&"1".to_string()));
        assert_eq!(rm.applied_index(), 2);
    }

    #[test]
    fn reference_model_reset_clears_pending_txs() {
        let mut rm = ReferenceModel::new();
        rm.apply(
            1,
            &Command::BeginTx {
                tx_id: "t1".into(),
                ops: vec![TxOp::Put { key: "a".into(), value: "1".into() }],
            },
        );
        rm.reset();
        // After reset, a DecideTx for the old tx must be a no-op
        // (the staged ops are gone), not a resurrection.
        rm.apply(
            1,
            &Command::DecideTx { tx_id: "t1".into(), decision: TxDecision::Commit },
        );
        assert_eq!(rm.get("a"), None);
    }
}