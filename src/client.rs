use crate::raft::node::{RaftNode, NodeState};
use crate::protocol::Command;
use std::sync::{Arc, RwLock};
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
        // Role check and state machine access (Original logic step 5)
        let (is_leader, state_machine_arc) = {
            let node = node_arc.read().unwrap();
            (node.state == NodeState::Leader, node.state_machine.clone())
        };

        if !is_leader {
            return serde_json::json!({"error":"Not a leader. Please connect to the leader node."});
        }

        // Command execution (Original logic step 6)
        match command {
            Command::Set { .. } | Command::Delete { .. } => {
                Self::apply_mutation(command, node_arc).await
            },
            Command::Get { key } => {
                let state_machine = state_machine_arc.read().unwrap();
                match state_machine.get(&key) {
                    Some(val) => serde_json::json!({"status": "ok", "data": val}),
                    None => serde_json::json!({"status": "not_found"}),
                }
            },
            Command::Compact => {
                serde_json::json!({"status": "error", "message": "compact not supported yet"})
            },
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
}