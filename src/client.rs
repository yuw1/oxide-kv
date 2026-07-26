use crate::raft::node::{RaftNode, NodeState};
use crate::protocol::{Command, TxDecision};
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
    async fn dispatch_command(command: Command, node_arc: &Arc<RwLock<RaftNode>>) -> serde_json::Value {
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
            Command::Vote { .. } | Command::DecideTx { .. } => {
                // Manual 2PC control commands — useful for tests and for a
                // future coordinator that wants to drive the lifecycle
                // explicitly. Treat as mutations so they go through Raft.
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
    async fn linearizable_get(key: &str, node_arc: &Arc<RwLock<RaftNode>>) -> serde_json::Value {
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
    /// On a single-node cluster the leader is the sole participant, so we
    /// auto-append the matching `DecideTx(Commit)` right after `BeginTx`.
    /// On a multi-node cluster the coordinator would instead solicit votes
    /// via `Vote` entries and only then append `DecideTx`.
    async fn begin_tx(
        tx_id: String,
        ops: Vec<crate::protocol::TxOp>,
        node_arc: &Arc<RwLock<RaftNode>>,
    ) -> serde_json::Value {
        let mut entries = vec![Command::BeginTx { tx_id: tx_id.clone(), ops }];
        entries.push(Command::DecideTx {
            tx_id: tx_id.clone(),
            decision: TxDecision::Commit,
        });

        // Propose both as a single batch so they commit together.
        let (success, first_index) = {
            let mut node = node_arc.write().unwrap();
            let ok = node.propose_batch(entries);
            let first = node.log.len().saturating_sub(1) as u64;
            (ok, first)
        };

        if success {
            RaftNode::sync_logs(node_arc.clone());
            serde_json::json!({"status": "ok", "tx_id": tx_id, "index": first_index})
        } else {
            serde_json::json!({"status": "error"})
        }
    }
}