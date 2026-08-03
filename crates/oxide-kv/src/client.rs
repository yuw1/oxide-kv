use crate::raft::node::{RaftNode, NodeState};
use crate::protocol::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct ClientHandler;

impl ClientHandler {
    /// Entry point for handling client TCP connections.
    /// Manages the network lifecycle and JSON serialization/deserialization.
    pub async fn handle_client_request(mut stream: TcpStream, node_arc: Arc<RwLock<RaftNode>>) {
        println!("Client connected: {:?}", stream.peer_addr());

        let (reader, mut writer) = stream.split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            match buf_reader.read_line(&mut line).await {
                Ok(0) => break, // Client disconnected
                Ok(_) => {
                    // 1. Parse JSON command
                    let command: Command = match serde_json::from_str(&line) {
                        Ok(cmd) => cmd,
                        Err(e) => {
                            eprintln!("❌ [Client] Failed to parse command: {}", e);
                            let error_resp = serde_json::json!({"status": "error", "message": format!("Invalid JSON: {}", e)});
                            let _ = writer.write_all(format!("{}\n", error_resp).as_bytes()).await;
                            continue;
                        }
                    };

                    // 2. Dispatch the command to business logic
                    let response_json = Self::dispatch_command(command, &node_arc).await;

                    // 3. Send response back to client
                    let resp_str = format!("{}\n", response_json.to_string());
                    if let Err(e) = writer.write_all(resp_str.as_bytes()).await {
                        eprintln!("Failed to send response: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Network read error: {}", e);
                    break;
                }
            }
        }
        println!("Client connection closed");
    }

    /// Routes the command based on its type and performs role validation.
    pub async fn dispatch_command(command: Command, node_arc: &Arc<RwLock<RaftNode>>) -> serde_json::Value {
        // Quick role check; Get does its own leader check via begin_read.
        {
            let node = node_arc.read().unwrap();
            if node.state != NodeState::Leader {
                return serde_json::json!({"error":"Not a leader. Please connect to the leader node."});
            }
        }

        match command {
            Command::Set { .. } | Command::Delete { .. } => {
                Self::apply_mutation(command, node_arc).await
            },
            Command::Get { key } => Self::linearizable_get(&key, node_arc).await,
            Command::BeginTx { tx_id, ops } => {
                Self::begin_tx(tx_id, ops, node_arc).await
            }
            Command::DecideTx { .. } => {
                // Manual 2PC control command for tests / admin: lets a test
                // force a Commit/Abort without driving the full coordinator
                // RPC. Treat as a mutation so it goes through Raft.
                //
                // As of P6, raw `Vote` entries no longer exist in the log
                // (votes flow on the side-channel RPC), so the previous
                // `Command::Vote` arm was removed alongside this one.
                Self::apply_mutation(command, node_arc).await
            }
            Command::Compact => {
                serde_json::json!({"status": "error", "message": "compact not supported yet"})
            },
        }
    }

    /// Linearizable Get via ReadIndex:
    ///   1. begin_read() captures the leader's commit_index and triggers a heartbeat.
    ///   2. Wait (poll) until confirm_read() reports safety.
    ///   3. Read the value from the state machine at that point.
    ///
    /// Single-node fast path: a leader with no peers has no quorum to prove
    /// against, so `begin_read` would block forever waiting for a heartbeat
    /// reply that never arrives. Skip ReadIndex entirely and read directly
    /// from the state machine after applying any pending committed entries.
    /// Safety still holds because:
    ///   - The node must be Leader (checked in dispatch_command).
    ///   - We call `apply_logs` first, so the read sees at least
    ///     everything up to the current `commit_index`.
    ///   - On a single-node cluster, the leader's `commit_index` advances
    ///     synchronously with each proposal, so the read is consistent with
    ///     all previously-acknowledged writes.
    pub async fn linearizable_get(key: &str, node_arc: &Arc<RwLock<RaftNode>>) -> serde_json::Value {
        // Single-node fast path: no peers → no quorum proof → skip ReadIndex.
        let is_single_node = {
            let node = node_arc.read().unwrap();
            node.is_single_node()
        };
        if is_single_node {
            // Apply any committed-but-not-yet-applied entries so the read
            // reflects the latest committed state.
            {
                let mut node = node_arc.write().unwrap();
                if node.state != NodeState::Leader {
                    return serde_json::json!({"error": "Not a leader. Please connect to the leader node."});
                }
                node.apply_logs();
            }
            let state_machine = node_arc.read().unwrap().state_machine.clone();
            let sm = state_machine.read().unwrap();
            return match sm.get(key) {
                Some(val) => serde_json::json!({"status": "ok", "data": val}),
                None => serde_json::json!({"status": "not_found"}),
            };
        }

        // Multi-node path: full ReadIndex for linearizable reads.
        let ri = match RaftNode::begin_read(node_arc.clone()) {
            Some(ri) => ri,
            None => return serde_json::json!({"error": "Not a leader. Please connect to the leader node."}),
        };

        let max_wait = Duration::from_millis(2000);
        let poll_interval = Duration::from_millis(10);
        let start = Instant::now();
        loop {
            let confirmed = {
                let node = node_arc.read().unwrap();
                node.confirm_read(ri)
            };
            if confirmed {
                break;
            }
            if start.elapsed() > max_wait {
                return serde_json::json!({
                    "status": "error",
                    "message": "read confirmation timeout (leader may have lost quorum)"
                });
            }
            tokio::time::sleep(poll_interval).await;
        }

        let state_machine = {
            let node = node_arc.read().unwrap();
            node.state_machine.clone()
        };
        let sm = state_machine.read().unwrap();
        match sm.get(key) {
            Some(val) => serde_json::json!({"status": "ok", "data": val}),
            None => serde_json::json!({"status": "not_found"}),
        }
    }

    /// Handles mutation commands (Set/Delete) by proposing them to the Raft log.
    /// Consolidates the redundant propose + sync_logs logic.
    async fn apply_mutation(command: Command, node_arc: &Arc<RwLock<RaftNode>>) -> serde_json::Value {
        let (success, index) = {
            let mut node = node_arc.write().unwrap();
            let ok = node.propose(command); // Appends to local WAL
            (ok, node.log.len() as u64)
        };

        if success {
            // Trigger log synchronization to followers immediately
            RaftNode::sync_logs(node_arc.clone());
            serde_json::json!({"status": "ok", "index": index})
        } else {
            serde_json::json!({"status": "error"})
        }
    }

    /// Begin a two-phase-commit transaction.
    ///
    /// Delegates to the leader-side coordinator
    /// (`crate::raft::coordinator::coordinate_tx`). The coordinator
    /// detects single-node vs multi-node membership and drives the
    /// appropriate path:
    ///   - **Single-node**: propose BeginTx + DecideTx(Commit) as one
    ///     batch.
    ///   - **Multi-node**: propose BeginTx, broadcast VoteRequest over
    ///     the multiplexed transport, apply the all-yes quorum policy
    ///     (textbook 2PC), then propose DecideTx(Commit | Abort).
    ///
    /// See `ROADMAP.md` P6 and `src/raft/coordinator.rs` for the locked
    /// decisions (coordinator = leader, all-yes quorum, side-channel
    /// vote transport).
    async fn begin_tx(
        tx_id: String,
        ops: Vec<crate::protocol::TxOp>,
        node_arc: &Arc<RwLock<RaftNode>>,
    ) -> serde_json::Value {
        let outcome =
            crate::raft::coordinator::coordinate_tx(node_arc.clone(), tx_id.clone(), ops).await;
        match outcome {
            crate::raft::coordinator::TxOutcome::Committed {
                begin_index,
                decide_index,
                tx_id,
            } => serde_json::json!({
                "status": "ok",
                "tx_id": tx_id,
                "decision": "commit",
                "begin_index": begin_index,
                "decide_index": decide_index,
            }),
            crate::raft::coordinator::TxOutcome::Aborted { tx_id, reason } => {
                serde_json::json!({
                    "status": "aborted",
                    "tx_id": tx_id,
                    "reason": reason,
                })
            }
            crate::raft::coordinator::TxOutcome::NotLeader { tx_id } => {
                serde_json::json!({
                    "status": "error",
                    "message": "not leader",
                    "tx_id": tx_id,
                })
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::node::RaftNode;
    use crate::raft::storage::RaftStorage;
    use crate::state_machine::{StateMachine, StateMachineConfig};
    use std::sync::{Arc, RwLock};

    /// Build a single-node (no-peers) RaftNode with on-disk state in a temp dir.
    fn make_single_node(node_id: &str) -> (tempfile::TempDir, Arc<RwLock<RaftNode>>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join(format!("{node_id}.wal")).to_str().unwrap().to_string();
        let meta = dir.path().join(format!("{node_id}_meta.json")).to_str().unwrap().to_string();
        let snap = dir.path().join(format!("{node_id}_snapshot.json")).to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join(format!("{node_id}_sm"));
        let sm_config = StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
        let mut node = RaftNode::new_with_storage(
            node_id.to_string(),
            vec![], // <-- no peers → single-node mode
            sm,
            storage,
        );
        // Main.rs auto-elevates a no-peers node to Leader; replicate that.
        node.state = crate::raft::node::NodeState::Leader;
        let arc = Arc::new(RwLock::new(node));
        (dir, arc)
    }

    #[test]
    fn is_single_node_true_when_peers_empty() {
        let (_d, node_arc) = make_single_node("solo");
        let node = node_arc.read().unwrap();
        assert!(node.is_single_node());
    }

    #[test]
    fn is_single_node_false_when_peers_present() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("n1.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("n1_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("n1_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join("n1_sm");
        let sm = Arc::new(RwLock::new(StateMachine::open(StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        }).unwrap()));
        let node = RaftNode::new_with_storage(
            "n1".to_string(),
            vec!["127.0.0.1:9002".into()],
            sm,
            storage,
        );
        assert!(!node.is_single_node());
    }

    #[tokio::test]
    async fn linearizable_get_returns_value_on_single_node_without_timeout() {
        // Regression: previously, Get on a single-node cluster timed out
        // because ReadIndex could never confirm a quorum heartbeat ack.
        let (_d, node_arc) = make_single_node("solo");

        // Seed a value via the same mutation path real clients use.
        let resp = ClientHandler::dispatch_command(
            Command::Set {
                key: "hello".to_string(),
                value: "world".to_string(),
            },
            &node_arc,
        )
        .await;
        assert_eq!(resp["status"], "ok", "Set must succeed: {resp}");

        // Get must return the value within the configured timeout window
        // without ever hitting the read-confirmation-timeout branch.
        let resp = ClientHandler::linearizable_get("hello", &node_arc).await;
        assert_eq!(resp["status"], "ok", "Get must succeed: {resp}");
        assert_eq!(resp["data"], "world");
    }

    #[tokio::test]
    async fn linearizable_get_returns_not_found_for_missing_key_on_single_node() {
        let (_d, node_arc) = make_single_node("solo");
        let resp = ClientHandler::linearizable_get("missing", &node_arc).await;
        assert_eq!(resp["status"], "not_found");
    }

    #[tokio::test]
    async fn linearizable_get_rejects_non_leader_on_single_node() {
        // Even with the fast path, a Follower/Candidate must never serve reads.
        let (_d, node_arc) = make_single_node("solo");
        {
            let mut node = node_arc.write().unwrap();
            node.state = crate::raft::node::NodeState::Follower;
        }
        let resp = ClientHandler::linearizable_get("anything", &node_arc).await;
        assert!(resp["error"].as_str().unwrap().contains("Not a leader"));
    }
}
