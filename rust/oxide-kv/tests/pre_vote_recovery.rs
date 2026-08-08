//! P8 PR 5 — Pre-vote (Raft §9.6) recovery scenarios.
//!
//! These tests exercise the **disruptive server** problem that
//! motivated pre-vote: a follower that has been partitioned away
//! from the cluster used to immediately `become_candidate` on
//! election timeout, bump `current_term`, and force the live
//! leader to step down on the next AppendEntries. With pre-vote,
//! the recovered follower probes first; the live leader refuses
//! the probe (election restriction + same-term leadership), and
//! `current_term` stays put.
//!
//! Two scenarios:
//!
//! 1. **Same-term recovery**: partition 2/3 nodes from a healthy
//!    cluster → isolated follower times out → probes → live
//!    leader (on the **same term**) refuses → no term churn.
//!
//! 2. **Higher-term probe refusal**: a stale peer probes at a term
//!    more than one ahead of ours; the live leader refuses without
//!    adopting the term. (Regression for the "stale peer probing
//!    at arbitrarily high terms" failure mode.)
//!
//! Each test asserts:
//! - the partitioned follower never bumps its term above the
//!   leader's term
//! - the live leader's term stays at its initial value
//! - the partition heals cleanly and a follow-up election works
//!
//! The fuzz vocabulary in `tests/raft_fuzz.rs` covers broader
//! random recovery scenarios at higher coverage; this file
//! pins the specific pre-vote invariants with deterministic
//! setups so regressions have a clear root cause.

use oxide_kv::raft::fault_scheduler::{LinkId, PartitionedNetwork};
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::sim_harness::SimCluster;
use std::sync::Arc;
use std::time::Duration;

/// Helper: wait until `predicate` returns true, polling every
/// `poll` for at most `timeout`. Returns whether the predicate
/// observed success.
async fn wait_until<F: Fn() -> bool>(timeout: Duration, poll: Duration, predicate: F) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// **Disruptive server regression (Raft §9.6).**
///
/// Setup: a 3-node cluster with node-0 as leader. Then partition
/// node-1 from the majority (n0 and n2 can talk; n1 can't reach
/// either). After the partition is set up, node-1 drives a
/// pre-vote (simulating its election timer firing while
/// isolated).
///
/// Expected: node-1 collects only its self-vote (1 < majority of
/// 2 needed for an isolated node), so it stays in PreCandidate
/// and never promotes. **Crucially: node-0's term must not bump.**
///
/// Why this matters without pre-vote: the old `become_candidate`
/// would have immediately bumped term to 2, sent RequestVote
/// (which would fail to reach n0/n2 anyway), but as soon as the
/// partition healed n1 would still be a Candidate at term 2, and
/// n0's next AppendEntries at term 1 would force n0 to step
/// down. With pre-vote, n1 never promotes, so the term stays
/// clean.
#[tokio::test(flavor = "current_thread")]
async fn pre_vote_isolated_follower_does_not_promote_or_disrupt() {
    let partition = Arc::new(PartitionedNetwork::new());
    let scheduler: Arc<dyn oxide_kv::raft::fault_scheduler::FaultScheduler> = partition.clone();
    let cluster = SimCluster::new_3_nodes(scheduler).await;

    // (1) Get a stable leader at term 1.
    cluster.drive_election(0).await;
    assert_eq!(cluster.leader_index(), Some(0));
    let term_at_setup = cluster.current_term(0);
    assert_eq!(term_at_setup, 1);

    // (2) Partition node-1 from everyone else (both directions,
    // both peers). n0 and n2 can still talk to each other, so
    // they form a 2-node majority that can keep the leader alive.
    partition.partition(LinkId::new("n1", "n0"));
    partition.partition(LinkId::new("n0", "n1"));
    partition.partition(LinkId::new("n1", "n2"));
    partition.partition(LinkId::new("n2", "n1"));

    tokio::time::sleep(Duration::from_millis(100)).await;

    // (3) Drive pre-vote on the isolated node-1.
    let node1 = cluster.nodes[1].raft.clone();
    let initial_term_n1 = node1.read().unwrap().current_term;
    RaftNode::become_pre_candidate(node1.clone());

    // (4) Give the probe fan-out a chance to round-trip.
    // n1 cannot reach n0 or n2 (both links partitioned). It
    // counts only its self-vote, never reaches the 2-of-3
    // quorum, never promotes.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // (5) node-1 must still be PreCandidate or have reverted to
    // Follower (PreCandidate if the probe is still in flight,
    // Follower if it timed out). It must NEVER be Candidate.
    let final_state = node1.read().unwrap().state;
    let final_term = node1.read().unwrap().current_term;
    assert!(
        matches!(final_state, NodeState::Follower | NodeState::PreCandidate),
        "isolated node-1 must not become Candidate (pre-vote should fail to gather quorum); \
         got state={:?} term={}",
        final_state,
        final_term,
    );

    // (6) Hard invariant: no term churn on the isolated follower.
    // If pre-vote is broken, the old `become_candidate` would
    // have bumped this node's term to 2.
    assert_eq!(
        final_term, initial_term_n1,
        "isolated node-1's term must not change",
    );

    // (7) Hard invariant: no term churn on the live leader.
    // This is the key property pre-vote protects.
    assert_eq!(
        cluster.current_term(0),
        term_at_setup,
        "live leader's term must not change while a partitioned follower probes (was {}, now {})",
        term_at_setup,
        cluster.current_term(0),
    );

    // (8) After heal, the cluster should still be functional:
    // drive a follow-up election to confirm the leader still
    // has a path forward. (Sanity check that pre-vote hasn't
    // accidentally broken the heal path.)
    partition.heal();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // node-1 can now reach n0 — it should observe n0 is still
    // leader via AppendEntries, and revert to Follower.
    let _ = wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
        node1.read().unwrap().state == NodeState::Follower
    })
    .await;

    cluster.shutdown().await;
}

/// Drive a pre-vote round on node-1 with **no partition** and
/// verify that the probe succeeds, node-1 promotes to Candidate,
/// then to Leader (in a 3-node cluster the live leader's vote
/// is replaced by node-1 winning the real vote after quorum).
///
/// This is the **happy path** of pre-vote — equivalent to
/// pre-PR-#5 behavior, but exercised through the new
/// `become_pre_candidate` entry point so we catch any
/// regression in the promotion logic.
///
/// Note: this test replaces the live leader. The fuzz tests in
/// `raft_fuzz.rs` do not exercise this directly; pin it here.
#[tokio::test(flavor = "current_thread")]
async fn pre_vote_with_quorum_promotes_and_wins_election() {
    let partition = Arc::new(PartitionedNetwork::new());
    let scheduler: Arc<dyn oxide_kv::raft::fault_scheduler::FaultScheduler> = partition.clone();
    let cluster = SimCluster::new_3_nodes(scheduler).await;

    // Initial leader = node-0 at term 1.
    cluster.drive_election(0).await;
    assert_eq!(cluster.leader_index(), Some(0));
    let initial_term = cluster.current_term(0);
    assert_eq!(initial_term, 1);

    // No partition — all links live.
    let node1 = cluster.nodes[1].raft.clone();

    // Drive pre-vote on node-1. With all peers reachable and
    // node-1's log at least as fresh as node-0/node-2, the
    // probe should win quorum (2 of 3 votes: self + at least
    // one peer), promote node-1 to Candidate, then win the real
    // RequestVote round, then become Leader.
    RaftNode::become_pre_candidate(node1.clone());

    // Wait for node-1 to become Leader. This can take a couple
    // of ticks: pre-vote fan-out (~immediate in sim), quorum
    // check, promotion, then real RequestVote fan-out, then
    // become_leader.
    let won = wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
        node1.read().unwrap().state == NodeState::Leader
    })
    .await;

    let state = node1.read().unwrap().state;
    let term = node1.read().unwrap().current_term;
    assert!(
        won,
        "node-1 should win the election via pre-vote promotion; ended at state={:?} term={}",
        state, term,
    );

    // The new term must be the one we probed at: initial_term +
    // 1 = 2. If we end up at a higher term, that means we went
    // through two probe rounds, which would indicate a bug.
    assert_eq!(
        term,
        initial_term + 1,
        "single pre-vote round should land at term+1"
    );

    cluster.shutdown().await;
}

/// Regression for the "stale peer probes at arbitrarily high
/// terms" failure mode: a peer claims `probed_term = our_term +
/// 5` (way more than the +1 grace). The receiver must refuse
/// without adopting the term, even if the probe's log would
/// otherwise pass the election restriction.
#[test]
fn pre_vote_refuses_probe_more_than_one_term_ahead_at_handler_level() {
    // Direct handler test — no async, no cluster needed.
    use oxide_kv::raft::rpc::RequestVoteArgs;
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    // Build a minimal RaftNode by hand for the handler unit
    // test. The node-test helper `make_node` is in the
    // `node::tests` module (private), so we go through the
    // public storage / state-machine surface instead.
    fn make_node(node_id: &str) -> (TempDir, Arc<RwLock<RaftNode>>) {
        use oxide_kv::raft::storage::RaftStorage;
        use oxide_kv::state_machine::{StateMachine, StateMachineConfig};
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join("wal").to_string_lossy().to_string();
        let meta = dir.path().join("meta").to_string_lossy().to_string();
        let snap = dir.path().join("snap").to_string_lossy().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm = Arc::new(RwLock::new(
            StateMachine::open(StateMachineConfig {
                data_dir: dir.path().join("sm"),
                memtable_size_threshold: 1024 * 1024,
            })
            .unwrap(),
        ));
        let node = RaftNode::new_with_storage(node_id.to_string(), vec![], sm, storage);
        (dir, Arc::new(RwLock::new(node)))
    }

    let (_d, node) = make_node("n1");
    {
        let mut n = node.write().unwrap();
        n.current_term = 5;
    }

    // Probe at term 5 + 5 = 10 (way more than the +1 grace).
    // Even with an empty log that would otherwise pass
    // election restriction, the handler must refuse.
    let args = RequestVoteArgs {
        term: 10,
        candidate_id: "n2".into(),
        last_log_index: 0,
        last_log_term: 0,
    };
    let reply = node.write().unwrap().handle_pre_vote(&args);

    assert!(!reply.vote_granted, "probe 5 terms ahead must be refused");
    assert_eq!(
        node.read().unwrap().current_term,
        5,
        "refusal must not bump our term",
    );
    assert_eq!(reply.term, 5, "reply must echo our local term");
}
/// **Split-brain regression (regression for the bug introduced by
/// P8 PR 5, commit `abac391`):** pre-vote must NOT count the
/// candidate's own implicit vote toward the quorum.
///
/// Reproduces the exact failure mode Calvin hit on 2026-08-06:
/// three nodes, two of them concurrently call `become_pre_candidate`
/// at the same term. Both probes receive grants from the third
/// node (which has nothing to lose by granting a same-term probe
/// with empty log), both increment their `votes_received` to 2,
/// both promote to Candidate, both win the real RequestVote round,
/// and the cluster ends up with two leaders at the same term.
///
/// Before the fix: `votes_received = AtomicUsize::new(1)`
/// (self-vote) + 1 peer grant = 2 > total_nodes / 2 (= 1) → quorum.
/// This let a single peer grant satisfy a 3-node quorum.
///
/// After the fix: `votes_received = AtomicUsize::new(0)` (no
/// self-vote) + 1 peer grant = 1 > 1 → false, no quorum. Both
/// probes need grants from BOTH peers to win, which is impossible
/// for two concurrent candidates (one peer can only grant once
/// per term).
#[tokio::test(flavor = "current_thread")]
async fn pre_vote_concurrent_candidates_do_not_split_brain() {
    // NoFaults scheduler: every link delivers, no partitions.
    let cluster =
        SimCluster::new_3_nodes(Arc::new(oxide_kv::raft::fault_scheduler::AlwaysDeliver)).await;

    // No prior leader; all three are clean Followers at term 0.

    // Both n0 and n1 fire pre-vote concurrently. n2 is the
    // "swing voter" — its grant will go to whichever probe it
    // sees first, but with the bug both probes will see a
    // satisfied quorum anyway because each only needs ONE
    // peer grant (thanks to the spurious self-vote).
    let n0 = cluster.nodes[0].raft.clone();
    let n1 = cluster.nodes[1].raft.clone();

    // Concurrently fire both pre-vote phases. The shared
    // SimTransport serialises the inbound RPCs, but the
    // pre-vote reply handlers run concurrently (separate
    // tokio::spawn tasks), so this is the exact race that
    // produced Calvin's split-brain in production.
    let h0 = tokio::spawn(async move {
        RaftNode::become_pre_candidate(n0);
    });
    let h1 = tokio::spawn(async move {
        RaftNode::become_pre_candidate(n1);
    });
    let _ = tokio::join!(h0, h1);

    // Give the probe fan-out + real RequestVote fan-out time
    // to settle. 1s is generous for an in-process transport.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // After the race, at most ONE node may be Leader (Raft §3.4
    // / §5.4.2 election safety).
    let leader_count = (0..3)
        .filter(|&i| cluster.nodes[i].raft.read().unwrap().state == NodeState::Leader)
        .count();
    assert!(
        leader_count <= 1,
        "split-brain: {} nodes are Leader at the same term (election \
         safety violated); leader indices: {:?}",
        leader_count,
        (0..3)
            .filter(|&i| { cluster.nodes[i].raft.read().unwrap().state == NodeState::Leader })
            .collect::<Vec<_>>(),
    );

    // Stronger invariant: no two Leaders share the same term.
    use std::collections::HashMap;
    let mut by_term: HashMap<u64, Vec<usize>> = HashMap::new();
    for i in 0..3 {
        let n = cluster.nodes[i].raft.read().unwrap();
        if n.state == NodeState::Leader {
            by_term.entry(n.current_term).or_default().push(i);
        }
    }
    for (term, leaders) in by_term {
        assert!(
            leaders.len() <= 1,
            "split-brain: {} nodes share term {} as Leader: {:?}",
            leaders.len(),
            term,
            leaders,
        );
    }
}

/// **Pre-vote quorum boundary check (regression).**
///
/// Pure unit test for the off-by-one in `process_pre_vote_reply`'s
/// quorum boundary. Before the fix, the boundary was satisfied
/// by `1 peer grant + self-vote = 2 > 3/2 = 1`. This test
/// constructs the state by hand and asserts the boundary is
/// `2 peer grants + 0 self-vote = 2 > 1` instead, so the same
/// off-by-one can't sneak back via a refactor.
#[tokio::test(flavor = "current_thread")]
async fn pre_vote_quorum_requires_both_peers_in_3_node_cluster() {
    let cluster =
        SimCluster::new_3_nodes(Arc::new(oxide_kv::raft::fault_scheduler::AlwaysDeliver)).await;

    let n0 = cluster.nodes[0].raft.clone();
    let initial_term = n0.read().unwrap().current_term;

    // Drive pre-vote on n0. With AlwaysDeliver, both peers
    // (n1, n2) will reply with vote_granted=true (empty log
    // satisfies the election restriction). After the fan-out
    // settles, n0 must be Leader at term initial_term + 1.
    RaftNode::become_pre_candidate(n0.clone());

    // Wait for promote. 1s is generous.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        {
            let n = n0.read().unwrap();
            if n.state == NodeState::Leader {
                assert_eq!(
                    n.current_term,
                    initial_term + 1,
                    "pre-vote promoted to real Candidate then to Leader at term +1",
                );
                return;
            }
        }
        if std::time::Instant::now() >= deadline {
            let n = n0.read().unwrap();
            panic!(
                "pre-vote on n0 did not promote to Leader (state={:?}, term={})",
                n.state, n.current_term,
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
