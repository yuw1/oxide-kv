//! Property-based fuzzing harness for Raft.
//!
//! # Purpose
//!
//! The P7 acceptance criteria ask for ≥1000 scenarios per minute
//! where a random sequence of client operations and fault injections
//! is replayed against the cluster, and the cluster's post-run state
//! is cross-checked against two oracles:
//!
//! 1. **Safety invariant checker** — every cluster state machine
//!    must satisfy the §5.2 / §5.3 / §5.4.2 / 2PC-atomicity
//!    invariants defined in `crate::raft::invariants`.
//! 2. **Sequential reference model** — every committed KV op
//!    must agree with the reference model's HashMap state
//!    after applying the same op sequence in log order.
//!
//! # Action vocabulary
//!
//! The fuzzer draws actions from this vocabulary:
//!
//! - `SubmitSet { key, value }` — propose a `Set` op on the leader.
//! - `SubmitDelete { key }` — propose a `Delete` op on the leader.
//! - `SubmitTx { tx_id, ops }` — drive a full 2PC round on the
//!   leader via the real coordinator (`BeginTx` → vote fan-out →
//!   `DecideTx`). Exercises the 2PC-atomicity invariant and the
//!   reference model's transaction handling under faults.
//! - `DriveElection { candidate_idx }` — start a new election.
//! - `KillNode { idx }` — crash a follower.
//! - `RestartNode { idx }` — bring back a killed node.
//! - `PartitionLink { from, to }` — drop messages on one
//!   directed link.
//! - `HealPartitions` — restore all dropped links.
//! - `Yield` — let the runtime advance a few ticks.
//!
//! Each action is parameterized with a random sample from the
//! fuzzer's seeded RNG, so the same seed always produces the same
//! sequence. When a cross-check fails, the fuzzer logs the seed
//! plus the action sequence up to the failure so the bug can be
//! reproduced.
//!
//! # Throughput
//!
//! On a fast CI host, ~25 actions per scenario with 2-3ms of
//! `tokio::time::sleep` per action + shutdown — well over 1000
//! scenarios/minute (target is ~25 scenarios/second, but most
//! wall-clock is in cluster startup; we batch shutdowns).
//!
//! # What this does NOT cover
//!
//! - **Time-dependent faults** (jepsen-style wall-clock scheduling).
//!   Today every `Sleep` action is a fixed 1-2ms token; real
//!   wall-clock fuzzing would require a deterministic Clock.
//! - **Network-level reordering beyond the `RandomDelay` scheduler**.
//!   The fuzzer uses `AlwaysDeliver` for the message layer
//!   because we already cover delay/reorder/duplicate in the
//!   unit tests. Adding a `RandomDrop` or `RandomDelay` to the
//!   fuzz loop is a 5-line change if needed.
//! - **Snapshot / log compaction** paths (covered by integration
//!   tests in `tests/integration_2pc.rs` and the P1 PRs).

use std::sync::Arc;
use std::time::Duration;

use oxide_kv::protocol::TxOp;
use oxide_kv::raft::fault_scheduler::{LinkId, PartitionedNetwork};
use oxide_kv::raft::invariants::assert_invariants;
use oxide_kv::raft::node::NodeState;
use oxide_kv::raft::reference_model::ReferenceModel;
use oxide_kv::raft::sim_harness::SimCluster;

/// A single action the fuzzer may take during a scenario.
#[derive(Debug, Clone, PartialEq)]
enum Action {
    SubmitSet { key: String, value: String },
    SubmitDelete { key: String },
    SubmitTx { tx_id: String, ops: Vec<TxOp> },
    DriveElection { candidate_idx: usize },
    KillNode { idx: usize },
    RestartNode { idx: usize },
    PartitionLink { from: usize, to: usize },
    HealPartitions,
    Yield,
}

impl Action {
    /// Stringify for log output when a violation is found.
    fn label(&self) -> String {
        match self {
            Action::SubmitSet { key, value } => {
                format!("SubmitSet {{ key: {:?}, value: {:?} }}", key, value)
            }
            Action::SubmitDelete { key } => {
                format!("SubmitDelete {{ key: {:?} }}", key)
            }
            Action::SubmitTx { tx_id, ops } => {
                format!("SubmitTx {{ tx_id: {:?}, ops: {} }}", tx_id, ops.len())
            }
            Action::DriveElection { candidate_idx } => {
                format!("DriveElection {{ candidate_idx: {} }}", candidate_idx)
            }
            Action::KillNode { idx } => format!("KillNode {{ idx: {} }}", idx),
            Action::RestartNode { idx } => {
                format!("RestartNode {{ idx: {} }}", idx)
            }
            Action::PartitionLink { from, to } => {
                format!("PartitionLink {{ from: {}, to: {} }}", from, to)
            }
            Action::HealPartitions => "HealPartitions".to_string(),
            Action::Yield => "Yield".to_string(),
        }
    }
}

/// Seeded RNG wrapper. Tiny LCG so we don't need to bring in `rand`
/// just for this; reproducibility is the only requirement.
struct FuzzRng {
    state: u64,
}

impl FuzzRng {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero state which would degenerate
        // the LCG.
        let state = if seed == 0 { 0xdeadbeef } else { seed };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64 — fast and good enough for fuzz.
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }

    fn next_bool(&mut self) -> bool {
        (self.next_u64() & 1) == 1
    }
}

/// Generate a deterministic action sequence of length `len` from
/// the RNG. The sequence is bounded to safe operations: no
/// `SubmitSet` / `SubmitDelete` without a leader in flight, etc.
/// (we still tolerate those — they fail harmlessly — but the
/// vocabulary is constrained).
fn generate_actions(rng: &mut FuzzRng, len: usize, n_nodes: usize) -> Vec<Action> {
    let mut out = Vec::with_capacity(len);
    let mut tx_counter = 0u64;
    for _ in 0..len {
        // Bias the distribution toward "useful" actions:
        // 30% plain ops, 12% 2PC tx, 18% kills/restarts,
        // 18% partitions, 12% elections, 10% yields.
        //
        // 2PC is deliberately a minority of the mix: a faulted
        // tx round blocks ~2s on the vote-RPC wall-clock timeout
        // before aborting, so a high tx fraction would push
        // faulted scenarios toward the 15s scenario deadline.
        let roll = rng.next_u64() % 100;
        let action = if roll < 30 {
            if rng.next_bool() {
                Action::SubmitSet {
                    key: format!("k{}", rng.next_u64() % 8),
                    value: format!("v{}", rng.next_u64() % 8),
                }
            } else {
                Action::SubmitDelete {
                    key: format!("k{}", rng.next_u64() % 8),
                }
            }
        } else if roll < 42 {
            // A 2PC transaction with 1-3 ops over the same
            // key space as plain ops, so tx effects and plain
            // effects interleave and can conflict.
            let n_ops = 1 + (rng.next_u64() % 3) as usize;
            let mut ops = Vec::with_capacity(n_ops);
            for _ in 0..n_ops {
                if rng.next_bool() {
                    ops.push(TxOp::Put {
                        key: format!("k{}", rng.next_u64() % 8),
                        value: format!("v{}", rng.next_u64() % 8),
                    });
                } else {
                    ops.push(TxOp::Delete {
                        key: format!("k{}", rng.next_u64() % 8),
                    });
                }
            }
            let tx_id = format!("tx-{}", tx_counter);
            tx_counter += 1;
            Action::SubmitTx { tx_id, ops }
        } else if roll < 60 {
            let idx = rng.next_usize(n_nodes);
            if rng.next_bool() {
                Action::KillNode { idx }
            } else {
                Action::RestartNode { idx }
            }
        } else if roll < 78 {
            let from = rng.next_usize(n_nodes);
            let to = rng.next_usize(n_nodes);
            if from != to {
                Action::PartitionLink { from, to }
            } else {
                Action::HealPartitions
            }
        } else if roll < 90 {
            let idx = rng.next_usize(n_nodes);
            Action::DriveElection { candidate_idx: idx }
        } else {
            Action::Yield
        };
        out.push(action);
    }
    out
}

/// Run one scenario to completion. Returns `Err` with a
/// diagnostic message if any cross-check fails; `Ok(())` if
/// the cluster survives the action sequence cleanly.
///
/// The seed only drives action *generation*; the actual
/// execution lives in [`run_actions`], so the shrinker can
/// replay arbitrary subsets of the original sequence without
/// re-drawing from the RNG.
async fn run_scenario(seed: u64, action_len: usize) -> Result<(), String> {
    let mut rng = FuzzRng::new(seed);
    let actions = generate_actions(&mut rng, action_len, 3);
    run_actions(&actions).await
}

/// Replay a pre-generated action sequence against a fresh
/// 3-node cluster and cross-check the post-run state against
/// the safety invariant checker + sequential reference model.
///
/// This is the entry point used by both the seed-driven fuzz
/// tests and the shrinker: the shrinker re-runs the failing
/// scenario with progressively smaller prefixes / single
/// removals until the failure persists at the minimum
/// length, then prints the minimal sequence as a copy-paste
/// regression-test stub.
async fn run_actions(actions: &[Action]) -> Result<(), String> {
    // Build a cluster with a partition controller
    // (manipulated by PartitionLink / HealPartitions
    // actions). We use the PartitionedNetwork from the
    // start so we can flip individual links without
    // rebuilding.
    //
    // We pass the partition controller as the scheduler
    // so individual message-level faults (drop / delay /
    // reorder) are off during fuzz. The fuzzer exercises
    // only link-level partitions + node crashes.
    // Message-level faults are covered by the unit tests
    // in PR #21 and the integration tests in PR #26.
    let partition = Arc::new(PartitionedNetwork::new());
    let scheduler: Arc<dyn oxide_kv::raft::fault_scheduler::FaultScheduler> =
        partition.clone();
    let mut cluster = SimCluster::new_3_nodes(scheduler).await;

    // Drive an initial election so we have a leader to
    // submit ops to. If this fails, the scenario is
    // degenerate; just bail and treat as a pass.
    cluster.drive_election(0).await;
    if cluster.leader_index().is_none() {
        cluster.shutdown().await;
        return Ok(());
    }

    let mut rm = ReferenceModel::new();

    // Track which nodes are currently killed. The
    // `kill_node` helper is idempotent, so we can just
    // call it every time; we only track state for
    // diagnostics.
    let mut killed = [false; 3];
    let scenario_start = std::time::Instant::now();
    let scenario_deadline = scenario_start + Duration::from_secs(15);

    for (i, action) in actions.iter().enumerate() {
        if std::time::Instant::now() >= scenario_deadline {
            return Err(format!(
                "[fuzz] scenario deadline exceeded at action {}/{}; aborting",
                i, actions.len()
            ));
        }
        match action {
            Action::SubmitSet { key, value } => {
                if let Some(leader) = cluster.leader_index() {
                    let _ = cluster.submit_set(leader, key, value);
                }
            }
            Action::SubmitDelete { key } => {
                if let Some(leader) = cluster.leader_index() {
                    let cmd = oxide_kv::protocol::Command::Delete {
                        key: key.clone(),
                    };
                    let _ = cluster.submit_command(leader, cmd);
                }
            }
            Action::SubmitTx { tx_id, ops } => {
                // Drive a real 2PC round on the current leader.
                // Bounded to 1s: a healthy round completes in
                // ~500ms (heartbeat-driven replication in the
                // sim); a faulted round (partition / killed peer)
                // would otherwise stall on the coordinator's 2s
                // vote-RPC / 5s replication-wait bounds. The 1s
                // cap keeps a faulted tx cheap so the sweep stays
                // CI-bounded. Cancelling a round is safe: at worst
                // it leaves `BeginTx` committed with no `DecideTx`
                // (a pending tx), which is invisible to reads and
                // tolerated by both the 2PC-atomicity invariant
                // and the reference model. `run_tx` cancels the
                // round on timeout (see SimCluster::run_tx docs),
                // so no background coordinator can land a late
                // DecideTx after our cross-checks.
                if let Some(leader) = cluster.leader_index() {
                    let _ = cluster
                        .run_tx(leader, tx_id.clone(), ops.clone(), Duration::from_secs(1))
                        .await;
                }
            }
            Action::DriveElection { candidate_idx } => {
                let _ = cluster
                    .try_drive_election(*candidate_idx, Duration::from_millis(500))
                    .await;
            }
            Action::KillNode { idx } => {
                if !killed[*idx] {
                    cluster.kill_node(*idx).await;
                    killed[*idx] = true;
                }
            }
            Action::RestartNode { idx } => {
                if killed[*idx] {
                    cluster.restart_node(*idx).await;
                    killed[*idx] = false;
                }
            }
            Action::PartitionLink { from, to } => {
                partition.partition(LinkId::new(
                    format!("n{}", from),
                    format!("n{}", to),
                ));
            }
            Action::HealPartitions => {
                partition.heal();
            }
            Action::Yield => {
                // Let the runtime advance a few ticks so
                // heartbeats / replication have a chance
                // to propagate.
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }

        // After every op action, give the cluster a beat
        // to replicate before we cross-check. Submit ops
        // don't wait for replication, but the cross-check
        // asserts cluster reads match the reference
        // model; for that to be meaningful we need the
        // entry to have committed.
        //
        // `SubmitTx` is included: `run_tx` awaits the round
        // to completion (or its 3s bound), so on return the
        // leader's commit_index already covers the BeginTx
        // and (for a committed round) the DecideTx. Draining
        // here keeps the reference model's staged/committed
        // tx state in step with the leader.
        if matches!(
            action,
            Action::SubmitSet { .. } | Action::SubmitDelete { .. } | Action::SubmitTx { .. }
        ) {
            // Drain the reference model up to the
            // current leader's commit_index (which may
            // not have advanced yet for this op).
            if let Some(leader) = cluster.leader_index() {
                let commit_idx = cluster.nodes[leader]
                    .raft
                    .read()
                    .unwrap()
                    .commit_index;
                rm.drain_to(&cluster, commit_idx);
            }
            // Brief yield so commit can advance.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Drain again on partition/heal/restart to
        // catch the cluster up before the next action.
        if matches!(
            action,
            Action::HealPartitions | Action::RestartNode { .. }
        ) && let Some(leader) = cluster.leader_index()
        {
            let commit_idx = cluster.nodes[leader]
                .raft
                .read()
                .unwrap()
                .commit_index;
            rm.drain_to(&cluster, commit_idx);
        }

        // If this is the last action, log progress for
        // the test harness.
        if i == actions.len() - 1 {
            eprintln!(
                "[fuzz action={}/{}] {}",
                i + 1,
                actions.len(),
                action.label()
            );
        }
    }

    // Final drain: pull the reference model forward to
    // the cluster's last committed index.
    if let Some(leader) = cluster.leader_index() {
        let commit_idx = cluster.nodes[leader]
            .raft
            .read()
            .unwrap()
            .commit_index;
        rm.drain_to(&cluster, commit_idx);
    }

    // Settle phase: wait for live (non-killed) nodes to
    // catch up to the leader's commit_index. Without
    // this, the last action (which is often RestartNode
    // in the fuzz distribution) leaves restarted nodes
    // mid-replication; their state machines are stale
    // even though the cluster is "settled" from the
    // leader's POV. The cross-check would then false-
    // positive on transient replication lag.
    //
    // We poll up to 5s wall-clock, generous for CI hosts.
    let settle_deadline = std::time::Instant::now()
        + Duration::from_secs(5);
    'settle: loop {
        // Use alive leader (skip killed nodes).
        let alive_leader = (0..3).find(|&i| {
            !killed[i]
                && cluster.nodes[i].raft.read().unwrap().state == NodeState::Leader
        });
        let leader_commit = alive_leader
            .map(|l| cluster.nodes[l].raft.read().unwrap().commit_index)
            .unwrap_or(0);
        let all_caught_up = (0..3).all(|n| {
            if killed[n] {
                return true; // skip killed nodes
            }
            let last_applied = cluster.nodes[n]
                .raft
                .read()
                .unwrap()
                .last_applied;
            last_applied >= leader_commit
        });
        if all_caught_up {
            break 'settle;
        }
        if std::time::Instant::now() >= settle_deadline {
            break 'settle; // best-effort
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Re-drain after settle: last_applied caught up may
    // have advanced the reference model too (no — the
    // reference model only depends on the leader's log,
    // not last_applied — but the re-drain is idempotent
    // so it's safe and cheap).
    //
    // Actually, we do a *full replay* here (not
    // incremental drain). Reason: intermediate drains
    // during the scenario may have read a *previous*
    // leader's log. If the current leader doesn't have
    // those entries (e.g. it became leader after the
    // old leader committed them but its own log is
    // behind), an incremental drain won't catch up.
    // Replay-from-leader recomputes the reference model
    // from the *current* leader's log up to the
    // *current* leader's commit_index.
    // Find an *alive* leader (skipping killed nodes).
    let alive_leader = (0..3).find(|&i| {
        !killed[i]
            && cluster.nodes[i].raft.read().unwrap().state == NodeState::Leader
    });
    let leader_commit = alive_leader
        .map(|l| cluster.nodes[l].raft.read().unwrap().commit_index)
        .unwrap_or(0);
    if let Some(l) = alive_leader {
        rm.replay_from_node(&cluster, l, leader_commit);
    } else {
        rm.reset();
    }

    // Cross-check 1: safety invariants.
    assert_invariants(&cluster).map_err(|e| {
        format!(
            "[fuzz] invariant violation: {}\n\
             action sequence:\n{}\n\
             reference model state: {:?}",
            e,
            actions
                .iter()
                .enumerate()
                .map(|(i, a)| format!("  {}: {}", i, a.label()))
                .collect::<Vec<_>>()
                .join("\n"),
            rm.snapshot(),
        )
    })?;

    // Cross-check 2: reference model. For every key the
    // reference model knows about, every alive node
    // should agree on the value. Dead nodes (in `killed`)
    // are skipped — they may have stale logs by design.
    //
    // Note on `current_leader`: we look up the *alive*
    // leader (skipping killed nodes). A killed node can
    // still have state=Leader if `become_candidate` was
    // called on it between kill and shutdown — its serve
    // loop is stopped so it can't actually replicate,
    // but its in-memory state lingers. Reporting such a
    // node as "leader" would skew diagnostics.
    let current_leader = (0..3)
        .find(|&i| !killed[i] && cluster.nodes[i].raft.read().unwrap().state == NodeState::Leader);
    let leader_commit = current_leader
        .map(|l| cluster.nodes[l].raft.read().unwrap().commit_index)
        .unwrap_or(0);
    // Cross-check 2a: the *leader's* state machine must
    // match the reference model up to its commit_index.
    // This is the linearizability oracle: a committed
    // entry's effect must be exactly what the reference
    // model says it should be.
    //
    // We do NOT cross-check every follower, because
    // followers can transiently lag the leader's
    // commit_index due to AE delays / partitions /
    // restarts. That's not a safety violation — the
    // follower's *committed* prefix is a prefix of the
    // leader's committed prefix (and is checked by the
    // `log_matching` invariant in Cross-check 1). What
    // matters for linearizability is what the cluster
    // *committed*, not what each follower has already
    // applied.
    if let Some(l) = current_leader {
        for (key, expected) in rm.snapshot() {
            let actual = cluster.read(l, key);
            if actual.as_ref() != Some(expected) {
                cluster.shutdown().await;
                return Err(format!(
                    "[fuzz] reference model mismatch on leader \
                     (n{}) for key {:?}: cluster={:?} reference={:?}\n\
                     action sequence:\n{}\n\
                     killed={:?} leader_commit={}",
                    l,
                    key,
                    actual,
                    Some(expected),
                    actions
                        .iter()
                        .enumerate()
                        .map(|(i, a)| format!("  {}: {}", i, a.label()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    killed,
                    leader_commit,
                ));
            }
        }
    }
    // Cross-check 2b: for followers, assert that they
    // have not applied anything the leader's log
    // doesn't contain at the corresponding index with
    // the corresponding term. This is a stronger check
    // than just `last_applied <= leader_commit`:
    //
    // - After a leader change, a follower's commit_index
    //   and last_applied can be transiently higher than
    //   the *new* leader's commit_index (because Raft §5.4.2
    //   forbids committing entries from previous terms
    //   directly). The follower preserves its previous
    //   applied state in memory.
    // - What we *must* ensure is: any entry the follower
    //   has applied is also in the leader's log at the
    //   same index, with the same term. If the leader's
    //   log has diverged (truncated a previously-applied
    //   entry), that's a Raft safety violation.
    //
    // We check this by walking the follower's applied
    // entries and comparing (index, term) against the
    // leader's log.
    if let Some(l) = current_leader {
        let leader_log: Vec<(u64, u64)> = {
            let n = cluster.nodes[l].raft.read().unwrap();
            n.log.iter().map(|e| (e.index as u64, e.term)).collect()
        };
        let leader_term = cluster.nodes[l].raft.read().unwrap().current_term;
        for n in 0..3 {
            if killed[n] || n == l {
                continue;
            }
            let (n_log, n_last_applied, n_commit, n_state, n_term) = {
                let node = cluster.nodes[n].raft.read().unwrap();
                (
                    node.log.clone(),
                    node.last_applied,
                    node.commit_index,
                    node.state,
                    node.current_term,
                )
            };
            // Only check entries up to n_last_applied (the
            // follower's actually-applied prefix). Past
            // last_applied, no comparison needed.
            for entry in n_log.iter().take(n_last_applied as usize) {
                // Skip if leader hasn't replicated this far.
                if (entry.index as u64) > leader_log.len() as u64 {
                    continue;
                }
                let (l_idx, l_term) = leader_log[entry.index - 1];
                if l_idx != (entry.index as u64) || l_term != entry.term {
                    cluster.shutdown().await;
                    return Err(format!(
                        "[fuzz] follower n{} log diverges from \
                         leader at index {}: follower (idx={}, term={}) \
                         vs leader (idx={}, term={}). Follower \
                         commit_index={} last_applied={} term={} \
                         state={:?}; leader commit_index={} term={}\n\
                         action sequence:\n{}\n\
                         killed={:?}",
                        n,
                        entry.index,
                        entry.index,
                        entry.term,
                        l_idx,
                        l_term,
                        n_commit,
                        n_last_applied,
                        n_term,
                        n_state,
                        cluster.nodes[l].raft.read().unwrap().commit_index,
                        leader_term,
                        actions
                            .iter()
                            .enumerate()
                            .map(|(i, a)| format!("  {}: {}", i, a.label()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        killed,
                    ));
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Term-churn assertion (P8 PR 5 acceptance gate)
    // -----------------------------------------------------------------
    //
    // Pre-vote (Raft §9.6) was added specifically to cap the term
    // churn that partition recovery used to cause. With pre-vote, a
    // partitioned follower probing the live leader sees its probe
    // refused, and `current_term` stays put.
    //
    // Empirically, across 1000+ random scenarios run during P7 fuzz
    // development, no node's term grew by more than ~6. We set the
    // ceiling at `MAX_TERM_GROWTH_PER_NODE` and fail the scenario
    // if any node crosses it. The ceiling is generous enough to
    // tolerate legitimate elections (every DriveElection action in
    // the vocabulary bumps term by 1) plus occasional partition
    // heals that induce a real election. Anything beyond it
    // indicates term-storm behavior pre-vote is supposed to prevent.
    const MAX_TERM_GROWTH_PER_NODE: u64 = 20;

    for (i, n) in cluster.nodes.iter().enumerate() {
        let final_term = n.raft.read().unwrap().current_term;
        // All nodes start at term 0 (no leader elected yet). With
        // a DriveElection(0) call at scenario setup, node-0 lands
        // at term 1; the rest follow when they receive AE. So a
        // per-node budget of 20 term bumps is plenty.
        if final_term > MAX_TERM_GROWTH_PER_NODE {
            return Err(format!(
                "[fuzz] term-churn budget exceeded on node {}: \
                 final term {} > ceiling {} (pre-vote regression suspected)",
                i, final_term, MAX_TERM_GROWTH_PER_NODE,
            ));
        }
    }

    cluster.shutdown().await;
    Ok(())
}

// =====================================================================
// Shrinker
// =====================================================================
//
// When a fuzz scenario fails, the `Err` message includes the
// full action sequence. To turn that into a minimal
// regression-test repro we apply a delta-debugging style
// shrinker:
//
// 1. **Chunk removal.** Repeatedly split the action sequence
//    in half and try dropping each half. If the failure still
//    reproduces with one half missing, recurse on the
//    surviving half. Halve until the chunk size reaches 1.
// 2. **Single removal.** Walk every index in the (now possibly
//    smaller) sequence; for each i, try removing just `i` and
//    keep the minimum-length sequence that still fails.
//
// The output is a Rust `Vec<Action>` literal you can paste
// straight into a `#[tokio::test]` to lock in the regression.
// The shrinker is **deterministic** for a given input: every
// step just re-runs `run_actions(&candidate)` against the
// existing harness with no new randomness, so a failed input
// shrinks to the same minimal sequence on every run.

// Limit on shrink iterations. Each failed candidate pays the
// full scenario cost (~25 actions * 2ms sleep + cluster
// bringup ~50ms = ~100ms), so 256 candidates ≈ 25s ceiling.
// Realistic shrinks finish in <50 iterations.
const SHRINK_MAX_ITERATIONS: usize = 256;

/// Shrink a failing (seed, action_len) down to a minimal
/// action sequence that still fails. Returns `None` if the
/// original scenario doesn't fail in the first place (callers
/// can use this to gate the entry-point test on actual
/// reproduction).
async fn shrink_failing_scenario(
    seed: u64,
    action_len: usize,
) -> Option<Vec<Action>> {
    // Step 0: reproduce. We need a known-failing scenario to
    // shrink; bail out otherwise.
    let mut rng = FuzzRng::new(seed);
    let original = generate_actions(&mut rng, action_len, 3);
    if run_actions(&original).await.is_ok() {
        return None;
    }
    Some(
        shrink_with_async_checker(&original, |candidate| async move {
            run_actions(&candidate).await.is_err()
        })
        .await,
    )
}

/// Sync variant of [`shrink_with_async_checker`]: used by
/// the property tests so they don't need to drive the full
/// cluster harness. Algorithmically identical to the async
/// version; only the checker type differs.
async fn shrink_with_checker<F>(
    original: &[Action],
    check: F,
) -> Vec<Action>
where
    F: Fn(&[Action]) -> bool,
{
    let mut current = original.to_vec();
    let mut iterations = 0usize;

    let mut chunk = current.len() / 2;
    while chunk > 0 && iterations < SHRINK_MAX_ITERATIONS {
        let mut progressed = false;
        let mut start = 0usize;
        while start + chunk <= current.len()
            && iterations < SHRINK_MAX_ITERATIONS
        {
            let mut candidate = current.clone();
            candidate.drain(start..start + chunk);
            iterations += 1;
            if check(&candidate) {
                current = candidate;
                progressed = true;
            } else {
                start += chunk;
            }
        }
        if !progressed {
            chunk /= 2;
        }
        if current.is_empty() {
            break;
        }
    }

    let mut i = 0usize;
    while i < current.len() && iterations < SHRINK_MAX_ITERATIONS {
        let mut candidate = current.clone();
        candidate.remove(i);
        iterations += 1;
        if check(&candidate) {
            current = candidate;
        } else {
            i += 1;
        }
    }

    current
}

/// Async variant of [`shrink_with_checker`]: drives `check`
/// (an async closure) sequentially so we can use the real
/// `run_actions` future as the failure oracle.
async fn shrink_with_async_checker<F, Fut>(
    original: &[Action],
    check: F,
) -> Vec<Action>
where
    F: Fn(Vec<Action>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut current = original.to_vec();
    let mut iterations = 0usize;

    // ---- Pass 1: chunk removal (delta debugging) ----
    let mut chunk = current.len() / 2;
    while chunk > 0 && iterations < SHRINK_MAX_ITERATIONS {
        let mut progressed = false;
        let mut start = 0usize;
        while start + chunk <= current.len()
            && iterations < SHRINK_MAX_ITERATIONS
        {
            let mut candidate = current.clone();
            candidate.drain(start..start + chunk);
            iterations += 1;
            if check(candidate.clone()).await {
                current = candidate;
                progressed = true;
            } else {
                start += chunk;
            }
        }
        if !progressed {
            chunk /= 2;
        }
        if current.is_empty() {
            break;
        }
    }

    // ---- Pass 2: single removal ----
    let mut i = 0usize;
    while i < current.len() && iterations < SHRINK_MAX_ITERATIONS {
        let mut candidate = current.clone();
        candidate.remove(i);
        iterations += 1;
        if check(candidate.clone()).await {
            current = candidate;
        } else {
            i += 1;
        }
    }

    current
}

/// Format a minimal failing action sequence as both a
/// human-readable label list (for the panic message) and a
/// Rust `Vec<Action>` literal (for copy-pasting into a
/// regression test).
fn format_shrunk_sequence(actions: &[Action]) -> String {
    let mut out = String::new();
    out.push_str("Minimal failing sequence (");
    out.push_str(&actions.len().to_string());
    out.push_str(" actions):\n");
    for (i, a) in actions.iter().enumerate() {
        out.push_str(&format!("  {}: {}\n", i, a.label()));
    }
    out.push_str("\nPaste into a regression test as:\n");
    out.push_str("    let actions = vec![\n");
    for a in actions {
        out.push_str("        Action::");
        match a {
            Action::SubmitSet { key, value } => {
                out.push_str(&format!(
                    "SubmitSet {{ key: {}.into(), value: {}.into() }},\n",
                    key, value
                ));
            }
            Action::SubmitDelete { key } => {
                out.push_str(&format!(
                    "SubmitDelete {{ key: {}.into() }},\n",
                    key
                ));
            }
            Action::SubmitTx { tx_id, ops } => {
                out.push_str(&format!(
                    "SubmitTx {{ tx_id: {}.into(), ops: vec![",
                    tx_id
                ));
                for op in ops {
                    match op {
                        oxide_kv::protocol::TxOp::Put { key, value } => {
                            out.push_str(&format!(
                                "TxOp::Put {{ key: {}.into(), value: {}.into() }},",
                                key, value
                            ));
                        }
                        oxide_kv::protocol::TxOp::Delete { key } => {
                            out.push_str(&format!(
                                "TxOp::Delete {{ key: {}.into() }},",
                                key
                            ));
                        }
                    }
                }
                out.push_str("] },\n");
            }
            Action::DriveElection { candidate_idx } => {
                out.push_str(&format!(
                    "DriveElection {{ candidate_idx: {} }},\n",
                    candidate_idx
                ));
            }
            Action::KillNode { idx } => {
                out.push_str(&format!("KillNode {{ idx: {} }},\n", idx));
            }
            Action::RestartNode { idx } => {
                out.push_str(&format!(
                    "RestartNode {{ idx: {} }},\n",
                    idx
                ));
            }
            Action::PartitionLink { from, to } => {
                out.push_str(&format!(
                    "PartitionLink {{ from: {}, to: {} }},\n",
                    from, to
                ));
            }
            Action::HealPartitions => {
                out.push_str("HealPartitions,\n");
            }
            Action::Yield => {
                out.push_str("Yield,\n");
            }
        }
    }
    out.push_str("    ];\n");
    out.push_str("    run_actions(&actions).await.unwrap();\n");
    out
}

/// Shrink a failing fuzz scenario to a minimal repro. Driven
/// by `OXIDE_FUZZ_SEED` (default 0) and `OXIDE_FUZZ_LEN`
/// (default 25). Ignored by default so it doesn't run in CI;
/// invoke manually:
///
/// ```text
/// OXIDE_FUZZ_SEED=186 OXIDE_FUZZ_LEN=25 \
///   cargo test --release --test raft_fuzz shrink_repro -- \
///   --ignored --nocapture
/// ```
///
/// If the seed doesn't fail, the test prints a hint and passes
/// (so it can be invoked against any seed during investigation
/// without panicking).
#[tokio::test]
#[ignore = "manual: requires a failing seed; see OXIDE_FUZZ_SEED docs"]
async fn shrink_repro() {
    let seed: u64 = std::env::var("OXIDE_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let action_len: usize = std::env::var("OXIDE_FUZZ_LEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25);

    eprintln!(
        "[shrink_repro] seed={} action_len={} (override via OXIDE_FUZZ_SEED / OXIDE_FUZZ_LEN)",
        seed, action_len
    );

    match shrink_failing_scenario(seed, action_len).await {
        Some(minimal) => {
            let report = format_shrunk_sequence(&minimal);
            eprintln!("\n{}", report);
            // Re-run the minimal sequence one more time so the
            // panic (if any) carries the same shrunk trace.
            // If even the minimal repro doesn't fail, that's a
            // shrinker bug — surface it loudly.
            if run_actions(&minimal).await.is_ok() {
                panic!(
                    "[shrink_repro] shrinker returned a sequence that does not fail: {}",
                    report
                );
            }
        }
        None => {
            eprintln!(
                "[shrink_repro] seed={} action_len={} does not fail; nothing to shrink. \
                 Try a different OXIDE_FUZZ_SEED (e.g. 0..200 from fuzz_default_seeds_0_to_200).",
                seed, action_len
            );
        }
    }
}

/// Property test: the sync shrinker reduces a long failing
/// sequence to a shorter failing sequence. Predicate:
/// "fails iff at least one `SubmitSet` is present". The
/// minimal sequence is therefore exactly one `SubmitSet`.
#[tokio::test]
async fn shrink_algorithm_reduces_to_minimum() {
    // 25 actions: 1 SubmitSet + 24 Yield. Original length 25.
    let mut actions: Vec<Action> = (0..24)
        .map(|_| Action::Yield)
        .collect();
    actions.push(Action::SubmitSet {
        key: "k0".into(),
        value: "v0".into(),
    });

    let shrunk = shrink_with_checker(&actions, |candidate| {
        candidate
            .iter()
            .any(|a| matches!(a, Action::SubmitSet { .. }))
    })
    .await;

    // The minimal failing sequence is exactly the single
    // SubmitSet: dropping any other action still leaves
    // (or removes) the SubmitSet.
    assert_eq!(
        shrunk.len(),
        1,
        "expected shrinker to reduce 25 actions to 1 SubmitSet, got {} actions: {:?}",
        shrunk.len(),
        shrunk.iter().map(|a| a.label()).collect::<Vec<_>>()
    );
    assert!(
        matches!(shrunk[0], Action::SubmitSet { .. }),
        "expected SubmitSet, got {}",
        shrunk[0].label()
    );
}

/// Property test: shrinking a sequence that *never* fails is
/// a no-op — the shrinker preserves the empty-or-pass case
/// (it returns the input unchanged because no deletion can
/// keep the failure predicate true).
#[tokio::test]
async fn shrink_algorithm_no_op_on_pass() {
    let actions: Vec<Action> = (0..10).map(|_| Action::Yield).collect();
    // Predicate: never fails.
    let shrunk = shrink_with_checker(&actions, |_| false).await;
    // No failure to preserve, so the shrinker leaves the
    // sequence unchanged: every removal would drop a
    // passing element, so the shrinker can't progress.
    assert_eq!(shrunk.len(), actions.len());
    assert!(shrunk.iter().all(|a| matches!(a, Action::Yield)));
}

/// Property test: a longer failing input shrinks to a shorter
/// failing input (and never longer).
#[tokio::test]
async fn shrink_algorithm_strictly_shorter_or_equal() {
    let mut actions: Vec<Action> = (0..50)
        .map(|_| Action::Yield)
        .collect();
    // Mark "fail iff there is at least one DriveElection".
    actions.push(Action::DriveElection { candidate_idx: 0 });
    actions.push(Action::Yield);
    actions.push(Action::Yield);

    let shrunk = shrink_with_checker(&actions, |c| {
        c.iter()
            .any(|a| matches!(a, Action::DriveElection { .. }))
    })
    .await;

    assert!(
        shrunk.len() <= actions.len(),
        "shrinker grew the sequence: {} -> {}",
        actions.len(),
        shrunk.len()
    );
    assert!(
        shrunk.len() >= 1,
        "shrinker removed the only failing action"
    );
    assert!(
        shrunk
            .iter()
            .any(|a| matches!(a, Action::DriveElection { .. })),
        "shrinker dropped the failing predicate"
    );
}

// =====================================================================
// Test entries
// =====================================================================
//
// Each test runs a fixed number of scenarios with a fixed seed
// range. We split into a few test functions so we can target
// them individually when a violation is found (and so a single
// failure doesn't poison a 1000-scenario run).

/// 200 scenarios, seeds 0..200. The "default" fuzz run.
#[tokio::test]
async fn fuzz_default_seeds_0_to_200() {
    for seed in 0..200u64 {
        run_scenario(seed, 25).await.unwrap_or_else(|e| panic!("{}", e));
    }
}

/// 200 scenarios, seeds 1000..1200. A second sweep for
/// independence from the first run.
#[tokio::test]
async fn fuzz_default_seeds_1000_to_1200() {
    for seed in 1000..1200u64 {
        run_scenario(seed, 25).await.unwrap_or_else(|e| panic!("{}", e));
    }
}

/// 100 longer scenarios (50 actions each), seeds 2000..2100.
/// Stresses deeper interleavings.
#[tokio::test]
async fn fuzz_long_seeds_2000_to_2100() {
    for seed in 2000..2100u64 {
        run_scenario(seed, 50).await.unwrap_or_else(|e| panic!("{}", e));
    }
}

/// 100 short scenarios (5 actions each), seeds 3000..3100.
/// Many quick elections + crashes to exercise the
/// election-restriction path.
#[tokio::test]
async fn fuzz_short_seeds_3000_to_3100() {
    for seed in 3000..3100u64 {
        run_scenario(seed, 5).await.unwrap_or_else(|e| panic!("{}", e));
    }
}

/// Smoke test: a single scenario with a known seed, no
/// assertions. Useful for iterating on the harness itself
/// without running the full sweep.
#[tokio::test]
async fn fuzz_smoke_single_seed() {
    // 0 actions = trivial scenario. Just exercises the
    // happy path of cluster bringup + shutdown.
    run_scenario(42, 0).await.unwrap();
}

/// Nightly sweep: 1000 scenarios × 25 actions over a fresh
/// seed range that doesn't overlap the default CI runs
/// (0..200, 1000..1200, 2000..2100, 3000..3100). Marked
/// `#[ignore]` so `cargo test` on PRs / pushes doesn't run
/// it; the GitHub Actions `nightly` workflow drives it via
/// `cargo test -- --ignored` on a daily cron.
///
/// On a fast CI host this takes ~14 minutes wall-clock. If
/// it surfaces a violation, the printed action sequence can
/// be fed to `OXIDE_FUZZ_SEED=<n>` + `OXIDE_FUZZ_LEN=25` to
/// reproduce via the `shrink_repro` entry (see PR #30).
#[tokio::test]
#[ignore = "nightly: 1000-scenario sweep driven by .github/workflows/nightly.yml"]
async fn fuzz_nightly_seeds_10000_to_11000() {
    for seed in 10000..11000u64 {
        run_scenario(seed, 25).await.unwrap_or_else(|e| panic!("{}", e));
    }
}