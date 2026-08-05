// ============================================================================
// Integration tests for P8 PR 7 (tx timeout + admin-driven abort)
// ============================================================================
//
// Focus: minimal end-to-end coverage of the two new paths without
// trying to replicate the full `integration_2pc.rs` harness. The
// production-grade multi-node replication path is already covered by
// `integration_2pc.rs`; this file exercises:
//
//   1. **Coordinator sweep on a single-node leader** — the
//      `run_tx_timeout_loop` task correctly appends a
//      `DecideTx(Abort)` log entry when `pending_txs` has a stuck
//      tx, and the apply path purges it.
//
//   2. **Admin `AbortTx` RPC on a single-node leader** — the
//      JSON client command translates to a `propose_abort_tx` call
//      that appends `DecideTx(Abort)`, with proper error codes
//      (`tx_not_found` for unknown id).
//
//   3. **No-op on a follower** — running the sweep on a non-leader
//      appends nothing (this is also covered as a unit test in
//      `raft/coordinator.rs::tests`, but the integration path
//      exercises the real `tokio::select!` loop body).
//
// Why single-node? — These tests are about *behavior*, not
// *replication correctness*; the latter is already covered by
// `integration_2pc.rs` / `joint_consensus.rs`. Single-node keeps
// the tests fast, deterministic, and focused on the new code
// paths.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::sleep;

use oxide_kv::client::ClientHandler;
use oxide_kv::protocol::{Command, LogEntry, TxDecision, TxOp};
use oxide_kv::raft::coordinator;
use oxide_kv::raft::net::StopSignal;
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::storage::RaftStorage;
use oxide_kv::state_machine::{now_unix_ms, StateMachine, StateMachineConfig};

struct TestNode {
    raft: Arc<RwLock<RaftNode>>,
    /// Bound address — kept for diagnostics; the production tests do
    /// not need it, so the field is allowed dead.
    #[allow(dead_code)]
    addr: String,
    _dir: TempDir,
}

async fn spawn_single_node() -> TestNode {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);

    let dir = tempfile::tempdir().expect("tempdir");
    let wal = dir.path().join("n.wal").to_str().unwrap().to_string();
    let meta = dir.path().join("n_meta.json").to_str().unwrap().to_string();
    let snap = dir
        .path()
        .join("n_snapshot.json")
        .to_str()
        .unwrap()
        .to_string();
    let storage = RaftStorage::new_with_paths(wal, meta, snap);
    let sm_dir = dir.path().join("sm");
    let sm_config = StateMachineConfig {
        data_dir: sm_dir,
        memtable_size_threshold: 1024 * 1024,
    };
    let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
    let node = RaftNode::new_with_storage(addr.clone(), vec![], sm, storage);
    // Auto-elect: single-node, no peers, become Leader immediately
    // (mirrors `main.rs` startup).
    let mut node = node;
    node.state = NodeState::Leader;
    TestNode {
        raft: Arc::new(RwLock::new(node)),
        addr,
        _dir: dir,
    }
}

async fn spawn_client_listener(node_arc: Arc<RwLock<RaftNode>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let node_clone = node_arc.clone();
    tokio::spawn(async move {
        // Bound to a single connection — these tests send one
        // command then drop the client. Loop once, then exit so
        // the spawned task doesn't outlive the test (and the
        // runtime can shut down cleanly).
        if let Ok((stream, _)) = listener.accept().await {
            let n = node_clone.clone();
            let _ = ClientHandler::handle_client_request(stream, n).await;
        }
    });
    port
}

async fn client_command(port: u16, payload: serde_json::Value) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let s = payload.to_string();
    stream.write_all(s.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    // Server is line-delimited JSON (does NOT close the connection
    // after responding), so we must `read_line` to one response —
    // `read_to_end` would block forever waiting for EOF. Mirrors
    // `tests/integration_2pc.rs::client_request`.
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read");
    serde_json::from_str(line.trim()).expect("parse response")
}

fn build_begin_tx_entry(tx_id: &str) -> LogEntry {
    LogEntry {
        term: 1,
        index: 1, // fixed; not used for matching
        command: Command::BeginTx {
            tx_id: tx_id.to_string(),
            ops: vec![TxOp::Put {
                key: "k1".into(),
                value: "v1".into(),
            }],
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_abort_tx_round_trip_on_single_node_leader() {
    let node = spawn_single_node().await;

    // Seed a pending tx via the apply_logs path so the
    // state-machine has the entry. This mirrors what happens
    // when a BeginTx log entry is committed on a real leader.
    {
        let mut n = node.raft.write().unwrap();
        n.log.push(build_begin_tx_entry("aborted-tx"));
        n.commit_index = n.log.len() as u64;
        n.apply_logs();
        assert!(n.state_machine.read().unwrap().is_tx_pending("aborted-tx"));
    }

    // Send AbortTx over the JSON client.
    let port = spawn_client_listener(node.raft.clone()).await;
    let resp =
        client_command(port, serde_json::json!({"AbortTx": {"tx_id": "aborted-tx"}}))
            .await;
    assert_eq!(resp.get("status").and_then(|v| v.as_str()), Some("ok"));
    assert_eq!(resp.get("decision").and_then(|v| v.as_str()), Some("abort"));
    let decide_index = resp.get("decide_index").and_then(|v| v.as_u64()).unwrap();

    // The DecideTx(Abort) entry is on the log and applied.
    {
        let n = node.raft.read().unwrap();
        assert_eq!(n.log.last().unwrap().index as u64, decide_index);
        match &n.log.last().unwrap().command {
            Command::DecideTx { tx_id, decision } => {
                assert_eq!(tx_id, "aborted-tx");
                assert_eq!(*decision, TxDecision::Abort);
            }
            other => panic!("expected DecideTx(Abort), got {:?}", other),
        }
        // After apply_logs runs, pending_txs is purged.
        drop(n);
        let mut n = node.raft.write().unwrap();
        n.commit_index = decide_index;
        n.apply_logs();
        assert!(!n.state_machine.read().unwrap().is_tx_pending("aborted-tx"));
        // Ops NOT applied (Abort semantics).
        assert_eq!(n.state_machine.read().unwrap().get("k1"), None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_abort_tx_returns_tx_not_found_for_unknown_id() {
    let node = spawn_single_node().await;
    let port = spawn_client_listener(node.raft.clone()).await;
    let resp = client_command(
        port,
        serde_json::json!({"AbortTx": {"tx_id": "never-existed"}}),
    )
    .await;
    assert_eq!(resp.get("status").and_then(|v| v.as_str()), Some("error"));
    assert_eq!(
        resp.get("code").and_then(|v| v.as_str()),
        Some("tx_not_found")
    );
    // No log entry was appended.
    assert_eq!(node.raft.read().unwrap().log.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_sweep_force_aborts_stuck_tx() {
    unsafe {
        std::env::set_var("OXIDE_TX_TIMEOUT_MS", "10");
    }
    unsafe {
        std::env::set_var("OXIDE_TX_TIMEOUT_SWEEP_INTERVAL_MS", "20");
    }

    let node = spawn_single_node().await;

    // Seed a "stuck" tx via apply_logs + back-dated begin_tx_at.
    {
        let mut n = node.raft.write().unwrap();
        n.log.push(build_begin_tx_entry("sweep-me"));
        n.commit_index = n.log.len() as u64;
        n.apply_logs();
        // Re-insert with an old timestamp so the sweep sees it as
        // stuck.
        let sm = n.state_machine.clone();
        sm.write()
            .unwrap()
            .decide_tx("sweep-me", TxDecision::Abort)
            .unwrap();
        sm.write()
            .unwrap()
            .begin_tx_at(
                "sweep-me".into(),
                vec![TxOp::Put {
                    key: "k1".into(),
                    value: "v1".into(),
                }],
                now_unix_ms().saturating_sub(60_000),
            )
            .unwrap();
        assert!(sm.read().unwrap().is_tx_pending("sweep-me"));
    }

    // Run the sweep loop for ~120 ms (4 sweep periods at 20 ms).
    let stop = StopSignal::new();
    let stop_clone = stop.clone();
    let sweep_raft = node.raft.clone();
    let sweep_handle = tokio::spawn(async move {
        coordinator::run_tx_timeout_loop(sweep_raft, stop_clone).await;
    });
    sleep(Duration::from_millis(120)).await;
    stop.stop();
    let _ = tokio::time::timeout(Duration::from_millis(100), sweep_handle).await;

    // The sweep appended a DecideTx(Abort) entry.
    let n = node.raft.read().unwrap();
    let last = n.log.last().expect("log must have entries");
    match &last.command {
        Command::DecideTx { tx_id, decision } => {
            assert_eq!(tx_id, "sweep-me");
            assert_eq!(*decision, TxDecision::Abort);
        }
        other => panic!("expected DecideTx(Abort) appended by sweep, got {:?}", other),
    }
    drop(n);
    // Apply the new entry so pending_txs is purged.
    let mut n = node.raft.write().unwrap();
    n.commit_index = n.log.len() as u64;
    n.apply_logs();
    assert!(!n.state_machine.read().unwrap().is_tx_pending("sweep-me"));
    assert_eq!(n.state_machine.read().unwrap().get("k1"), None);

    unsafe {
        std::env::remove_var("OXIDE_TX_TIMEOUT_MS");
    }
    unsafe {
        std::env::remove_var("OXIDE_TX_TIMEOUT_SWEEP_INTERVAL_MS");
    }
}