// ============================================================================
// Cold-new-server catch-up integration test (P8 PR 6a)
// ============================================================================
//
// Purpose
// -------
// PR #38 (joint consensus) assumes the new server already knows the
// leader's address (`set_peers(vec![leader_addr])` in the test harness).
// That works in-process; in production a brand-new server starts with
// empty `peers` and no one knows its address, so it cannot be reached
// via the normal AppendEntries path. PR #6a closes that hole with a
// new `JoinCluster` RPC: the candidate sends `JoinClusterRequest` to
// a *hint* address it learned out-of-band; the leader replies with
// the current peer list; the candidate then calls `set_peers(...)`
// and waits to be added via the Joint consensus path.
//
// This integration test exercises the full chain end-to-end:
//   1. Start a 3-node cluster; elect n0 as leader.
//   2. Start a brand-new server (n3') with empty peers.
//   3. n3' sends JoinCluster to n0's address (the hint).
//   4. n3' receives the peer list, calls `set_peers(...)`.
//   5. Leader runs `propose_add_node(n3')` -> Joint commit -> Simple
//      commit on n1, n2 (n3' applies both after heartbeat catch-up).
//   6. Submit a write; verify all 4 nodes see it.
//
// Negative tests cover the rejection paths:
//   - non-leader route (hint lands on a follower)
//   - self-address (candidate_addr == leader_addr)
//   - already-member (idempotent retry safety net)
//
// Tests run in-process via `tokio::spawn` + `tokio::net::TcpListener`,
// matching the pattern in `joint_consensus.rs`. Real cross-process
// smoke testing lives in the P8 PR #9 systemd integration test.

use oxide_kv::protocol::{Command, ServerId};
use oxide_kv::raft::net::StopSignal;
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::rpc::{JoinClusterRequest, JoinClusterResponse, RaftMessage, RpcServer};
use oxide_kv::raft::storage::RaftStorage;
use oxide_kv::state_machine::{StateMachine, StateMachineConfig};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;

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
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let r2 = r.clone();
                    tokio::spawn(async move {
                        let _ = RpcServer::handle_raft_rpc(stream, r2).await;
                    });
                }
                Err(_) => break,
            }
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

/// Drive node 0 to leader on a 3-node cluster.
async fn elect_leader_3(nodes: &[TestNode]) -> usize {
    let raft0 = nodes[0].raft.clone();
    RaftNode::become_candidate(raft0);
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
    // Let AppendEntries reset peer election timers.
    tokio::time::sleep(Duration::from_millis(200)).await;
    0
}

/// Wait until leader's `commit_index` reaches `target`.
async fn wait_for_commit(nodes: &[TestNode], leader_idx: usize, target: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let committed = {
            let n = nodes[leader_idx].raft.read().unwrap();
            n.commit_index
        };
        if committed >= target {
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

/// Send a JoinCluster RPC from `candidate_addr` to `target_addr` and
/// return the response. Uses the leader's own transport so we get the
/// real wire path (length-prefixed framing, protobuf, async round-trip).
async fn send_join_cluster(
    leader: &TestNode,
    target_addr: &str,
    candidate_addr: &str,
) -> JoinClusterResponse {
    let transport = leader.raft.read().unwrap().transport_handle();
    let req = RaftMessage::JoinCluster(JoinClusterRequest {
        candidate_addr: candidate_addr.to_string(),
    });
    let reply = transport
        .send_raft(target_addr, req)
        .await
        .expect("transport.send_raft");
    match reply {
        RaftMessage::JoinClusterResponse(resp) => resp,
        other => panic!("expected JoinClusterResponse, got {:?}", other),
    }
}

// ============================================================================
// Happy path
// ============================================================================

#[tokio::test]
async fn cold_new_server_joins_3_node_cluster_via_join_cluster_rpc() {
    // 1. Stand up a 3-node cluster.
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let cluster = vec![n0, n1, n2];
    let leader_idx = elect_leader_3(&cluster).await;
    let leader_addr = cluster[leader_idx].addr.clone();

    // 2. Cold-new-server: starts with empty peers, no leader_addr_hint
    //    configured, no log entries.
    let candidate = spawn_node(vec![]).await;
    let candidate_addr = candidate.addr.clone();

    // Sanity: candidate's log is empty and current_config is just itself.
    {
        let c = candidate.raft.read().unwrap();
        assert!(c.peers().is_empty(), "candidate must start with no peers");
        assert_eq!(c.log.len(), 0, "candidate must start with empty log");
    }

    // 3. Candidate sends JoinCluster to leader's hint.
    let resp = send_join_cluster(&cluster[leader_idx], &leader_addr, &candidate_addr).await;
    assert!(resp.accepted, "expected accept; reason={}", resp.reason);
    assert_eq!(resp.leader_addr, leader_addr);
    assert_eq!(resp.term, 1);
    let mut got = resp.peer_addrs.clone();
    got.sort();
    // Peer list = all_servers \ {self, candidate} = {leader,n1,n2} \ {leader, candidate}
    // = {n1, n2}. (We compare by addr; ordering doesn't matter.)
    let mut expected = vec![cluster[1].addr.clone(), cluster[2].addr.clone()];
    expected.sort();
    assert_eq!(got, expected);

    // 4. Candidate populates peers from the response.
    {
        let mut c = candidate.raft.write().unwrap();
        c.set_peers(resp.peer_addrs.clone());
    }

    // 5. Leader runs propose_add_node -> Joint + Simple commit.
    let joint_idx = {
        let mut leader = cluster[leader_idx].raft.write().unwrap();
        leader
            .propose_add_node(ServerId {
                node_id: candidate_addr.clone(),
                addr: candidate_addr.clone(),
            })
            .expect("leader should accept AddNode")
    };
    // NOTE: must call sync_logs OUTSIDE the write-lock scope; sync_logs
    // takes a read-lock on the same RaftNode which would deadlock
    // if we held a write-lock across it. The `let joint_idx = { ... }`
    // expression-block above releases the leader's write-lock at the
    // closing brace.
    RaftNode::sync_logs(cluster[leader_idx].raft.clone());
    let simple_idx = joint_idx + 1;
    wait_for_commit(&cluster, leader_idx, simple_idx).await;
    // Give candidates/peers a moment to apply Simple via heartbeat.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 6. Verify all 4 nodes have a 4-node Simple config.
    for (i, n) in cluster.iter().enumerate() {
        let cfg = &n.raft.read().unwrap().current_config;
        let all = cfg.all_servers();
        let addrs: Vec<&str> = all.iter().map(|s| s.addr.as_str()).collect();
        let mut addrs_sorted = addrs.clone();
        addrs_sorted.sort();
        let mut expected = vec![
            cluster[0].addr.as_str(),
            cluster[1].addr.as_str(),
            cluster[2].addr.as_str(),
            candidate_addr.as_str(),
        ];
        expected.sort();
        assert_eq!(
            addrs_sorted, expected,
            "cluster node {} expected 4-node Simple; got {:?}",
            i, addrs_sorted
        );
    }
    {
        let cfg = &candidate.raft.read().unwrap().current_config;
        let all = cfg.all_servers();
        let addrs: Vec<&str> = all.iter().map(|s| s.addr.as_str()).collect();
        let mut addrs_sorted = addrs.clone();
        addrs_sorted.sort();
        let mut expected = vec![
            cluster[0].addr.as_str(),
            cluster[1].addr.as_str(),
            cluster[2].addr.as_str(),
            candidate_addr.as_str(),
        ];
        expected.sort();
        assert_eq!(
            addrs_sorted, expected,
            "candidate expected 4-node Simple; got {:?}",
            addrs_sorted
        );
    }

    // Cleanup.
    for n in &cluster {
        n.shutdown();
    }
    candidate.shutdown();
}

// ============================================================================
// Negative: non-leader route
// ============================================================================

#[tokio::test]
async fn join_cluster_routed_to_follower_is_rejected() {
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let cluster = vec![n0, n1, n2];
    let leader_idx = elect_leader_3(&cluster).await;
    let leader_addr = cluster[leader_idx].addr.clone();

    // Pick a follower address.
    let follower_addr = cluster[(leader_idx + 1) % cluster.len()].addr.clone();
    assert_ne!(
        follower_addr, leader_addr,
        "follower must differ from leader"
    );

    let candidate = spawn_node(vec![]).await;
    let resp = send_join_cluster(&cluster[leader_idx], &follower_addr, &candidate.addr).await;
    assert!(!resp.accepted);
    assert_eq!(resp.reason, "not leader");
    assert!(resp.peer_addrs.is_empty());
    // term is still the current cluster term.
    assert_eq!(resp.term, 1);
    // leader_addr field carries the responder's node_id even on reject
    // (lets the candidate log which node rejected).
    assert_eq!(resp.leader_addr, follower_addr);

    for n in &cluster {
        n.shutdown();
    }
    candidate.shutdown();
}

// ============================================================================
// Negative: candidate_addr == leader_addr
// ============================================================================

#[tokio::test]
async fn join_cluster_rejects_self_address() {
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let cluster = vec![n0, n1, n2];
    let leader_idx = elect_leader_3(&cluster).await;
    let leader_addr = cluster[leader_idx].addr.clone();

    // Candidate_addr == leader_addr (e.g. hint landed back on leader).
    let resp = send_join_cluster(&cluster[leader_idx], &leader_addr, &leader_addr).await;
    assert!(!resp.accepted);
    assert_eq!(resp.reason, "candidate_addr is the leader itself");
    assert!(resp.peer_addrs.is_empty());

    for n in &cluster {
        n.shutdown();
    }
}

// ============================================================================
// Negative: already-member idempotent retry
// ============================================================================

#[tokio::test]
async fn join_cluster_rejects_candidate_addr_already_member() {
    let n0 = spawn_node(vec![]).await;
    let n1 = spawn_node(vec![n0.addr.clone()]).await;
    let n2 = spawn_node(vec![n0.addr.clone(), n1.addr.clone()]).await;
    {
        let mut n = n0.raft.write().unwrap();
        n.set_peers(vec![n1.addr.clone(), n2.addr.clone()]);
    }
    let cluster = vec![n0, n1, n2];
    let leader_idx = elect_leader_3(&cluster).await;

    // Try to JoinCluster with n1's address (already a member).
    let resp = send_join_cluster(
        &cluster[leader_idx],
        &cluster[leader_idx].addr,
        &cluster[1].addr,
    )
    .await;
    assert!(!resp.accepted);
    assert_eq!(resp.reason, "candidate_addr is already a cluster member");
    assert!(resp.peer_addrs.is_empty());

    for n in &cluster {
        n.shutdown();
    }
}
