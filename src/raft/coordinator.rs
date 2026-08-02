//! 2PC coordinator orchestration on the Raft leader (P6 PR #13).
//!
//! Responsibilities:
//!   - Detect single-node vs multi-node cluster membership.
//!   - Single-node fast path: propose `BeginTx` + `DecideTx(Commit)` as one
//!     batch so the existing single-node behavior is unchanged.
//!   - Multi-node path: propose `BeginTx` only, wait for it to commit and
//!     be applied on the local state machine, then broadcast a
//!     `VoteRequest` to every peer over the multiplexed transport (see
//!     `crate::raft::rpc::RpcClient::send_tx_vote_rpc` and
//!     `crate::raft::transport`). Collect replies, apply the **all-yes**
//!     quorum policy (textbook 2PC), then propose `DecideTx(Commit)` or
//!     `DecideTx(Abort)` accordingly.
//!
//! Why a separate module:
//!   - Keeps the multi-node coordination state machine (vote collection,
//!     timeout, term-advance step-down) out of `client.rs` and `node.rs`,
//!     which already have their own concerns.
//!   - Mirrors the structure of `crate::raft::transport` (P6 PR #12) and
//!     `crate::raft::rpc`.
//!
//! Locked decisions (see `ROADMAP.md` P6):
//!   - Coordinator = Raft leader (no separate election).
//!   - Vote transport = side-channel RPC (multiplexed onto the Raft port).
//!   - Quorum = all-yes required. Any No / timeout / error aborts.
//!   - Failure recovery = coordinator-only for P6; participant-side
//!     autonomous abort is deferred.
//!
//! Out of scope (deferred to PR #14 or later):
//!   - 3-node integration test against a running cluster.
//!   - Participant-side recovery on coordinator crash (TODO: log-and-resume
//!     on leader step-up so abandoned BeginTx entries can be re-voted).

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::timeout;

use crate::coordination::{VoteRequest, VoteResponse};
use crate::protocol::{Command, TxDecision, Vote};
use crate::raft::node::{NodeState, RaftNode};
use crate::raft::rpc::RpcClient;

/// Outcome of a coordinator-driven 2PC round.
///
/// Returned by `coordinate_tx`. The client path in `client.rs` translates
/// this into a JSON response for the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOutcome {
    /// The transaction committed on a quorum of nodes and the ops are
    /// applied. Carries the log index of the matching `DecideTx`.
    Committed {
        begin_index: u64,
        decide_index: u64,
        tx_id: String,
    },
    /// The transaction was aborted (any peer returned No, timed out, or
    /// returned a higher term causing the leader to step down).
    Aborted {
        tx_id: String,
        reason: String,
    },
    /// The node is no longer the leader (stepped down during the round)
    /// and the transaction cannot be completed on this connection.
    NotLeader { tx_id: String },
}

/// Maximum time to wait for an individual peer's `VoteResponse`.
///
/// Generous relative to the per-RPC timeouts (1–1.5 s for Raft RPCs) so
/// that a peer under transient load doesn't get falsely treated as a No.
/// The total round is bounded by `peers.len() * TX_VOTE_TIMEOUT_MS`
/// (joins are concurrent, so the wall-clock bound is one timeout).
const TX_VOTE_TIMEOUT_MS: u64 = 2_000;

/// Wall-clock bound for the entire coordinator round (BeginTx commit +
/// vote fan-out + DecideTx commit).
const COORDINATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Drive a 2PC transaction to completion on the leader.
///
/// This is the entry point called by `client.rs::begin_tx` after the
/// command has been routed to the leader. The function blocks until the
/// transaction is committed, aborted, or the leader steps down.
///
/// # Errors / failure modes
///
/// - **Single-node fast path** (`peers.is_empty()`): proposes
///   `BeginTx` + `DecideTx(Commit)` as one batch, returns `Committed`.
/// - **Multi-node happy path**: all peers respond Yes → proposes
///   `DecideTx(Commit)`, returns `Committed`.
/// - **Multi-node No / timeout**: any peer returns No, errors, or
///   times out → proposes `DecideTx(Abort)`, returns `Aborted`.
/// - **Term advance**: a peer returns a higher term → leader steps
///   down to Follower, returns `NotLeader`. The transaction is left
///   in `pending_txs` and the new leader will need to re-broadcast
///   (out of scope for P6 — see ROADMAP.md).
pub async fn coordinate_tx(
    node_arc: Arc<RwLock<RaftNode>>,
    tx_id: String,
    ops: Vec<crate::protocol::TxOp>,
) -> TxOutcome {
    match timeout(
        COORDINATE_TIMEOUT,
        coordinate_tx_inner(node_arc.clone(), tx_id.clone(), ops),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => TxOutcome::Aborted {
            tx_id,
            reason: format!(
                "coordinator round exceeded {}s wall-clock bound",
                COORDINATE_TIMEOUT.as_secs()
            ),
        },
    }
}

async fn coordinate_tx_inner(
    node_arc: Arc<RwLock<RaftNode>>,
    tx_id: String,
    ops: Vec<crate::protocol::TxOp>,
) -> TxOutcome {
    // ---- Step 0: single-node fast path ---------------------------------
    //
    // Mirrors the pre-PR-#13 behavior so a single-node cluster (which
    // has no quorum to vote with) keeps working unchanged. A no-peers
    // leader auto-commits every transaction it proposes.
    let is_single_node = {
        let n = node_arc.read().unwrap();
        n.is_single_node()
    };
    if is_single_node {
        return single_node_fast_path(node_arc, tx_id, ops).await;
    }

    // ---- Step 1: propose BeginTx only, wait for commit + apply ----------
    //
    // We need the BeginTx entry to be replicated to every peer BEFORE we
    // send VoteRequest, otherwise the peer's `handle_tx_vote_request`
    // will reject the vote with "tx not pending" (see node.rs PR #12
    // step 4). Replicating through `propose_batch` + `sync_logs` and
    // waiting for `last_applied >= begin_index` on the leader guarantees
    // the entry is in every peer's log once their next AppendEntries
    // round completes (AppendEntries + apply_logs is what populates
    // `pending_txs` on the follower).
    let begin_index = match propose_and_wait_for_apply(
        &node_arc,
        Command::BeginTx {
            tx_id: tx_id.clone(),
            ops,
        },
    )
    .await
    {
        Ok(idx) => idx,
        Err(_reason) => return TxOutcome::NotLeader { tx_id },
    };

    // Step 1b: wait for the BeginTx entry to be replicated to every
    // peer. `match_index[peer] >= begin_index` means the peer has
    // acknowledged AppendEntries for the entry — once that happens the
    // entry is in the peer's log and the next `apply_logs` round on
    // the peer will populate its `pending_txs` table. Without this
    // gate, peers would reply "tx not pending" to the vote RPC and
    // the coordinator would spuriously abort (see PR #14 integration
    // test for the regression).
    if let Err(reason) = wait_for_replication(&node_arc, begin_index).await {
        return TxOutcome::Aborted {
            tx_id,
            reason: format!("replication failed: {}", reason),
        };
    }
    // Snapshot the leader's term and peer set BEFORE fanning out: any
    // term advance reported by a peer needs to be compared against this
    // snapshot to decide whether to step down.
    let (leader_term, peer_addrs, leader_id, begin_log_term) = {
        let n = node_arc.read().unwrap();
        let begin_term = n
            .get_log_entry(begin_index)
            .map(|e| e.term)
            .unwrap_or_else(|| n.current_term());
        (n.current_term(), n.peers().to_vec(), n.node_id().to_string(), begin_term)
    };

    // ---- Step 2: record the leader's implicit Yes -------------------------
    //
    // The leader votes Yes by committing BeginTx. Recording it explicitly
    // keeps `pending_txs[tx_id].votes` consistent with peer-side state
    // and gives `pending_tx_view` a complete picture for diagnostics.
    {
        let n = node_arc.write().unwrap();
        let _ = n
            .state_machine
            .write()
            .unwrap()
            .record_vote(&tx_id, leader_id.clone(), Vote::Yes);
    }

    // ---- Step 3: fan-out VoteRequest to every peer ----------------------
    let req = VoteRequest {
        term: leader_term,
        tx_id: tx_id.clone(),
        last_log_index: begin_index,
        last_log_term: begin_log_term,
    };

    // Concurrent fan-out via tokio::spawn. Each peer gets its own task so
    // a single slow peer does not stretch the round beyond its own RPC
    // timeout. We collect into a Vec<(peer_addr, result)> preserving the
    // (addr, result) shape the rest of the function expects.
    let mut handles = Vec::with_capacity(peer_addrs.len());
    for addr in &peer_addrs {
        let addr = addr.clone();
        let req = req.clone();
        handles.push(tokio::spawn(async move {
            let result = RpcClient::send_tx_vote_rpc(
                &addr,
                req,
                Duration::from_millis(TX_VOTE_TIMEOUT_MS),
            )
            .await;
            (addr, result)
        }));
    }
    let mut vote_results: Vec<(String, anyhow::Result<VoteResponse>)> =
        Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(pair) => vote_results.push(pair),
            Err(e) => {
                // JoinError only fires if the task panicked. Treat as a
                // No-equivalent RPC failure so the coordinator aborts.
                vote_results.push((
                    "<unknown peer>".to_string(),
                    Err(anyhow::anyhow!("vote task panicked: {}", e)),
                ));
            }
        }
    }

    // ---- Step 4: tally votes ------------------------------------------
    //
    // all-yes required (textbook 2PC). Any single No / timeout / error /
    // term advance → Abort.
    let mut expected_voters: HashSet<String> = peer_addrs.iter().cloned().collect();
    expected_voters.insert(leader_id.clone());

    let mut yes_voters: HashSet<String> = HashSet::new();
    yes_voters.insert(leader_id.clone());
    let mut abort_reason: Option<String> = None;

    for (addr, result) in vote_results {
        match result {
            Ok(resp) => {
                // Term advance check: a peer in a higher term means the
                // leader is partitioned. Step down and abort.
                if resp.term > leader_term {
                    let mut n = node_arc.write().unwrap();
                    if n.state == NodeState::Leader {
                        n.current_term = resp.term;
                        n.state = NodeState::Follower;
                        n.vote_for = None;
                        let _ = n.storage.save_meta(n.current_term, n.vote_for.clone());
                    }
                    return TxOutcome::NotLeader { tx_id };
                }
                if resp.vote_granted {
                    yes_voters.insert(addr.clone());
                    // Record the peer's Yes on the state machine so
                    // `pending_tx_view` is complete.
                    let n = node_arc.write().unwrap();
                    let _ = n
                        .state_machine
                        .write()
                        .unwrap()
                        .record_vote(&tx_id, addr.clone(), Vote::Yes);
                } else {
                    let reason = if resp.reason.is_empty() {
                        format!("peer {} declined vote", addr)
                    } else {
                        format!("peer {} declined vote: {}", addr, resp.reason)
                    };
                    abort_reason = Some(reason);
                }
            }
            Err(e) => {
                abort_reason = Some(format!(
                    "peer {} vote RPC failed: {} (treated as No)",
                    addr, e
                ));
            }
        }
    }

    // ---- Step 5: decide and propose DecideTx ----------------------------
    let decision = if abort_reason.is_some() || yes_voters != expected_voters {
        TxDecision::Abort
    } else {
        TxDecision::Commit
    };

    let decide_index = match propose_and_wait_for_apply(
        &node_arc,
        Command::DecideTx {
            tx_id: tx_id.clone(),
            decision: decision.clone(),
        },
    )
    .await
    {
        Ok(idx) => idx,
        Err(_reason) => {
            // The leader stepped down between BeginTx commit and DecideTx
            // commit. The transaction is still in `pending_txs` on every
            // node; the new leader will see it on the next replay. For
            // P6 we report NotLeader and leave recovery to the next leader.
            return TxOutcome::NotLeader { tx_id };
        }
    };

    if decision == TxDecision::Commit {
        TxOutcome::Committed {
            begin_index,
            decide_index,
            tx_id,
        }
    } else {
        TxOutcome::Aborted {
            tx_id,
            reason: abort_reason.unwrap_or_else(|| "incomplete vote set".to_string()),
        }
    }
}

/// Single-node fast path: propose `BeginTx` + `DecideTx(Commit)` as one
/// batch, wait for the batch to commit and be applied, return Committed.
async fn single_node_fast_path(
    node_arc: Arc<RwLock<RaftNode>>,
    tx_id: String,
    ops: Vec<crate::protocol::TxOp>,
) -> TxOutcome {
    let begin_index = match propose_batch_and_wait_for_apply(
        &node_arc,
        vec![
            Command::BeginTx {
                tx_id: tx_id.clone(),
                ops,
            },
            Command::DecideTx {
                tx_id: tx_id.clone(),
                decision: TxDecision::Commit,
            },
        ],
    )
    .await
    {
        Ok(begin_idx) => begin_idx,
        Err(_reason) => return TxOutcome::NotLeader { tx_id },
    };
    // In single-node mode both entries commit together; the DecideTx is
    // the entry right after BeginTx.
    TxOutcome::Committed {
        begin_index,
        decide_index: begin_index + 1,
        tx_id,
    }
}

/// Propose a single command and wait until it is both committed and
/// applied (`last_applied >= index`).
///
/// Returns the log index of the proposed entry. Returns `Err(reason)`
/// if the node is no longer leader at any point during the wait, or
/// if the wall-clock timeout elapses.
async fn propose_and_wait_for_apply(
    node_arc: &Arc<RwLock<RaftNode>>,
    command: Command,
) -> Result<u64, String> {
    let index = {
        let mut n = node_arc.write().unwrap();
        if n.state != NodeState::Leader {
            return Err("not leader at propose time".to_string());
        }
        let ok = n.propose(command);
        if !ok {
            return Err("propose rejected".to_string());
        }
        n.log.len() as u64
    };

    // Trigger replication.
    RaftNode::sync_logs(node_arc.clone());

    // Poll until commit_index and last_applied catch up.
    let start = std::time::Instant::now();
    let bound = Duration::from_secs(5);
    loop {
        let (state, commit_idx, last_applied, is_leader) = {
            let n = node_arc.read().unwrap();
            (
                n.state,
                n.commit_index,
                n.last_applied,
                n.state == NodeState::Leader,
            )
        };
        if !is_leader {
            return Err(format!("stepped down to {:?} mid-commit", state));
        }
        if commit_idx >= index && last_applied >= index {
            return Ok(index);
        }
        if start.elapsed() > bound {
            return Err(format!(
                "timed out waiting for index {} to apply (commit={}, applied={})",
                index, commit_idx, last_applied
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until every peer has acknowledged replication of `index` via
/// a successful AppendEntries reply (i.e. `match_index[peer] >= index`).
///
/// This is the gate between `BeginTx` commit on the leader and the
/// vote fan-out: in a multi-node cluster the BeginTx entry must be on
/// every peer's log AND applied to populate `pending_txs` BEFORE the
/// peer can answer `VoteRequest` meaningfully. Otherwise peers reply
/// "tx not pending" and the coordinator aborts a transaction that would
/// otherwise have committed (PR #14 caught this race with the
/// 3-node integration test).
///
/// `match_index` is updated on the **leader** when AppendEntries
/// succeeds (Raft §5.3). After `match_index >= index` the entry is
/// already in the peer's log, and the next `apply_logs` round on the
/// peer will populate `pending_txs`. We then wait for
/// `last_applied >= index` on the leader (which only advances once
/// the entry has been applied locally) — but note that `last_applied`
/// on the leader is only about the leader's own state machine, not
/// the peers. The actual cross-node guarantee relies on the leader
/// having replication proof (match_index) AND the entry being safe
/// to apply (no `prev_log` mismatch in subsequent AppendEntries).
///
/// Returns `Ok(())` on success, `Err(reason)` on timeout or step-down.
async fn wait_for_replication(
    node_arc: &Arc<RwLock<RaftNode>>,
    index: u64,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    let bound = Duration::from_secs(5);
    loop {
        let snapshot = {
            let n = node_arc.read().unwrap();
            if n.state != NodeState::Leader {
                return Err(format!("stepped down to {:?} during replication wait", n.state));
            }
            n.peers()
                .iter()
                .map(|p| (p.clone(), n.match_index_for(p)))
                .collect::<Vec<(String, u64)>>()
        };
        if snapshot.iter().all(|(_, mi)| *mi >= index) {
            return Ok(());
        }
        if start.elapsed() > bound {
            return Err(format!(
                "timed out waiting for index {} to replicate to all peers (current: {:?})",
                index, snapshot
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Like `propose_and_wait_for_apply` but for a batch. Returns the index
/// of the first entry in the batch. All batch entries are contiguous in
/// the log and commit together (caller invariant).
async fn propose_batch_and_wait_for_apply(
    node_arc: &Arc<RwLock<RaftNode>>,
    commands: Vec<Command>,
) -> Result<u64, String> {
    let batch_len = commands.len() as u64;
    let first_index = {
        let mut n = node_arc.write().unwrap();
        if n.state != NodeState::Leader {
            return Err("not leader at propose time".to_string());
        }
        let ok = n.propose_batch(commands);
        if !ok {
            return Err("propose_batch rejected".to_string());
        }
        // After appending N entries, log.len() points to the last entry;
        // the first entry of the batch is log.len() - N + 1.
        n.log.len() as u64 - batch_len + 1
    };

    RaftNode::sync_logs(node_arc.clone());

    let start = std::time::Instant::now();
    let bound = Duration::from_secs(5);
    loop {
        let (state, commit_idx, last_applied, is_leader) = {
            let n = node_arc.read().unwrap();
            (
                n.state,
                n.commit_index,
                n.last_applied,
                n.state == NodeState::Leader,
            )
        };
        if !is_leader {
            return Err(format!("stepped down to {:?} mid-commit", state));
        }
        if commit_idx >= first_index && last_applied >= first_index {
            return Ok(first_index);
        }
        if start.elapsed() > bound {
            return Err(format!(
                "timed out waiting for batch first index {} to apply (commit={}, applied={})",
                first_index, commit_idx, last_applied
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// =========================================================================
// Tests
// =========================================================================
//
// PR #13 is a behavior-heavy PR (multi-node orchestration), but the only
// reliable end-to-end test for it is a 3-node integration test, which is
// deferred to PR #14. PR #13's unit tests therefore focus on:
//
//   1. **Single-node fast path** — `coordinate_tx` on a no-peers leader
//      produces a Committed outcome with the expected indices.
//   2. **apply_logs fix** — BeginTx / DecideTx entries applied through
//      `apply_logs` (not just `replay_logs`) actually populate
//      `pending_txs` and apply the ops on Commit.
//   3. **TxOutcome enum** — equality and Debug formatting (smoke).
//
// The vote-fan-out logic itself is exercised end-to-end by PR #14 against
// a real 3-node cluster.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{TxDecision, TxOp};
    use crate::raft::node::RaftNode;
    use crate::raft::storage::RaftStorage;
    use crate::state_machine::{StateMachine, StateMachineConfig};

    /// Build a single-node (no-peers) RaftNode with on-disk state in a
    /// temp dir, auto-elevated to Leader (mirrors `main.rs` startup).
    fn make_single_node(node_id: &str) -> (tempfile::TempDir, Arc<RwLock<RaftNode>>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir
            .path()
            .join(format!("{node_id}.wal"))
            .to_str()
            .unwrap()
            .to_string();
        let meta = dir
            .path()
            .join(format!("{node_id}_meta.json"))
            .to_str()
            .unwrap()
            .to_string();
        let snap = dir
            .path()
            .join(format!("{node_id}_snapshot.json"))
            .to_str()
            .unwrap()
            .to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join(format!("{node_id}_sm"));
        let sm_config = StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
        let mut node = RaftNode::new_with_storage(node_id.to_string(), vec![], sm, storage);
        node.state = NodeState::Leader;
        let arc = Arc::new(RwLock::new(node));
        (dir, arc)
    }

    #[tokio::test]
    async fn coordinate_tx_single_node_commits_atomically() {
        let (_d, node_arc) = make_single_node("solo-coord");

        let ops = vec![
            TxOp::Put {
                key: "k1".into(),
                value: "v1".into(),
            },
            TxOp::Put {
                key: "k2".into(),
                value: "v2".into(),
            },
        ];
        let outcome = coordinate_tx(node_arc.clone(), "tx-solo".into(), ops).await;

        let (begin, decide, tx_id) = match outcome {
            TxOutcome::Committed {
                begin_index,
                decide_index,
                tx_id,
            } => (begin_index, decide_index, tx_id),
            other => panic!("expected Committed, got {:?}", other),
        };
        assert_eq!(tx_id, "tx-solo");
        // Single-node batch: BeginTx at N, DecideTx at N+1.
        assert_eq!(begin + 1, decide);
        // State machine should have both keys applied.
        let sm = node_arc.read().unwrap().state_machine.clone();
        let sm_read = sm.read().unwrap();
        assert_eq!(sm_read.get("k1"), Some("v1".to_string()));
        assert_eq!(sm_read.get("k2"), Some("v2".to_string()));
        assert_eq!(sm_read.pending_tx_count(), 0); // DecideTx purged the pending entry
    }

    #[tokio::test]
    async fn apply_logs_applies_begin_tx_and_decide_tx_in_steady_state() {
        // Regression test for the pre-PR-#13 bug: `apply_logs` used to
        // match only `Set` and `Delete`, so BeginTx / DecideTx entries
        // applied through the steady-state path (not just `replay_logs`)
        // were no-ops. This test seeds a leader with a BeginTx + DecideTx
        // pair, drives `apply_logs` by advancing commit_index, and
        // verifies that `pending_txs` is purged and the ops are applied.
        let (_d, node_arc) = make_single_node("apply-steady");
        let mut node = node_arc.write().unwrap();

        // Hand-append a BeginTx + DecideTx pair directly to the log.
        node.log.push(crate::protocol::LogEntry {
            term: 1,
            index: 1,
            command: Command::BeginTx {
                tx_id: "tx-apply".into(),
                ops: vec![TxOp::Put {
                    key: "applied-key".into(),
                    value: "applied-val".into(),
                }],
            },
        });
        node.log.push(crate::protocol::LogEntry {
            term: 1,
            index: 2,
            command: Command::DecideTx {
                tx_id: "tx-apply".into(),
                decision: TxDecision::Commit,
            },
        });
        node.commit_index = 2;
        drop(node);

        let mut node = node_arc.write().unwrap();
        node.apply_logs();
        drop(node);

        let sm = node_arc.read().unwrap().state_machine.clone();
        let sm_read = sm.read().unwrap();
        assert_eq!(sm_read.get("applied-key"), Some("applied-val".to_string()));
        assert_eq!(sm_read.pending_tx_count(), 0);
    }

    #[tokio::test]
    async fn apply_logs_abort_decision_does_not_apply_ops() {
        // Counterpart of the previous test for Abort: DecideTx(Abort)
        // through `apply_logs` must drop the pending entry without
        // applying any ops.
        let (_d, node_arc) = make_single_node("apply-abort");
        let mut node = node_arc.write().unwrap();

        node.log.push(crate::protocol::LogEntry {
            term: 1,
            index: 1,
            command: Command::BeginTx {
                tx_id: "tx-abort-steady".into(),
                ops: vec![TxOp::Put {
                    key: "ghost".into(),
                    value: "never".into(),
                }],
            },
        });
        node.log.push(crate::protocol::LogEntry {
            term: 1,
            index: 2,
            command: Command::DecideTx {
                tx_id: "tx-abort-steady".into(),
                decision: TxDecision::Abort,
            },
        });
        node.commit_index = 2;
        drop(node);

        let mut node = node_arc.write().unwrap();
        node.apply_logs();
        drop(node);

        let sm = node_arc.read().unwrap().state_machine.clone();
        let sm_read = sm.read().unwrap();
        assert_eq!(sm_read.get("ghost"), None);
        assert_eq!(sm_read.pending_tx_count(), 0);
    }

    #[test]
    fn tx_outcome_equality_and_debug_smoke() {
        // Smoke test: the public enum is well-formed. We rely on
        // PartialEq + Debug in coordinator logic, so a compile-time check
        // here catches accidental derivations.
        let a = TxOutcome::Committed {
            begin_index: 1,
            decide_index: 2,
            tx_id: "t".into(),
        };
        let b = TxOutcome::Committed {
            begin_index: 1,
            decide_index: 2,
            tx_id: "t".into(),
        };
        let c = TxOutcome::Aborted {
            tx_id: "t".into(),
            reason: "r".into(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(format!("{:?}", a).contains("Committed"));
        assert!(format!("{:?}", c).contains("Aborted"));
    }
}
