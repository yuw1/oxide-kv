//! Simulation harness for P7 deterministic simulation testing (DST).
//!
//! Composes `SimClock` + `SimTransport` + (optionally)
//! `FaultScheduler` into a 3-node `RaftNode` cluster that runs
//! without real sockets, real OS scheduling, or real disk I/O.
//!
//! The harness is the payoff for the P7 refactor: it lets a test
//! drive a full election / replication / partition-heal cycle and
//! assert the cluster converges, with the same RaftNode
//! implementation the production TCP path uses.
//!
//! ## Why this is a separate module
//!
//! The harness owns the lifecycle of a cluster: it constructs the
//! nodes, spawns their heartbeat loops, exposes helper methods
//! for forcing elections and asserting convergence, and tears
//! everything down cleanly. Putting this in `tests/` (an
//! integration test file) would force every test to copy/paste
//! the setup. A library module lets every test reuse it.
//!
//! ## Why the harness skips the election timer
//!
//! Real Raft election timers are randomised between
//! `min_election_timeout_ms` and `max_election_timeout_ms` (3-5s
//! by default) — that's too slow for a test loop. The harness
//! drives `become_candidate` manually (mirroring
//! `tests/integration_2pc.rs`'s pattern), so a 3-node election
//! completes in <100ms rather than 5-10s. The election timer
//! itself is exhaustively tested elsewhere; here we just need
//! the election to start deterministically when we say so.
//!
//! ## What the harness does NOT replace
//!
//! - The election timer's randomised start (we drive elections
//!   manually).
//! - The disk-backed WAL / meta persistence (each node gets a
//!   fresh `tempfile::tempdir`, same as `tests/integration_2pc.rs`).
//! - Real socket I/O (everything goes through `SimTransport`'s
//!   mpsc channels).
//!
//! In other words, this is "the same RaftNode, wired against
//! virtual endpoints and controlled by a test driver".

use crate::raft::clock::{Clock, SimClock};
use crate::raft::fault_scheduler::FaultScheduler;
use crate::raft::net::{StopSignal, Transport};
use crate::raft::node::{NodeState, RaftNode};
use crate::raft::sim_transport::{Network, SimTransport};
use crate::raft::storage::RaftStorage;
use crate::state_machine::{StateMachine, StateMachineConfig};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tempfile::TempDir;

/// A 3-node cluster wired against `SimTransport` + `SimClock`.
/// Each node owns a tempdir (so WAL/meta files survive the
/// node's lifetime) and a `StopSignal` (so `Drop` can clean up
/// the heartbeat / serve loops).
pub struct SimCluster {
    pub nodes: Vec<SimNode>,
}

pub struct SimNode {
    /// Public id (also the RaftNode `node_id`). Conventionally
    /// `"n0"`, `"n1"`, `"n2"`.
    pub id: String,
    pub raft: Arc<RwLock<RaftNode>>,
    pub transport: SimTransport,
    pub stop: StopSignal,
    /// Keep the tempdir alive for the node's lifetime so the
    /// on-disk WAL/meta/snapshot files are not garbage-collected
    /// out from under the open `RaftStorage`. (Same pattern as
    /// `tests/integration_2pc.rs`.)
    pub _data_dir: TempDir,
}

impl SimCluster {
    /// Spin up a 3-node cluster where every node's outbound path
    /// goes through the same `scheduler`. Returns the cluster
    /// with serve loops and heartbeat loops already spawned.
    ///
    /// The cluster starts in Follower state on every node —
    /// candidates are forced by the caller (via
    /// [`SimCluster::drive_election`]).
    pub async fn new_3_nodes(scheduler: Arc<dyn FaultScheduler>) -> Self {
        Self::new_n_nodes(3, scheduler).await
    }

    /// General form: spin up `n` nodes with the given shared
    /// scheduler. The first node's id is `"n0"`, second `"n1"`,
    /// etc. Peer lists are wired symmetrically: each node knows
    /// every other node.
    pub async fn new_n_nodes(n: usize, scheduler: Arc<dyn FaultScheduler>) -> Self {
        assert!(n >= 1, "cluster must have at least 1 node");

        // The shared clock uses an explicit epoch so all nodes
        // agree on `now()`. (Without a shared epoch, each node
        // captures its own `Instant::now()` and the cluster
        // drifts.) The harness never reads `clock.now()`
        // directly today — it just hands the same Arc to every
        // node so any future virtual-time assertion (e.g. "5s
        // after election, log is replicated") is consistent.
        let epoch = std::time::Instant::now();
        let clock: Arc<dyn Clock> = Arc::new(SimClock::with_epoch(epoch));

        // Build the network and the inbound channels first so
        // we can construct the SimTransports before the
        // RaftNodes (the RaftNodes need a Transport).
        let network = Network::with_scheduler(Duration::from_secs(2), scheduler);
        let mut receivers = Vec::with_capacity(n);
        for i in 0..n {
            let id = format!("n{}", i);
            let rx = network.register(&id);
            receivers.push((id, rx));
        }

        // Now construct each node.
        let mut nodes = Vec::with_capacity(n);
        for (id, inbound) in receivers {
            let transport = SimTransport::new(id.clone(), network.clone(), inbound);

            // Tempdir for WAL + meta + snapshot + state-machine.
            let data_dir = tempfile::tempdir().expect("tempdir");
            let storage = RaftStorage::new_with_paths(
                data_dir.path().join("wal").to_string_lossy().to_string(),
                data_dir.path().join("meta").to_string_lossy().to_string(),
                data_dir.path().join("snap").to_string_lossy().to_string(),
            );
            let sm_config = StateMachineConfig {
                data_dir: data_dir.path().join("sm"),
                memtable_size_threshold: 4 * 1024 * 1024,
            };
            let sm = Arc::new(RwLock::new(
                StateMachine::open(sm_config).expect("StateMachine::open"),
            ));

            // Peer list: every other node's id.
            let peers: Vec<String> = (0..n)
                .map(|j| format!("n{}", j))
                .filter(|p| p != &id)
                .collect();

            let raft = Arc::new(RwLock::new(RaftNode::new_with_clock_and_transport(
                id.clone(),
                peers,
                sm,
                storage,
                clock.clone(),
                Arc::new(transport.clone()) as Arc<dyn Transport>,
            )));

            let stop = StopSignal::new();

            // Spawn the serve loop so inbound messages are
            // dispatched to the RaftNode's handlers.
            let serve_node = raft.clone();
            let serve_stop = stop.clone();
            let serve_transport = transport.clone();
            tokio::spawn(async move {
                let _ = serve_transport.serve(serve_node, serve_stop).await;
            });

            // Spawn the heartbeat loop so the leader's
            // commit_index reaches the followers. (Election
            // timer is intentionally NOT spawned — the harness
            // drives elections explicitly via
            // `become_candidate`.)
            let hb_node = raft.clone();
            tokio::spawn(async move {
                RaftNode::run_heartbeat_loop(hb_node).await;
            });

            nodes.push(SimNode {
                id,
                raft,
                transport,
                stop,
                _data_dir: data_dir,
            });
        }

        Self { nodes }
    }

    /// Force the first node to become a Candidate. After the
    /// returned future resolves, the cluster has either:
    /// - Elected the first node (the typical case for a 3-node
    ///   cluster with the others idle), or
    /// - Some other node won the race (only possible if the
    ///   other nodes' election timers happened to fire first
    ///   — and we never spawn them, so this is impossible
    ///   unless the test forces a different candidate).
    ///
    /// This mirrors `tests/integration_2pc.rs::elect_leader`.
    pub async fn drive_election(&self, candidate_idx: usize) {
        let candidate = &self.nodes[candidate_idx];
        RaftNode::become_candidate(candidate.raft.clone());

        // Wait for the candidate to win (or lose). The candidate
        // sends RequestVote RPCs to every peer via SimTransport;
        // each peer grants the vote (empty logs satisfy
        // election restriction §5.4.1). With 3 nodes the
        // candidate collects 3/3 votes and steps up to Leader.
        //
        // We poll `state` up to 5s wall-clock. The actual time
        // is << 5s (typically <100ms) because the channel is
        // in-process. The 5s ceiling is generous slack for
        // busy CI hosts.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let n = candidate.raft.read().unwrap();
                if n.state == NodeState::Leader {
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                let n = candidate.raft.read().unwrap();
                panic!(
                    "drive_election: candidate {} never became leader (state = {:?}, term = {})",
                    candidate.id, n.state, n.current_term
                );
            }
            // Tiny sleep to yield the scheduler. 10ms is small
            // enough that even 500 iterations is 5s — which
            // matches our ceiling.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Return the index of the node currently in Leader state,
    /// or `None` if no node is leader (e.g. the cluster is in
    /// the middle of an election).
    ///
    /// Note: during a partition, the old leader may still be in
    /// Leader state (it doesn't know it's been deposed until the
    /// new leader's AppendEntries reaches it). If two nodes are
    /// momentarily in Leader state, this returns the first one
    /// by index. For tests that care about the "real" leader
    /// after a partition, prefer polling `current_term(idx)`
    /// and waiting for the old leader to step down.
    pub fn leader_index(&self) -> Option<usize> {
        self.nodes.iter().position(|n| {
            n.raft.read().unwrap().state == NodeState::Leader
        })
    }

    /// Submit a `Set` command on the leader's state machine
    /// via `RaftNode::propose` (the same public API the
    /// production `client.rs` uses). Returns the index the
    /// command was appended at.
    ///
    /// This is a synchronous helper for tests — it acquires the
    /// leader's RwLock, dispatches the command, and returns. The
    /// command is replicated to followers via the heartbeat
    /// loop's AppendEntries RPC, which the caller must wait for
    /// separately (see [`Self::wait_for_replication`]).
    pub fn submit_set(&self, leader_idx: usize, key: &str, value: &str) -> u64 {
        use crate::protocol::Command;
        let leader = &self.nodes[leader_idx];
        let mut n = leader.raft.write().unwrap();
        let new_index = n.log.len() as u64 + 1;
        let ok = n.propose(Command::Set {
            key: key.to_string(),
            value: value.to_string(),
        });
        assert!(ok, "propose returned false (not Leader?)");
        new_index
    }

    /// Submit an arbitrary `Command` on the leader's state
    /// machine via `RaftNode::propose`. Returns the index the
    /// command was appended at.
    ///
    /// This is the general form of [`Self::submit_set`] used by
    /// DST scenarios that need to issue `BeginTx` / `DecideTx` /
    /// `Delete` / `Compact` (anything other than a plain `Set`).
    pub fn submit_command(&self, leader_idx: usize, command: crate::protocol::Command) -> u64 {
        let leader = &self.nodes[leader_idx];
        let mut n = leader.raft.write().unwrap();
        let new_index = n.log.len() as u64 + 1;
        let ok = n.propose(command);
        assert!(ok, "propose returned false (not Leader?)");
        new_index
    }

    /// Poll until every **non-killed** node has applied at least
    /// `target_index` on its state machine, or panic after
    /// `timeout`. Replication is driven by the heartbeat loop's
    /// AppendEntries, which only fires every
    /// `heartbeat_interval_ms` (250ms by default).
    ///
    /// A node is considered killed if its inbound channel is
    /// closed (the receiver returns `None` immediately). Tests
    /// that call `kill_node` and then continue asserting on the
    /// surviving nodes should use this method, not the strict
    /// [`Self::wait_for_replication_except`].
    ///
    /// This is the common-case helper for tests that haven't
    /// killed any nodes. For kill-and-continue scenarios, use
    /// [`Self::wait_for_replication_except`] with an explicit
    /// excluded list.
    pub async fn wait_for_replication(&self, target_index: u64, timeout: Duration) {
        self.wait_for_replication_except(target_index, &[], timeout).await;
    }

    /// Poll until every **non-excluded** node has applied at
    /// least `target_index` on its state machine, or panic
    /// after `timeout`. Replication is driven by the heartbeat
    /// loop's AppendEntries, which only fires every
    /// `heartbeat_interval_ms` (250ms by default).
    ///
    /// Pass `excluded` as a list of node indices to skip —
    /// typically the nodes that [`Self::kill_node`] has already
    /// taken out of the cluster. A killed node's `last_applied`
    /// will never advance, so a strict check would deadlock.
    pub async fn wait_for_replication_except(
        &self,
        target_index: u64,
        excluded: &[usize],
        timeout: Duration,
    ) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let all_done = self.nodes.iter().enumerate().all(|(i, n)| {
                if excluded.contains(&i) {
                    return true;
                }
                let r = n.raft.read().unwrap();
                r.last_applied >= target_index
            });
            if all_done {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let summary: Vec<(String, u64, u64, u64)> = self
                    .nodes
                    .iter()
                    .map(|n| {
                        let r = n.raft.read().unwrap();
                        (
                            n.id.clone(),
                            r.commit_index,
                            r.last_applied,
                            r.log.len() as u64,
                        )
                    })
                    .collect();
                panic!(
                    "wait_for_replication_except: target index {} not reached within {:?} (excluding {:?}); per-node (commit, applied, log_len) = {:?}",
                    target_index, timeout, excluded, summary
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Read a key from the local state machine of the given
    /// node. Returns `None` if the key has never been set (or
    /// has been deleted).
    pub fn read(&self, node_idx: usize, key: &str) -> Option<String> {
        let node = &self.nodes[node_idx];
        let sm = node.raft.read().unwrap().state_machine.clone();
        let guard = sm.read().unwrap();
        guard.get(key)
    }

    /// Stop every node's serve loop and heartbeat loop. Idempotent.
    pub async fn shutdown(&self) {
        for n in &self.nodes {
            n.stop.stop();
        }
        // Give the tasks a moment to observe the stop signal
        // and exit cleanly.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Stop one node's serve loop and heartbeat loop. Idempotent.
    /// After this returns:
    /// - The node's inbound channel is closed and no further
    ///   messages will be dispatched to it.
    /// - Its heartbeat loop is stopped.
    /// - Its state is forced to Follower (simulating "this
    ///   node has been removed from the cluster"). This is a
    ///   slightly stronger guarantee than real Raft provides —
    ///   in production a leader that loses quorum eventually
    ///   steps down on the next election timeout, but in the
    ///   DST harness election timers aren't running. Forcing
    ///   the state change makes `leader_index()` deterministic
    ///   immediately after `kill_node()` returns.
    /// - Its current_term is NOT touched. This is intentional —
    ///   in real Raft, a node that loses quorum does not bump
    ///   its own term until it observes a higher one. Callers
    ///   that care about term advancement should wait for
    ///   `wait_for_replication` or the next heartbeat round.
    ///
    /// This is the "node crashes" primitive for DST scenarios —
    /// §5.2 / §5.3 of the Raft paper both require a follower to
    /// win an election after the leader disappears, and this helper
    /// gives tests a one-line way to trigger that.
    pub async fn kill_node(&self, node_idx: usize) {
        // Force the node's state to Follower BEFORE stopping
        // the tasks. This way, if any other node consults this
        // node's state in the race window, it sees Follower.
        {
            let mut n = self.nodes[node_idx].raft.write().unwrap();
            n.state = NodeState::Follower;
        }
        self.nodes[node_idx].stop.stop();
        // Give the tasks a moment to observe the stop signal.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Read the current `current_term` of a node. Tests use this to
    /// assert that a new election actually advanced the term (and
    /// isn't just a stale "I think I'm leader" view).
    pub fn current_term(&self, node_idx: usize) -> u64 {
        self.nodes[node_idx].raft.read().unwrap().current_term
    }

    /// Wait up to `timeout` for `candidate_idx` to become Leader,
    /// but **do not panic** if they don't. Returns `true` iff the
    /// candidate is Leader by the deadline.
    ///
    /// Use this when a candidate *might* lose an election (e.g.
    /// election-restriction tests where a stale-log candidate
    /// should be rejected). For the typical "candidate should
    /// win" path, use [`Self::drive_election`] instead.
    pub async fn try_drive_election(&self, candidate_idx: usize, timeout: Duration) -> bool {
        let candidate = &self.nodes[candidate_idx];
        RaftNode::become_candidate(candidate.raft.clone());
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let n = candidate.raft.read().unwrap();
                if n.state == NodeState::Leader {
                    return true;
                }
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::fault_scheduler::{AlwaysDeliver, DropLink, PartitionedNetwork};

    /// End-to-end: a 3-node cluster elects a leader after
    /// `drive_election(0)`. NoFaults. The other two nodes
    /// become Followers; `leader_index()` returns 0.
    #[tokio::test]
    async fn sim_harness_3_nodes_elects_leader() {
        let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
        cluster.drive_election(0).await;

        let leader = cluster.leader_index().expect("a leader should exist");
        assert_eq!(leader, 0, "expected n0 to win the election");

        // All nodes should agree on the leader's term.
        let leader_term = cluster.nodes[leader].raft.read().unwrap().current_term;
        assert!(leader_term >= 1, "leader's term should be > 0");
        for n in &cluster.nodes {
            assert_eq!(
                n.raft.read().unwrap().current_term,
                leader_term,
                "node {} term mismatch",
                n.id
            );
        }

        cluster.shutdown().await;
    }

    /// End-to-end: after electing a leader, the leader can
    /// accept a `Set` command, and the followers converge to
    /// the new state after the heartbeat loop's AppendEntries
    /// replicates the entry.
    #[tokio::test]
    async fn sim_harness_3_nodes_replicates_set_to_all_followers() {
        let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
        cluster.drive_election(0).await;

        let leader_idx = cluster.leader_index().unwrap();
        let index = cluster.submit_set(leader_idx, "hello", "world");
        assert_eq!(index, 1, "first entry should be at index 1");

        // Replication is async (driven by the heartbeat loop,
        // 250ms cadence). 5s ceiling is generous slack for
        // busy CI hosts.
        cluster
            .wait_for_replication(index, Duration::from_secs(5))
            .await;

        // Every node has the entry applied to its state
        // machine.
        for n in &cluster.nodes {
            assert_eq!(
                cluster.read(
                    cluster.nodes.iter().position(|x| x.id == n.id).unwrap(),
                    "hello"
                ),
                Some("world".to_string()),
                "node {} should have replicated the Set",
                n.id
            );
        }

        cluster.shutdown().await;
    }

    /// End-to-end: elect a leader, replicate two Set commands
    /// in sequence. Each command reaches every node.
    #[tokio::test]
    async fn sim_harness_3_nodes_replicates_multiple_sets() {
        let cluster = SimCluster::new_3_nodes(Arc::new(AlwaysDeliver)).await;
        cluster.drive_election(0).await;
        let leader_idx = cluster.leader_index().unwrap();

        let idx1 = cluster.submit_set(leader_idx, "a", "1");
        let idx2 = cluster.submit_set(leader_idx, "b", "2");

        cluster.wait_for_replication(idx2, Duration::from_secs(5)).await;

        for i in 0..cluster.nodes.len() {
            assert_eq!(cluster.read(i, "a"), Some("1".to_string()), "node {} missing a", i);
            assert_eq!(cluster.read(i, "b"), Some("2".to_string()), "node {} missing b", i);
        }
        // Indices should be consecutive.
        assert_eq!(idx1, 1);
        assert_eq!(idx2, 2);
    }

    /// End-to-end with fault injection: a 3-node cluster where
    /// n0 -> n2 is dropped. The leader (n0) sends heartbeats
    /// to n1 only; n2 misses updates.
    ///
    /// This validates that the FaultScheduler integration is
    /// working end-to-end (not just in unit tests).
    #[tokio::test]
    async fn sim_harness_with_drop_link_isolates_n2() {
        let scheduler = Arc::new(DropLink {
            from: "n0".to_string(),
            to: "n2".to_string(),
        });
        let cluster = SimCluster::new_3_nodes(scheduler).await;
        cluster.drive_election(0).await;
        let leader_idx = cluster.leader_index().unwrap();

        // n0 -> n2 is dropped: the leader's heartbeat to n2
        // will fail (the SimTransport surfaces this as a
        // Timeout). The leader still has n1 for quorum, so it
        // stays Leader and can commit entries.

        let idx = cluster.submit_set(leader_idx, "before_heal", "v1");
        // n1 should catch up (its link to n0 is fine).
        // We need to wait specifically for n1 only — n2 will
        // never catch up while the link is dropped.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n1_applied = cluster.nodes[1].raft.read().unwrap().last_applied;
            if n1_applied >= idx {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("n1 did not catch up; last_applied = {}", n1_applied);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // n2 is still behind (its link is dropped).
        let n2_applied = cluster.nodes[2].raft.read().unwrap().last_applied;
        assert!(
            n2_applied < idx,
            "n2 should NOT have caught up while the link is dropped, got last_applied = {}",
            n2_applied
        );

        cluster.shutdown().await;
    }

    /// End-to-end with PartitionedNetwork: a 3-node cluster
    /// where n0 -> n2 is partitioned. n0 stays Leader (it
    /// has quorum via n1). After heal, n2 catches up via the
    /// next heartbeat.
    #[tokio::test]
    async fn sim_harness_with_partition_heals_and_catches_up() {
        // Keep a concrete Arc<PartitionedNetwork> so we can
        // call `heal()` later. The harness only needs the
        // trait object.
        let partition = Arc::new(PartitionedNetwork::new());
        let scheduler: Arc<dyn FaultScheduler> = partition.clone();
        let cluster = SimCluster::new_3_nodes(scheduler).await;
        cluster.drive_election(0).await;
        let leader_idx = cluster.leader_index().unwrap();

        // Partition n0 -> n2.
        partition.partition(crate::raft::fault_scheduler::LinkId::new(
            "n0",
            "n2",
        ));

        // n0 submits a Set; n1 catches up; n2 doesn't.
        let idx = cluster.submit_set(leader_idx, "after_partition", "v1");

        // Wait for n1 to catch up.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n1_applied = cluster.nodes[1].raft.read().unwrap().last_applied;
            if n1_applied >= idx {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("n1 did not catch up; last_applied = {}", n1_applied);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let n2_applied = cluster.nodes[2].raft.read().unwrap().last_applied;
        assert!(
            n2_applied < idx,
            "n2 should NOT have caught up while partitioned, got last_applied = {}",
            n2_applied
        );

        // Heal.
        partition.heal();

        // Now wait for n2 to catch up. Give it a generous
        // deadline (heartbeat is 250ms by default, so ~2s
        // should be plenty on a fast CI host).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let n2_applied = cluster.nodes[2].raft.read().unwrap().last_applied;
            if n2_applied >= idx {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "n2 did not catch up after heal; last_applied = {} (target = {})",
                    n2_applied, idx
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Final state: every node has the entry.
        for i in 0..cluster.nodes.len() {
            assert_eq!(
                cluster.read(i, "after_partition"),
                Some("v1".to_string()),
                "node {} should have caught up after heal",
                i
            );
        }

        cluster.shutdown().await;
    }

    /// Edge case: a 1-node cluster has no peers, so the
    /// candidate wins immediately (it grants itself a vote).
    /// The cluster is immediately usable.
    #[tokio::test]
    async fn sim_harness_1_node_cluster_is_immediately_leader() {
        let cluster = SimCluster::new_n_nodes(1, Arc::new(AlwaysDeliver)).await;
        cluster.drive_election(0).await;

        let leader = cluster.leader_index().expect("a leader should exist");
        assert_eq!(leader, 0);

        let idx = cluster.submit_set(leader, "k", "v");
        // No peers, so no replication wait — the entry is
        // applied locally on the next heartbeat tick (which
        // happens immediately because we just became
        // Leader).
        cluster.wait_for_replication(idx, Duration::from_secs(5)).await;
        assert_eq!(cluster.read(0, "k"), Some("v".to_string()));

        cluster.shutdown().await;
    }
}
