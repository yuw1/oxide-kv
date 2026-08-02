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
//! commands (`Set`, `Delete`) that get committed. The
//! reference model doesn't model 2PC (`BeginTx` / `DecideTx`)
//! directly — the integration tests in
//! `tests/integration_2pc.rs` already exercise that path end
//! to end — and it ignores `Compact` (which is a Raft-internal
//! marker, not a state-machine mutation).
//!
//! The cross-check is: after every `submit_set` /
//! `submit_delete`, the cluster's `commit_index` advances by
//! 1 on a quorum. The reference model applies that entry in
//! order. Every `cluster.read(node, key)` should return the
//! same value the reference model would produce *at the same
//! committed-index prefix*. If not, the cluster is serving a
//! stale or anomalous state.
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

use crate::protocol::Command;

use super::sim_harness::SimCluster;

/// Sequential reference model. Single-threaded HashMap that
/// applies committed `Set` / `Delete` commands in log-index
/// order.
#[derive(Debug, Default, Clone)]
pub struct ReferenceModel {
    state: HashMap<String, String>,
    /// The log index of the *next* entry to apply.
    /// Raft log indices start at 1 (per the thesis), so
    /// a freshly-created model has `next_index = 1`.
    next_index: u64,
}

impl ReferenceModel {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
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
            // 2PC ops: out of scope for the KV
            // linearizability check. The integration
            // tests cover them.
            Command::BeginTx { .. } | Command::DecideTx { .. } => {}
            // Get: a read op that shouldn't appear in the
            // log (it's handled by the client layer). If
            // we see one, ignore it — the production
            // client never writes Get to the log.
            Command::Get { .. } => {}
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
}