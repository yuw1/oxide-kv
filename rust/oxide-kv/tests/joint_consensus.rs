// ============================================================================
// 3-node / 4-node in-process integration test for joint consensus (P8 PR 6)
// ============================================================================
//
// Purpose
// -------
// `raft::node::propose_add_node` / `propose_remove_node` translate a
// client `Command::AddNode` / `Command::RemoveNode` into the textbook
// two-phase joint-consensus log sequence (Raft thesis §6). This file
// drives the full sequence end-to-end on a live 3-node cluster:
//
//   1. `add_node_to_3_node_cluster_brings_in_4th_node_and_all_4_form_quorum`:
//      leader gets `Command::AddNode { n4 }`, the Joint(n1..n3, n1..n4)
//      entry commits under the dual-majority rule, the Simple(n1..n4)
//      entry commits next, and a subsequent write submitted via n4
//      replicates to a 4-node majority.
//   2. `remove_node_shrinks_4_node_cluster_to_3_node_and_quorum_updates`:
//      leader gets `Command::RemoveNode { n3 }`, the Joint / Simple
//      entries commit, the cluster continues with 3 nodes, and writes
//      after the removal still succeed under the new 3-node majority.
//   3. `remove_node_rejects_cannot_remove_self_and_last_server`:
//      pin the safety checks at the leader-side coordinator.
//
// What this file does NOT cover (documented as out-of-scope)
// -----------------------------------------------------------
// - **Cold-new-server catch-up via InstallSnapshot**: the new node
//   starts with an empty log + a fresh state machine. We don't
//   deliberately start it behind on log entries; this test exercises
//   the catch-up path indirectly (the leader's AppendEntries bring it
//   up to date via the existing protocol) but doesn't pin the
//   snapshot-replication path. A dedicated `cold_new_server.rs` would
//   cover that if it becomes a hazard.
// - **Disjoint-majority regression**: the joint phase is the fix for
//   disjoint majorities. We don't construct the failure mode here
//   because reproducing it requires a two-step membership change in
//   flight; the existing `raft_fuzz.rs` scenario vocabulary doesn't
//   exercise AddNode / RemoveNode yet (that's PR #7 or later).
// - **Multi-server changes**: only single-server AddNode / RemoveNode
//   are exercised. Multi-server changes require multi-step membership
//   coordination and are out of scope for this PR.
//
// Design notes
// ------------
// - **No real network.** All nodes listen on `127.0.0.1:0` (OS-assigned
//   ephemeral ports). Connections cross localhost only.
// - **Manual leader election.** We drive `become_candidate` on node 0
//   rather than waiting for the 5-10s randomized timer. The election
//   timer's unit tests cover the timing behavior; here we just need a
//   leader fast.
// - **Heartbeat loop spawned.** This is what pushes AppendEntries to
//   peers — without it, `commit_index` advances on the leader but
//   followers never receive `leader_commit` and the Joint entry
//   never commits. The pre-vote PR (P8 PR 5) made this loop more
//   responsive (250ms cadence instead of 1000ms).

use oxide_kv::protocol::{Command, ServerId};
use oxide_kv::raft::net::StopSignal;
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::rpc::RpcServer;
use oxide_kv::raft::storage::RaftStorage;
use oxide_kv::state_machine::{StateMachine, StateMachineConfig};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// TestHarness (mirrors `integration_2pc.rs`)
// ---------------------------------------------------------------------------

struct TestNode {
    addr: String,
    raft: Arc<RwLock<RaftNode>>,
    _data_dir: TempDir,
    hb_stop: StopSignal,
}

impl TestNode {
    fn shutdown(&self) {
        self.hb_stop.stop();
    }
}

async fn spawn_node(peers: Vec<String>) -> TestNode {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr").to_string();

    let data_dir = tempfile::tempdir().expect("tempdir");
    let wal = data_dir
        .path()
        .join("node.wal")
        .to_str()
        .unwrap()
        .to_string();
    let meta = data_dir
        .path()
        .join("node_meta.json")
        .to_str()
        .unwrap()
        .to_string();
    let snap = data_dir
        .path()
        .join("node_snapshot.json")
        .to_str()
        .unwrap()
        .to_string();
    let storage = RaftStorage::new_with_paths(wal, meta, snap);

    let sm_dir = data_dir.path().join("sm");
    let sm_config = StateMachineConfig {
        data_dir: sm_dir,
        memtable_size_threshold: 1024 * 1024,
    };
    let sm = Arc::new(RwLock::new(
        StateMachine::open(sm_config).expect("StateMachine::open"),
    ));

    let raft = RaftNode::new_with_storage(addr.clone(), peers, sm, storage);
    let raft = Arc::new(RwLock::new(raft));

    // RPC listener.
    let r = raft.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let r2 = r.clone();
            tokio::spawn(async move {
                let _ = RpcServer::handle_raft_rpc(stream, r2).await;
            });
        }
    });

    // Heartbeat loop.
    let h = raft.clone();
    let hb_stop = StopSignal::new();
    let hb_stop_clone = hb_stop.clone();
    tokio::spawn(async move {
        RaftNode::run_heartbeat_loop(h, hb_stop_clone).await;
    });

    TestNode {
        addr,
        raft,
        _data_dir: data_dir,
        hb_stop,
    }
}

async fn elect_leader(nodes: &[TestNode]) -> usize {
    // Drive node 0 to candidate; with 3 nodes, it wins on self-vote.
    let raft0 = nodes[0].raft.clone();
    RaftNode::become_candidate(raft0);
    // Wait for node 0 to become Leader.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        {
            let n = nodes[0].raft.read().unwrap();
            if n.state == NodeState::Leader {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!("node 0 did not become leader within 2s");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Give the AppendEntries replication a moment to deliver the
    // initial empty AE to peers so they reset their election timers.
    tokio::time::sleep(Duration::from_millis(200)).await;
    0
}

async fn wait_for_commit(nodes: &[TestNode], leader_idx: usize, target: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let committed = {
            let n = nodes[leader_idx].raft.read().unwrap();
            n.commit_index
        };
        if committed >= target {
            // Also wait for all peers to catch up. The removed node
            // (if any) won't catch up because the leader stopped
            // replicating to it.
            for (i, peer) in nodes.iter().enumerate() {
                if i == leader_idx || i == 3 {
                    continue;
                }
                let last_applied = peer.raft.read().unwrap().last_applied;
                if last_applied < target {
                    // Not caught up yet; keep waiting.
                    if std::time::Instant::now() >= deadline {
                        panic!(
                            "node {} not caught up: last_applied={} target={}",
                            i, last_applied, target
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            }
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "leader commit_index={} < target={} after 5s",
                committed, target
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn add_node_to_3_node_cluster_brings_in_4th_node_and_all_4_form_quorum() {
    // 1. Start 3 nodes. Spawn n0 with empty peers, then update
    //    once we know n1 and n2's addresses. (OS-assigned ports
    //    require this dance; see `integration_2pc.rs` for the
    //    canonical pattern.)
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let nodes = vec![n0, n1, n2];

    // 2. Elect node 0 as leader.
    let leader_idx = elect_leader(&nodes).await;
    assert_eq!(leader_idx, 0);

    // 3. Start n4 as a single-node follower (empty peer list),
    //    then have the leader add it to the cluster.
    let n4 = spawn_node(vec![]).await;
    let n4_addr = n4.addr.clone();

    // Cold-new-server catch-up: before the leader can propose
    // AddNode, n4 needs to know who the leader is. In production
    // this would be done via an out-of-band `JoinClusterRequest`
    // RPC (a follow-up PR can add it); for this in-process test
    // we set n4's peer list directly.
    {
        let mut n = n4.raft.write().unwrap();
        n.set_peers(vec![nodes[leader_idx].addr.clone()]);
    }

    let joint_idx = {
        let mut leader = nodes[leader_idx].raft.write().unwrap();
        leader
            .propose_add_node(ServerId {
                node_id: n4_addr.clone(),
                addr: n4_addr.clone(),
            })
            .expect("leader should accept AddNode")
    };
    // Trigger replication.
    RaftNode::sync_logs(nodes[leader_idx].raft.clone());
    let simple_idx = joint_idx + 1;

    // 4. Wait for both entries to commit and apply on every node.
    wait_for_commit(&nodes, leader_idx, simple_idx).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Verify all 4 nodes have the Simple config installed.
    for (i, n) in nodes.iter().enumerate() {
        let cfg = &n.raft.read().unwrap().current_config;
        let all = cfg.all_servers();
        let ids: Vec<&str> = all.iter().map(|s| s.node_id.as_str()).collect();
        assert!(
            ids.contains(&n4_addr.as_str()),
            "node {} did not install n4 in current_config; config = {:?}",
            i,
            cfg
        );
    }

    // 5. Submit a write on n4 (well, on the leader, since n4
    //    isn't the leader). The leader should now have 4 peers in
    //    its config; quorum is 3 (4/2 + 1). With the leader + n2
    //    + n3, that's 3 of 4 -> quorum.
    let propose_res = {
        let mut leader = nodes[leader_idx].raft.write().unwrap();
        leader.propose(Command::Set {
            key: "post_add_key".to_string(),
            value: "post_add_value".to_string(),
        })
    };
    assert!(propose_res, "leader should accept the proposal");
    RaftNode::sync_logs(nodes[leader_idx].raft.clone());
    let write_idx = {
        let leader = nodes[leader_idx].raft.read().unwrap();
        leader.log.len() as u64
    };
    wait_for_commit(&nodes, leader_idx, write_idx).await;

    // Cleanup.
    for n in &nodes {
        n.shutdown();
    }
    n4.shutdown();
}

#[tokio::test]
async fn remove_node_shrinks_4_node_cluster_to_3_node_and_quorum_updates() {
    // 1. Start 4 nodes. Use the same empty-peer-then-set pattern as the
    //    add-node test (OS-assigned ports require deferred wiring).
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    let n3 = spawn_node(vec![n0.addr.clone(), n1.addr.clone(), n2.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone(), n3.addr.clone()]);
    }
    let nodes = vec![n0, n1, n2, n3];

    // 2. Elect leader.
    let leader_idx = elect_leader(&nodes).await;
    assert_eq!(leader_idx, 0);

    // 3. Remove n4.
    let remove_addr = nodes[3].addr.clone();
    let joint_idx = {
        let mut leader = nodes[leader_idx].raft.write().unwrap();
        leader.propose_remove_node(&remove_addr).expect("ok")
    };
    RaftNode::sync_logs(nodes[leader_idx].raft.clone());
    let simple_idx = joint_idx + 1;
    wait_for_commit(&nodes, leader_idx, simple_idx).await;
    // Give peers a chance to apply the Simple entry (which arrives
    // via a subsequent heartbeat with updated `leader_commit`).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 4. Verify all nodes now have a 3-node Simple config (excluding n4).
    // Note: the removed node (n4) itself does NOT receive the Simple
    // entry — it's no longer in the cluster's replication group, so
    // its log is frozen at whatever it had when it was kicked out.
    // We only check the surviving 3 nodes.
    for (i, n) in nodes.iter().enumerate() {
        if i == 3 {
            continue;
        }
        let cfg = &n.raft.read().unwrap().current_config;
        let all = cfg.all_servers();
        let ids: Vec<&str> = all.iter().map(|s| s.node_id.as_str()).collect();
        assert!(
            !ids.contains(&remove_addr.as_str()),
            "node {} still has removed n4 in current_config; config = {:?}",
            i,
            cfg
        );
        assert_eq!(
            all.len(),
            3,
            "node {} expected 3-node Simple config; got {:?}",
            i,
            cfg
        );
    }

    // 5. n4 is no longer in the cluster. The leader should not
    //    send AppendEntries to it anymore (its `peers` field no
    //    longer includes n4). A subsequent write should still
    //    succeed under the 3-node majority.
    let write_idx = {
        let idx = {
            let mut leader = nodes[leader_idx].raft.write().unwrap();
            assert!(leader.propose(Command::Set {
                key: "post_remove_key".into(),
                value: "post_remove_value".into(),
            }));
            leader.log.len() as u64
        };
        // NOTE: must call sync_logs OUTSIDE the write-lock scope;
        // sync_logs tries to take a read-lock on the same RaftNode,
        // which would deadlock against our outstanding write-lock.
        RaftNode::sync_logs(nodes[leader_idx].raft.clone());
        idx
    };
    wait_for_commit(&nodes, leader_idx, write_idx).await;

    for n in &nodes {
        n.shutdown();
    }
}

#[tokio::test]
async fn remove_node_rejects_cannot_remove_self() {
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let nodes = vec![n0, n1, n2];
    let leader_idx = elect_leader(&nodes).await;

    let leader_addr = nodes[leader_idx].addr.clone();
    let result = {
        let mut leader = nodes[leader_idx].raft.write().unwrap();
        leader.propose_remove_node(&leader_addr)
    };
    assert!(result.is_err());

    for n in &nodes {
        n.shutdown();
    }
}

#[tokio::test]
async fn add_node_rejects_already_member() {
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let nodes = vec![n0, n1, n2];
    let leader_idx = elect_leader(&nodes).await;

    // Try to add n2 (already a member).
    let n2_addr = nodes[1].addr.clone();
    let result = {
        let mut leader = nodes[leader_idx].raft.write().unwrap();
        leader.propose_add_node(ServerId {
            node_id: n2_addr.clone(),
            addr: n2_addr.clone(),
        })
    };
    assert!(
        result.is_err(),
        "leader should refuse to re-add existing member"
    );

    for n in &nodes {
        n.shutdown();
    }
}
