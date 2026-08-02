//! Deterministic simulation tests (DST) for Raft log conflict
//! resolution.
//!
//! PR #23 covered leader failover + partition heal on a 3-node
//! cluster. PR #24 covers the harder invariant: when two
//! partitions each elect a leader and write divergent logs, what
//! happens when the partition heals? Raft's §5.4 property
//! ("Leader Completeness") plus the AppendEntries consistency
//! check guarantee that only the higher-term leader's log
//! survives, and every follower truncates its divergent prefix.
//!
//! These scenarios would be hard to test on real sockets:
//! - You'd need to inject asymmetric partitions (iptables rules
//!   per-direction, which gets you booted from most CI hosts).
//! - You'd need to keep two leaders alive simultaneously.
//! - You'd need to wait for several election timeouts to elapse
//!   (3-5 seconds each) before the post-partition election fires.
//! The SimHarness compresses all of that into ~1 second.

use oxide_kv::raft::fault_scheduler::{AlwaysDeliver, LinkId, PartitionedNetwork};
use oxide_kv::raft::sim_harness::SimCluster;
use std::sync::Arc;
use std::time::Duration;

/// §5.4 split-brain recovery: old leader appends uncommitted
/// entries during a partition, the other partition elects a
/// new leader, after heal the old leader truncates its
/// divergent log and catches up.
///
/// Sequence:
/// 1. Elect n0; commit entry "a" on all 3 nodes (index 1).
/// 2. Partition n0 <-> n1 and n0 <-> n2 (n0 fully isolated).
/// 3. n0 (still thinks it's leader) appends "b" (index 2,
///    uncommitted on n0 only).
/// 4. n1 wins a post-partition election for term 2.
/// 5. n1 appends "c" (index 2 from n1's perspective — same
///    global index, different content). n1 + n2 commit "c".
/// 6. Heal.
/// 7. n0 receives AppendEntries from n1 (term 2 > n0.term=1) —
///    n0 steps down to follower and truncates its log to
///    match n1's prefix.
/// 8. n0 catches up: it now has [a, c] like n1 and n2.
///
/// After this test:
/// - All 3 nodes' logs are identical: [Set a, Set c].
/// - The divergent "b" entry (only on n0) is gone.
#[tokio::test]
async fn dst_split_brain_old_leader_truncates_divergent_log() {
    let scheduler = Arc::new(PartitionedNetwork::new());
    let cluster = SimCluster::new_3_nodes(scheduler.clone()).await;
    cluster.drive_election(0).await;

    // Step 1: commit "a" on all 3 nodes.
    let idx1 = cluster.submit_set(0, "a", "1");
    assert_eq!(idx1, 1);
    cluster
        .wait_for_replication(idx1, Duration::from_secs(5))
        .await;

    // Step 2: fully isolate n0.
    scheduler.partition(LinkId::new("n0", "n1"));
    scheduler.partition(LinkId::new("n0", "n2"));
    scheduler.partition(LinkId::new("n1", "n0"));
    scheduler.partition(LinkId::new("n2", "n0"));

    // Step 3: n0 (unaware it's isolated) appends "b". This
    // only lands in n0's log; n1/n2 never see it.
    let n0_idx_b = cluster.submit_set(0, "b", "2");
    assert_eq!(n0_idx_b, 2, "n0's log should have a 2nd entry");
    // Give the append a moment to land (it's synchronous but
    // the network call returns immediately).
    tokio::time::sleep(Duration::from_millis(50)).await;
    // n0's log has 2 entries; n1/n2 still have only 1.
    let n0_log_len = cluster.nodes[0].raft.read().unwrap().log.len();
    let n1_log_len = cluster.nodes[1].raft.read().unwrap().log.len();
    let n2_log_len = cluster.nodes[2].raft.read().unwrap().log.len();
    assert_eq!(n0_log_len, 2, "n0 has 'b' (uncommitted)");
    assert_eq!(n1_log_len, 1, "n1 hasn't received 'b'");
    assert_eq!(n2_log_len, 1, "n2 hasn't received 'b'");

    // Step 4: n1 wins a post-partition election.
    cluster.drive_election(1).await;
    // Verify n1 is the post-partition leader by its state +
    // term, not by leader_index() (which can still return n0
    // until n0 receives n1's first AE).
    {
        let n1 = cluster.nodes[1].raft.read().unwrap();
        assert_eq!(n1.state, oxide_kv::raft::node::NodeState::Leader);
        assert!(n1.current_term > 1);
    }

    // Step 5: n1 (new leader) appends "c" and commits it on
    // n1 + n2.
    let n1_idx_c = cluster.submit_set(1, "c", "3");
    assert_eq!(
        n1_idx_c, 2,
        "n1's index 2 is 'c' (n1 doesn't see n0's 'b')"
    );
    cluster
        .wait_for_replication_except(n1_idx_c, &[0], Duration::from_secs(5))
        .await;

    // n1 + n2 now have [a, c]. n0 still has [a, b].
    assert_eq!(cluster.nodes[1].raft.read().unwrap().log.len(), 2);
    assert_eq!(cluster.nodes[2].raft.read().unwrap().log.len(), 2);

    // Step 6: heal.
    scheduler.heal();

    // n1 should be the sole leader after heal. (We need
    // to wait for n0 to step down before checking this.)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let leader = cluster.leader_index();
        if leader == Some(1) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("leader_index never settled on n1, got {:?}", leader);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Step 7+8: wait for n0 to step down and catch up. n0's
    // log should be truncated to [a, c] (matching n1/n2) and
    // its last_applied should reach index 2.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n0_log = &cluster.nodes[0].raft.read().unwrap().log;
        let n0_applied = cluster.nodes[0].raft.read().unwrap().last_applied;
        let n0_term = cluster.nodes[0].raft.read().unwrap().current_term;
        // Truncation done when n0's log has 2 entries and
        // neither of them is "b".
        let has_b = n0_log.iter().any(|e| {
            matches!(
                &e.command,
                oxide_kv::protocol::Command::Set { key, .. } if key == "b"
            )
        });
        if !has_b && n0_log.len() == 2 && n0_applied >= 2 && n0_term >= 2 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "n0 did not converge after heal: log_len={}, has_b={}, last_applied={}, term={}",
                n0_log.len(), has_b, n0_applied, n0_term
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Final invariant: every node has the same KV state.
    assert_eq!(cluster.read(0, "a"), Some("1".to_string()));
    assert_eq!(cluster.read(0, "c"), Some("3".to_string()));
    assert_eq!(
        cluster.read(0, "b"),
        None,
        "n0's divergent 'b' should be gone"
    );
    assert_eq!(cluster.read(1, "a"), Some("1".to_string()));
    assert_eq!(cluster.read(1, "c"), Some("3".to_string()));
    assert_eq!(cluster.read(2, "a"), Some("1".to_string()));
    assert_eq!(cluster.read(2, "c"), Some("3".to_string()));

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// §5.4 divergent log: two different leaders in two different
/// partitions each append entries; the lower-term leader's
/// uncommitted entries get truncated when the partition heals.
///
/// Sequence:
/// 1. n0 elected for term 1; commit "a" on all 3 nodes.
/// 2. Partition n0 <-> {n1, n2}. (n0 isolated.)
/// 3. n0 appends "b" (its log: [a, b], uncommitted).
/// 4. n1 + n2 form a majority partition. n1 wins election for
///    term 2 (n1 + n2's votes = 2/3 majority).
/// 5. n1 appends "c" (its log: [a, c]). n1 + n2 commit "c".
/// 6. Heal.
/// 7. n0 receives AppendEntries from n1 (term=2 > n0.term=1).
///    n0 steps down to Follower, truncates its log back to
///    n1's prefix (length 1, just "a").
/// 8. n0 catches up: appends "c", reaching [a, c] like n1/n2.
///
/// Final state:
/// - All 3 nodes have logs [a, c] (same as n1/n2's log).
/// - The divergent "b" entry (only on n0) is gone everywhere.
///
/// Note: this is technically not a "three-way" divergence — n2
/// always agrees with n1 in this scenario. A real
/// three-way divergence (n0/n1/n2 each having different logs
/// concurrently) requires 5 nodes or a much more elaborate
/// scenario. The Raft invariant we exercise here is the
/// §5.4 AppendEntries consistency check + Leader Completeness,
/// which is sufficient to demonstrate the safety property.
#[tokio::test]
async fn dst_divergent_log_higher_term_wins() {
    let scheduler = Arc::new(PartitionedNetwork::new());
    let cluster = SimCluster::new_3_nodes(scheduler.clone()).await;
    cluster.drive_election(0).await;

    // Step 1.
    let idx_a = cluster.submit_set(0, "a", "1");
    assert_eq!(idx_a, 1);
    cluster
        .wait_for_replication(idx_a, Duration::from_secs(5))
        .await;

    // Step 2: isolate n0.
    scheduler.partition(LinkId::new("n0", "n1"));
    scheduler.partition(LinkId::new("n0", "n2"));
    scheduler.partition(LinkId::new("n1", "n0"));
    scheduler.partition(LinkId::new("n2", "n0"));

    // Step 3: n0 appends "b".
    let _ = cluster.submit_set(0, "b", "2");
    tokio::time::sleep(Duration::from_millis(50)).await;
    // n0 has 2 entries; n1/n2 still have 1.
    assert_eq!(cluster.nodes[0].raft.read().unwrap().log.len(), 2);
    assert_eq!(cluster.nodes[1].raft.read().unwrap().log.len(), 1);
    assert_eq!(cluster.nodes[2].raft.read().unwrap().log.len(), 1);

    // Step 4: n1 wins (n1 + n2 votes = 2/3 majority).
    cluster.drive_election(1).await;
    // Verify n1 is leader by state + term (not leader_index,
    // which can return n0 during partition-heal window).
    {
        let n1 = cluster.nodes[1].raft.read().unwrap();
        assert_eq!(n1.state, oxide_kv::raft::node::NodeState::Leader);
        assert!(n1.current_term > 1);
    }

    // Step 5: n1 appends "c", commits on n1 + n2.
    let idx_c = cluster.submit_set(1, "c", "3");
    assert_eq!(idx_c, 2, "n1's index 2 is 'c'");
    cluster
        .wait_for_replication_except(idx_c, &[0], Duration::from_secs(5))
        .await;

    // Step 6: heal.
    scheduler.heal();

    // Step 7+8: wait for n0 to converge.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n0 = cluster.nodes[0].raft.read().unwrap();
        if n0.current_term >= 2
            && n0.state == oxide_kv::raft::node::NodeState::Follower
            && n0.log.len() == 2
            && n0.last_applied >= 2
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "n0 did not converge: term={}, state={:?}, log_len={}, last_applied={}",
                n0.current_term, n0.state, n0.log.len(), n0.last_applied
            );
        }
        drop(n0);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Final invariant: all 3 nodes have the same KV state.
    for i in 0..3 {
        assert_eq!(
            cluster.read(i, "a"),
            Some("1".to_string()),
            "node {} should have 'a'",
            i
        );
        assert_eq!(
            cluster.read(i, "c"),
            Some("3".to_string()),
            "node {} should have 'c'",
            i
        );
        assert_eq!(
            cluster.read(i, "b"),
            None,
            "node {} should NOT have 'b' (divergent log truncated)",
            i
        );
    }

    // All logs should be identical (length 2).
    let n0_len = cluster.nodes[0].raft.read().unwrap().log.len();
    let n1_len = cluster.nodes[1].raft.read().unwrap().log.len();
    let n2_len = cluster.nodes[2].raft.read().unwrap().log.len();
    assert_eq!(n0_len, 2);
    assert_eq!(n1_len, 2);
    assert_eq!(n2_len, 2);

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// §5.2 term rollback on stale leader: the old leader steps
/// down on the next heartbeat, learns the new term, and never
/// applies its uncommitted log entries.
///
/// Sequence:
/// 1. n0 elected for term 1; commit "a".
/// 2. Partition n0 <-> {n1, n2}.
/// 3. n0 appends "b" (uncommitted, only on n0).
/// 4. n0 appends "c" (uncommitted, only on n0 — its log is
///    now [a, b, c]).
/// 5. n1 wins election for term 2.
/// 6. Heal.
/// 7. n0 receives AppendEntries from n1 (term=2 > n0.term=1).
///    n0 must:
///    - Update its term to 2
///    - Step down to Follower
///    - Truncate its log back to n1's prefix (index 1)
/// 8. Note: log truncation is *not* asserted here. Raft's
///    safety guarantee is that uncommitted entries are never
///    applied, not that they're deleted from the log
///    immediately. Log GC happens via snapshot/compaction in
///    production. Truncating the divergent prefix requires
///    the leader to backtrack next_index on consistency-check
///    failure; that path is exercised by
///    [`Self::dst_split_brain_old_leader_truncates_divergent_log`]
///    above where the old leader's 'b' does get truncated.
#[tokio::test]
async fn dst_stale_leader_steps_down_and_does_not_apply_uncommitted() {
    let scheduler = Arc::new(PartitionedNetwork::new());
    let cluster = SimCluster::new_3_nodes(scheduler.clone()).await;
    cluster.drive_election(0).await;

    let _ = cluster.submit_set(0, "a", "1");
    cluster
        .wait_for_replication(1, Duration::from_secs(5))
        .await;

    // Isolate n0.
    scheduler.partition(LinkId::new("n0", "n1"));
    scheduler.partition(LinkId::new("n0", "n2"));
    scheduler.partition(LinkId::new("n1", "n0"));
    scheduler.partition(LinkId::new("n2", "n0"));

    // n0 appends b, c.
    let _ = cluster.submit_set(0, "b", "2");
    let _ = cluster.submit_set(0, "c", "3");
    // n0's log now has 3 entries: [a, b, c]. n1/n2 still have 1.
    assert_eq!(cluster.nodes[0].raft.read().unwrap().log.len(), 3);
    assert_eq!(cluster.nodes[1].raft.read().unwrap().log.len(), 1);
    assert_eq!(cluster.nodes[2].raft.read().unwrap().log.len(), 1);

    // n1 wins.
    cluster.drive_election(1).await;
    {
        let n1 = cluster.nodes[1].raft.read().unwrap();
        assert_eq!(n1.state, oxide_kv::raft::node::NodeState::Leader);
        assert!(n1.current_term > 1);
    }

    // Heal.
    scheduler.heal();

    // Wait for n0 to step down (state + term). We do NOT
    // assert log_len == 1: Raft's safety property is that
    // uncommitted entries are never applied, not that they're
    // removed from the log immediately.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n0 = cluster.nodes[0].raft.read().unwrap();
        if n0.current_term >= 2
            && n0.state == oxide_kv::raft::node::NodeState::Follower
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "n0 did not step down: term={}, state={:?}",
                n0.current_term, n0.state
            );
        }
        drop(n0);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // n0 should not still be Leader.
    assert!(
        cluster.nodes[1].raft.read().unwrap().state
            == oxide_kv::raft::node::NodeState::Leader
            && cluster.current_term(1) >= 2,
        "n1 should be the post-partition leader"
    );
    assert_eq!(
        cluster.read(0, "a"),
        Some("1".to_string())
    );
    assert_eq!(
        cluster.read(0, "b"),
        None,
        "n0 must not have applied uncommitted 'b'"
    );
    assert_eq!(
        cluster.read(0, "c"),
        None,
        "n0 must not have applied uncommitted 'c'"
    );

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// §5.3 minority partition never recovers (sanity): a single
/// isolated node cannot make progress on its own. When it
/// heals back, it must catch up to whatever the majority
/// committed in the meantime.
///
/// Sequence:
/// 1. n0 elected; commit "a".
/// 2. Partition n2 <-> {n0, n1} (n2 is fully isolated).
/// 3. n0 commits "b" (replicated to n1 only; n2 doesn't know).
/// 4. n0 commits "c" (replicated to n1 only; n2 doesn't know).
/// 5. Heal.
/// 6. n2 receives AppendEntries from n0 (or via n1's AE that
///    includes leader_commit=3). n2 catches up: log becomes
///    [a, b, c], applied >= 3.
#[tokio::test]
async fn dst_minority_isolated_node_catches_up_on_heal() {
    let scheduler = Arc::new(PartitionedNetwork::new());
    let cluster = SimCluster::new_3_nodes(scheduler.clone()).await;
    cluster.drive_election(0).await;

    // Commit "a" everywhere.
    let idx_a = cluster.submit_set(0, "a", "1");
    cluster
        .wait_for_replication(idx_a, Duration::from_secs(5))
        .await;

    // Isolate n2 (minority).
    scheduler.partition(LinkId::new("n0", "n2"));
    scheduler.partition(LinkId::new("n1", "n2"));
    scheduler.partition(LinkId::new("n2", "n0"));
    scheduler.partition(LinkId::new("n2", "n1"));

    // Majority commits b, c (n2 doesn't know).
    let idx_b = cluster.submit_set(0, "b", "2");
    assert_eq!(idx_b, 2);
    let idx_c = cluster.submit_set(0, "c", "3");
    assert_eq!(idx_c, 3);
    // Wait for n1 to catch up.
    cluster
        .wait_for_replication_except(idx_c, &[2], Duration::from_secs(5))
        .await;
    // n2's log is still at 1.
    assert_eq!(cluster.nodes[2].raft.read().unwrap().log.len(), 1);
    assert_eq!(cluster.nodes[2].raft.read().unwrap().last_applied, 1);

    // Heal.
    scheduler.heal();

    // Wait for n2 to catch up.
    cluster
        .wait_for_replication(idx_c, Duration::from_secs(10))
        .await;

    // All 3 nodes have a, b, c.
    for i in 0..3 {
        assert_eq!(cluster.read(i, "a"), Some("1".to_string()));
        assert_eq!(cluster.read(i, "b"), Some("2".to_string()));
        assert_eq!(cluster.read(i, "c"), Some("3".to_string()));
    }

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}

/// Sanity: a no-partition cluster always converges. This is
/// the simplest possible divergent-log scenario (zero
/// divergence) and is here as a control: if this fails, the
/// harness itself is broken, not Raft.
#[tokio::test]
async fn dst_no_partition_baseline_converges() {
    let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
    cluster.drive_election(0).await;

    let idx1 = cluster.submit_set(0, "a", "1");
    let idx2 = cluster.submit_set(0, "b", "2");
    let idx3 = cluster.submit_set(0, "c", "3");

    cluster
        .wait_for_replication(idx3, Duration::from_secs(5))
        .await;

    for i in 0..3 {
        assert_eq!(cluster.read(i, "a"), Some("1".to_string()));
        assert_eq!(cluster.read(i, "b"), Some("2".to_string()));
        assert_eq!(cluster.read(i, "c"), Some("3".to_string()));
    }
    assert_eq!(idx1, 1);
    assert_eq!(idx2, 2);
    assert_eq!(idx3, 3);

    // P7 safety invariants: every teardown runs the
    // four-invariants check so any latent safety bug
    // surfaces here, not in a future test.
    oxide_kv::raft::invariants::assert_invariants(&cluster)
        .expect("safety invariants violated at teardown");
    cluster.shutdown().await;
}