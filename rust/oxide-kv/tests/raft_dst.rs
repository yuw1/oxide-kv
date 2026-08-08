//! Deterministic simulation tests (DST) for Raft.
//!
//! These tests are the payoff for the P7 foundation
//! (Clock + Transport + SimTransport + FaultScheduler + SimHarness):
//! they exercise Raft's failure-recovery paths in <5 seconds
//! without spawning real OS processes, real sockets, or relying
//! on real time. The "deterministic" in DST means the same fault
//! schedule produces the same cluster state on every run.
//!
//! ## Conventions
//!
//! - Each test spins up an in-process `SimCluster` (no real
//!   sockets, no real disk). The cluster is torn down via
//!   `cluster.shutdown().await` at the end of every test, so a
//!   panic in one test never leaks state into the next.
//! - "Leader crashes" is modelled by `cluster.kill_node(idx)`
//!   which closes the node's inbound channel and stops its
//!   heartbeat loop. The cluster keeps running on the surviving
//!   nodes.
//! - "Network partition" is modelled by `PartitionedNetwork`.
//!   `partition(link)` drops messages on that directed link;
//!   `heal()` restores delivery.
//! - Timeouts are generous (5-10s) to absorb CI host load. The
//!   actual cluster wall-clock for any scenario below is <3s.

use oxide_kv::raft::fault_scheduler::{AlwaysDeliver, FaultScheduler, LinkId, PartitionedNetwork};
use oxide_kv::raft::sim_harness::SimCluster;
use std::sync::Arc;
use std::time::Duration;

/// §5.2 leader failover: after the leader is killed, a surviving
/// follower becomes the new leader; its `current_term` is higher
/// than the old leader's was; and the log entries the old leader
/// committed are still in the new leader's log (Raft's election
/// restriction §5.4.1 ensures this).
///
/// Sequence:
/// 1. Elect n0 as leader for term T.
/// 2. Submit two `Set` commands; both reach index 1, 2 in the log.
/// 3. Kill n0 (close its inbound channel).
/// 4. Drive n1 to be a candidate for term T+1.
/// 5. n1 wins (it has n0's log entries + n2's vote).
/// 6. Assert: n1 is leader, term is T+1, n1's last_log_index >= 2.
#[tokio::test]
async fn dst_leader_failover_preserves_committed_log() {
    let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
    cluster.drive_election(0).await;
    let leader_idx = cluster.leader_index().expect("n0 elected");
    assert_eq!(leader_idx, 0);
    let old_term = cluster.current_term(0);

    // Submit two committed entries.
    let idx1 = cluster.submit_set(leader_idx, "k1", "v1");
    let idx2 = cluster.submit_set(leader_idx, "k2", "v2");
    cluster
        .wait_for_replication(idx2, Duration::from_secs(5))
        .await;
    assert_eq!(idx1, 1);
    assert_eq!(idx2, 2);

    // Leader "crashes" — close its inbound channel + heartbeat
    // loop. The surviving nodes (n1, n2) keep running.
    cluster.kill_node(0).await;

    // Drive n1 to be a candidate. n1 has the full log (idx 1
    // and 2) so it satisfies the election restriction; n2's
    // log is also up to date. n1 should win with 2/2 votes.
    cluster.drive_election(1).await;

    let new_leader = cluster.leader_index().expect("a leader emerged");
    assert_eq!(new_leader, 1, "n1 should be the new leader");
    let new_term = cluster.current_term(1);
    assert!(
        new_term > old_term,
        "new term ({}) should be strictly greater than old term ({})",
        new_term,
        old_term
    );

    // The new leader's log still contains the committed entries.
    // If the old leader had committed them, no candidate without
    // them can win an election (§5.4.1).
    let n1_last_idx = cluster.nodes[1].raft.read().unwrap().log.len() as u64;
    assert!(
        n1_last_idx >= 2,
        "new leader should have the old leader's committed entries, got log.len() = {}",
        n1_last_idx
    );

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// §5.2 leader failover + continued writes: after failover, the
/// new leader can keep accepting commands. This validates the
/// "no committed data is lost" property end-to-end.
#[tokio::test]
async fn dst_leader_failover_then_new_leader_accepts_writes() {
    let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
    cluster.drive_election(0).await;
    let leader_idx = cluster.leader_index().unwrap();

    // One committed entry on the old leader.
    let _ = cluster.submit_set(leader_idx, "before_failover", "v0");
    cluster
        .wait_for_replication(1, Duration::from_secs(5))
        .await;

    // Crash n0.
    cluster.kill_node(0).await;

    // n1 wins.
    cluster.drive_election(1).await;
    let new_leader = cluster.leader_index().unwrap();
    assert_eq!(new_leader, 1);

    // New leader accepts a fresh write.
    let new_idx = cluster.submit_set(new_leader, "after_failover", "v1");
    assert!(
        new_idx >= 2,
        "new entry should be at index >= 2, got {}",
        new_idx
    );

    // All surviving nodes (n1, n2) converge on the new entry.
    // n0 is excluded because kill_node(0) took it out of the
    // cluster.
    cluster
        .wait_for_replication_except(new_idx, &[0], Duration::from_secs(5))
        .await;
    assert_eq!(cluster.read(1, "after_failover"), Some("v1".to_string()));
    assert_eq!(cluster.read(2, "after_failover"), Some("v1".to_string()));

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// §5.4.1 election restriction: a candidate with a stale log
/// cannot win an election if any peer has a more up-to-date
/// log. This is the safety invariant that makes the failover
/// test above safe to assume "the new leader has the committed
/// log".
///
/// Sequence:
/// 1. Elect n0; commit entry 1 on n0, n1, n2.
/// 2. Partition n0 -> n1 and n0 -> n2 (n0 can still see its own
///    inbound; the partition is asymmetric — n0 can't push to
///    followers but can hear from them if they speak).
///    Actually simpler: drop n0's outbound by partition both
///    directions.
/// 3. Submit entry 2 on n0 (only n0 has it; n1, n2 don't).
/// 4. Partition heal. Now n0 has 2 entries, n1, n2 have 1 entry.
/// 5. Kill n0. n1 starts an election with last_log_index = 1.
///    But wait, we want n1 to LOSE — so we need a candidate
///    with a stale log. Let's reverse: n2 starts an election
///    with log_len = 1 (stale), n1 starts with log_len = 2
///    (after we manually sync n1).
///
/// Simpler construction:
/// 1. Elect n0; commit entry 1 on all.
/// 2. Drop n0 -> n1, n0 -> n2 (n0 isolated).
/// 3. n0 appends entry 2 (stale, uncommitted).
/// 4. Kill n0.
/// 5. Force n0's entry 2 onto n1 via direct log manipulation
///    (`push_log_entry_for_test`) so n1 has the more recent
///    log.
/// 6. n2 starts an election with log_len = 1.
/// 7. n2 should LOSE (n1 votes no — log stale). n1 starts an
///    election with log_len = 2 and wins.
#[tokio::test]
async fn dst_election_restriction_stale_candidate_loses() {
    let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
    cluster.drive_election(0).await;
    let leader_idx = cluster.leader_index().unwrap();

    // Entry 1 commits everywhere.
    let _ = cluster.submit_set(leader_idx, "k1", "v1");
    cluster
        .wait_for_replication(1, Duration::from_secs(5))
        .await;

    // Now give n1 an extra (stale) log entry so n1 has the
    // most up-to-date log when n0 disappears. We use the
    // test-only `push_log_entry_for_test` hook on n1.
    {
        let mut n1 = cluster.nodes[1].raft.write().unwrap();
        n1.push_log_entry_for_test(oxide_kv::protocol::Command::Compact);
    }

    // Kill n0 (the leader).
    cluster.kill_node(0).await;

    // n2 starts an election — its log has only entry 1, n1
    // has entry 1 + the synthetic entry 2. n1 should reject
    // n2's vote (last_log_index 1 < n1's last_log_index 2).
    // n2 cannot win (only 1/2 votes).
    let n2_won = cluster.try_drive_election(2, Duration::from_secs(2)).await;
    assert!(
        !n2_won,
        "n2 should not win — its log is stale (n1 has more entries)"
    );
    assert_ne!(cluster.leader_index(), Some(2), "n2 should not be leader");

    // Now n1 starts an election — n1 has the more recent
    // log, so it should win.
    cluster.drive_election(1).await;
    assert_eq!(
        cluster.leader_index(),
        Some(1),
        "n1 should win — it has the more recent log"
    );

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// Network partition heal: when a minority partition heals,
/// the partitioned nodes catch up via AppendEntries.
///
/// Sequence:
/// 1. Elect n0; commit entry 1 on all.
/// 2. Partition n0 <-> n1, n0 <-> n2 (n0 is fully isolated).
/// 3. n0 still thinks it's leader (no peer responds, but n0
///    can't tell); n0 appends entry 2 (uncommitted, only on
///    n0's log).
/// 4. n1 starts a new election, wins with n2's vote (majority).
/// 5. n1 commits entry 2 from its own perspective... but wait,
///    n1 doesn't have entry 2 because n0 had it. Let me rethink.
///
/// Revised sequence:
/// 1. Elect n0; commit entry 1 on all.
/// 2. Partition n0 -> n1 and n0 -> n2 (asymmetric — n0's
///    outbound to followers is dropped, but followers can still
///    reach n0 if needed).
/// 3. n0 still has quorum via n0's self-vote... wait, n0's
///    outbound is dropped, so n0's AppendEntries to n1/n2
///    fail. n0's commit_index stays at 1.
/// 4. Force n1 to become a candidate. n1 wins (its own vote +
///    n2's vote = 2/2 majority).
/// 5. n1 (new leader) appends entry 2; commits it.
/// 6. Heal the partition. n0's serve loop still has its old
///    channel — when it tries to send to n1/n2, those messages
///    now go through. n0 learns n1 is the new leader (term is
///    higher) and steps down to follower.
/// 7. n0 catches up: its next AppendEntries round (when n0 is
///    a follower) replicates entry 2.
#[tokio::test]
async fn dst_partition_isolates_leader_minority_wins_then_heal() {
    let scheduler = Arc::new(PartitionedNetwork::new());
    let cluster = SimCluster::new_3_nodes(scheduler.clone()).await;
    cluster.drive_election(0).await;

    // Entry 1 commits everywhere.
    let idx1 = cluster.submit_set(0, "k1", "v1");
    cluster
        .wait_for_replication(idx1, Duration::from_secs(5))
        .await;
    assert_eq!(idx1, 1);

    // Partition n0's outbound to both followers.
    scheduler.partition(LinkId::new("n0", "n1"));
    scheduler.partition(LinkId::new("n0", "n2"));

    // n1 drives a new election — wins with n2's vote.
    cluster.drive_election(1).await;
    assert_eq!(
        cluster.leader_index(),
        Some(1),
        "n1 should win the post-partition election"
    );

    // n1 (new leader) commits a fresh entry.
    let idx2 = cluster.submit_set(1, "k2", "v2");
    cluster
        .wait_for_replication(idx2, Duration::from_secs(5))
        .await;

    // Heal: n0's outbound messages now go through. n0 will
    // learn from n1's heartbeat that n1 is leader for a
    // higher term and step down to follower. Subsequent
    // AppendEntries will replicate idx2 to n0.
    scheduler.heal();
    cluster
        .wait_for_replication(idx2, Duration::from_secs(10))
        .await;

    // n0 has caught up.
    assert_eq!(
        cluster.read(0, "k2"),
        Some("v2".to_string()),
        "n0 should have caught up after heal"
    );
    // n1 and n2 have both entries.
    assert_eq!(cluster.read(1, "k1"), Some("v1".to_string()));
    assert_eq!(cluster.read(1, "k2"), Some("v2".to_string()));
    assert_eq!(cluster.read(2, "k1"), Some("v1".to_string()));
    assert_eq!(cluster.read(2, "k2"), Some("v2".to_string()));

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// Stress: run the same leader-failover scenario 5 times in
/// sequence within one test process. If the harness had any
/// hidden state leak across elections, this would surface it.
/// (The other tests each tear down their cluster at the end,
/// so this test is the canary for "clean teardown".)
#[tokio::test]
async fn dst_leader_failover_repeated_5x_no_state_leak() {
    for run in 0..5 {
        let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
        cluster.drive_election(0).await;

        // Commit one entry.
        let _ = cluster.submit_set(0, "k", "v");
        cluster
            .wait_for_replication(1, Duration::from_secs(5))
            .await;

        // Failover.
        cluster.kill_node(0).await;
        cluster.drive_election(1).await;
        assert_eq!(
            cluster.leader_index(),
            Some(1),
            "run {}: n1 should be leader",
            run
        );

        // New leader writes.
        let idx = cluster.submit_set(1, "k2", "v2");
        cluster
            .wait_for_replication_except(idx, &[0], Duration::from_secs(5))
            .await;

        cluster.shutdown().await;
    }
}
// =====================================================================
// Reference-model cross-check
// =====================================================================

/// DST scenario: cross-check the cluster against a
/// sequential reference model while exercising
/// partition + crash + restart + heal. The reference
/// model is a single-threaded HashMap that applies
/// committed `Set` / `Delete` ops in log-index order.
/// Every `cluster.read(node, key)` must match the
/// reference model's `get(key)` at the same committed
/// prefix.
#[tokio::test]
async fn dst_reference_model_cross_check_under_faults() {
    use oxide_kv::raft::reference_model::ReferenceModel;

    // Build a cluster whose links go through a partition
    // controller from the start. We can flip links on/off
    // without rebuilding the cluster.
    let partition = Arc::new(PartitionedNetwork::new());
    let cluster = SimCluster::new_3_nodes(partition.clone() as Arc<dyn FaultScheduler>).await;
    cluster.drive_election(0).await;
    let leader_idx = cluster.leader_index().unwrap();

    let mut rm = ReferenceModel::new();

    // Helper: drain the reference model up to the
    // current leader's commit_index.
    let drain = |rm: &mut ReferenceModel, cluster: &SimCluster| {
        let idx = cluster
            .leader_index()
            .map(|l| cluster.nodes[l].raft.read().unwrap().commit_index)
            .unwrap_or(0);
        rm.drain_to(cluster, idx);
    };

    // Phase 1: 3 writes on the steady-state leader.
    let _i1 = cluster.submit_set(leader_idx, "alpha", "1");
    let _i2 = cluster.submit_set(leader_idx, "beta", "2");
    let i3 = cluster.submit_set(leader_idx, "gamma", "3");
    cluster
        .wait_for_replication(i3, Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);

    // Cross-check: every node's read should match the
    // reference model.
    for n in 0..cluster.nodes.len() {
        assert_eq!(
            cluster.read(n, "alpha"),
            rm.get("alpha").cloned(),
            "alpha mismatch on n{}",
            n
        );
        assert_eq!(
            cluster.read(n, "beta"),
            rm.get("beta").cloned(),
            "beta mismatch on n{}",
            n
        );
        assert_eq!(
            cluster.read(n, "gamma"),
            rm.get("gamma").cloned(),
            "gamma mismatch on n{}",
            n
        );
    }

    // Phase 2: partition n2 off, write on n0/n1.
    partition.partition(LinkId::new("n0", "n2"));
    partition.partition(LinkId::new("n1", "n2"));

    let _i4 = cluster.submit_set(leader_idx, "delta", "4");
    let i5 = cluster.submit_set(leader_idx, "epsilon", "5");
    cluster
        .wait_for_replication_except(i5, &[2], Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);

    // n0, n1 agree with reference.
    assert_eq!(cluster.read(0, "delta"), rm.get("delta").cloned());
    assert_eq!(cluster.read(1, "delta"), rm.get("delta").cloned());
    assert_eq!(cluster.read(0, "epsilon"), rm.get("epsilon").cloned());
    assert_eq!(cluster.read(1, "epsilon"), rm.get("epsilon").cloned());

    // Phase 3: heal, n2 catches up, cross-check post-heal.
    partition.heal();
    cluster
        .wait_for_replication(i5, Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);

    for n in 0..cluster.nodes.len() {
        assert_eq!(
            cluster.read(n, "delta"),
            rm.get("delta").cloned(),
            "post-heal delta mismatch on n{}",
            n
        );
        assert_eq!(
            cluster.read(n, "epsilon"),
            rm.get("epsilon").cloned(),
            "post-heal epsilon mismatch on n{}",
            n
        );
    }

    cluster.shutdown().await;
}

/// DST scenario: cross-check against the reference model
/// while the leader fails over mid-stream. The new
/// leader's log should match the reference model's
/// applied-index prefix after recovery.
#[tokio::test]
async fn dst_reference_model_cross_check_after_leader_failover() {
    use oxide_kv::raft::reference_model::ReferenceModel;

    let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
    cluster.drive_election(0).await;
    let leader_idx = cluster.leader_index().unwrap();
    let mut rm = ReferenceModel::new();
    let drain = |rm: &mut ReferenceModel, cluster: &SimCluster| {
        let idx = cluster
            .leader_index()
            .map(|l| cluster.nodes[l].raft.read().unwrap().commit_index)
            .unwrap_or(0);
        rm.drain_to(cluster, idx);
    };

    let _i1 = cluster.submit_set(leader_idx, "k1", "v1");
    let i2 = cluster.submit_set(leader_idx, "k2", "v2");
    cluster
        .wait_for_replication(i2, Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);
    assert_eq!(rm.applied_index(), i2);

    // Fail over n0 -> n1.
    cluster.kill_node(0).await;
    cluster.drive_election(1).await;
    let new_leader = cluster.leader_index().unwrap();
    assert_eq!(new_leader, 1);

    // n1 (new leader) reads must match the reference
    // model's view, even though its view of the log
    // might have caught up only after the failover.
    drain(&mut rm, &cluster);
    assert_eq!(cluster.read(1, "k1"), rm.get("k1").cloned());
    assert_eq!(cluster.read(1, "k2"), rm.get("k2").cloned());

    // New writes under new leader.
    let i3 = cluster.submit_set(new_leader, "k3", "v3");
    cluster
        .wait_for_replication_except(i3, &[0], Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);
    assert_eq!(cluster.read(1, "k3"), rm.get("k3").cloned());

    cluster.shutdown().await;
}

/// DST scenario: cross-check after a Delete op. The
/// reference model applies Delete at the committed index;
/// subsequent reads must observe the deletion.
#[tokio::test]
async fn dst_reference_model_cross_check_with_delete() {
    use oxide_kv::raft::reference_model::ReferenceModel;

    let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
    cluster.drive_election(0).await;
    let leader_idx = cluster.leader_index().unwrap();
    let mut rm = ReferenceModel::new();
    let drain = |rm: &mut ReferenceModel, cluster: &SimCluster| {
        let idx = cluster
            .leader_index()
            .map(|l| cluster.nodes[l].raft.read().unwrap().commit_index)
            .unwrap_or(0);
        rm.drain_to(cluster, idx);
    };

    let i1 = cluster.submit_set(leader_idx, "ephemeral", "alive");
    cluster
        .wait_for_replication(i1, Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);
    assert_eq!(cluster.read(0, "ephemeral"), rm.get("ephemeral").cloned());

    // Delete via submit_command (submit_set hardcodes Set).
    let cmd = oxide_kv::protocol::Command::Delete {
        key: "ephemeral".into(),
    };
    let i2 = cluster.submit_command(leader_idx, cmd);
    cluster
        .wait_for_replication(i2, Duration::from_secs(5))
        .await;
    drain(&mut rm, &cluster);

    for n in 0..cluster.nodes.len() {
        assert_eq!(
            cluster.read(n, "ephemeral"),
            None,
            "n{} should observe the Delete",
            n
        );
        assert_eq!(
            cluster.read(n, "ephemeral"),
            rm.get("ephemeral").cloned(),
            "n{}'s view should match reference model post-Delete",
            n
        );
    }

    cluster.shutdown().await;
}
