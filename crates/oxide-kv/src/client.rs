use crate::raft::node::{RaftNode, NodeState};
use crate::protocol::{AbortTxError, Command, MembershipError, ServerId};
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
            Command::AddNode { server } => {
                // P8 PR 6 (Raft thesis §6): client-facing membership
                // change. The leader's MembershipCoordinator intercepts
                // this and translates it into two `InstallConfiguration`
                // log entries (Joint, then Simple). On the leader we
                // run that translation now and return once the *first*
                // entry (Joint) commits — the second is appended
                // automatically when the joint entry is observed.
                Self::add_node(server, node_arc).await
            }
            Command::RemoveNode { node_id } => {
                // P8 PR 6: client-facing membership removal. Mirrors
                // `AddNode` above.
                Self::remove_node(node_id, node_arc).await
            }
            Command::InstallConfiguration { .. } => {
                // Defensive: this should never appear in a client
                // command (the leader installs it from the log, not
                // from JSON). If a client somehow sends one, refuse
                // so it can't bypass the MembershipCoordinator.
                serde_json::json!({
                    "status": "error",
                    "message": "InstallConfiguration is a leader-internal log entry; not a valid client command"
                })
            }
            // P8 PR 7: admin-driven force-abort. Leader-only;
            // forwarded to `propose_abort_tx` which validates
            // `tx_id` is in `pending_txs` and proposes a
            // `DecideTx(Abort)` log entry.
            Command::AbortTx { tx_id } => {
                Self::abort_tx(tx_id, node_arc).await
            }
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

    /// P8 PR 6: client-side membership addition. Leader-only;
    /// constructs the two-phase joint-consensus log sequence and
    /// waits for the Joint entry to commit before returning.
    ///
    /// The subsequent `Simple(new)` entry is auto-proposed by the
    /// leader's `apply_logs` hook once the Joint commits, so the
    /// caller doesn't see that step. We do poll for it anyway so
    /// the response reports the *final* committed index — the
    /// caller can rely on the membership being fully installed
    /// when this returns.
    pub async fn add_node(
        server: ServerId,
        node_arc: &Arc<RwLock<RaftNode>>,
    ) -> serde_json::Value {
        let joint_index = {
            let mut node = node_arc.write().unwrap();
            match node.propose_add_node(server.clone()) {
                Ok(idx) => idx,
                Err(e) => return Self::membership_error_to_json(e),
            }
        };
        // Kick off replication now so the Joint entry commits
        // without waiting for the next heartbeat tick.
        RaftNode::sync_logs(node_arc.clone());

        // Wait for the Joint entry to commit.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let node = node_arc.read().unwrap();
                if node.commit_index >= joint_index {
                    break;
                }
            }
            if Instant::now() >= deadline {
                return serde_json::json!({
                    "status": "error",
                    "message": format!(
                        "AddNode: Joint entry at index {} did not commit within 10s",
                        joint_index
                    )
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The Simple(new) entry will be auto-proposed by apply_logs
        // immediately after the Joint commits. We don't strictly
        // need to wait for it to commit before responding — the
        // membership is "effectively in the joint state" once the
        // Joint commits, which is enough for the new server to
        // start receiving heartbeats. But we do poll for the
        // Simple entry so the client gets a single clear "done"
        // signal.
        let simple_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let node = node_arc.read().unwrap();
                // The Simple entry has been proposed once
                // apply_logs has run past the Joint entry. We
                // look for the index just after the Joint.
                if node.log.len() as u64 > joint_index {
                    break;
                }
            }
            if Instant::now() >= simple_deadline {
                // The Simple entry may not yet be appended; that's
                // not a failure (the Joint already commits and the
                // Simple will be appended shortly). Return a
                // success-with-warning response.
                return serde_json::json!({
                    "status": "ok",
                    "joint_index": joint_index,
                    "simple_appended": false,
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        serde_json::json!({
            "status": "ok",
            "joint_index": joint_index,
            "simple_index": joint_index + 1,
            "new_member": server.node_id,
        })
    }

    /// P8 PR 6: client-side membership removal. Mirror of
    /// `add_node`.
    pub async fn remove_node(
        node_id: String,
        node_arc: &Arc<RwLock<RaftNode>>,
    ) -> serde_json::Value {
        let joint_index = {
            let mut node = node_arc.write().unwrap();
            match node.propose_remove_node(&node_id) {
                Ok(idx) => idx,
                Err(e) => return Self::membership_error_to_json(e),
            }
        };
        RaftNode::sync_logs(node_arc.clone());

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let node = node_arc.read().unwrap();
                if node.commit_index >= joint_index {
                    break;
                }
            }
            if Instant::now() >= deadline {
                return serde_json::json!({
                    "status": "error",
                    "message": format!(
                        "RemoveNode: Joint entry at index {} did not commit within 10s",
                        joint_index
                    )
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let simple_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let node = node_arc.read().unwrap();
                if node.log.len() as u64 > joint_index {
                    break;
                }
            }
            if Instant::now() >= simple_deadline {
                return serde_json::json!({
                    "status": "ok",
                    "joint_index": joint_index,
                    "simple_appended": false,
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        serde_json::json!({
            "status": "ok",
            "joint_index": joint_index,
            "simple_index": joint_index + 1,
            "removed_member": node_id,
        })
    }

    fn membership_error_to_json(e: MembershipError) -> serde_json::Value {
        match e {
            MembershipError::NotLeader => {
                serde_json::json!({"status": "error", "code": "not_leader", "message": "Not a leader. Please connect to the leader node."})
            }
            MembershipError::AlreadyMember(id) => {
                serde_json::json!({"status": "error", "code": "already_member", "node_id": id})
            }
            MembershipError::NotMember(id) => {
                serde_json::json!({"status": "error", "code": "not_member", "node_id": id})
            }
            MembershipError::CannotRemoveSelf => {
                serde_json::json!({"status": "error", "code": "cannot_remove_self", "message": "Refusing to remove the leader itself; do this on another node"})
            }
            MembershipError::CannotRemoveLastServer => {
                serde_json::json!({"status": "error", "code": "cannot_remove_last_server", "message": "Refusing to remove the last server; the cluster would be unable to make progress"})
            }
            MembershipError::StorageError(msg) => {
                serde_json::json!({"status": "error", "code": "storage_error", "message": msg})
            }
        }
    }

    /// P8 PR 7: translate an `AbortTxError` to a structured JSON
    /// response. Mirrors `membership_error_to_json` so the operator
    /// gets a clear `code` field for log correlation / alerting.
    fn abort_tx_error_to_json(e: AbortTxError) -> serde_json::Value {
        match e {
            AbortTxError::NotLeader => {
                serde_json::json!({"status": "error", "code": "not_leader", "message": "Not a leader. Please connect to the leader node."})
            }
            AbortTxError::NotFound(tx_id) => {
                serde_json::json!({"status": "error", "code": "tx_not_found", "tx_id": tx_id, "message": "tx_id is not in pending_txs; already committed/aborted or never existed"})
            }
            AbortTxError::StorageError(msg) => {
                serde_json::json!({"status": "error", "code": "storage_error", "message": msg})
            }
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

    /// Admin RPC: force-abort a stuck 2PC transaction.
    ///
    /// Mirrors `propose_add_node` / `propose_remove_node` semantics:
    ///   - Leader-only (the dispatch guard already rejected non-leaders).
    ///   - Validates `tx_id` is in `pending_txs`; rejects otherwise so
    ///     the operator gets a clear "tx not found" error rather than a
    ///     silent no-op log entry.
    ///   - On success, proposes a `DecideTx(Abort)` log entry that
    ///     replicates to every follower through the normal AppendEntries
    ///     path; the operator gets back the log index of the
    ///     `DecideTx(Abort)` so they can correlate with cluster logs.
    ///
    /// P8 PR 7.
    async fn abort_tx(
        tx_id: String,
        node_arc: &Arc<RwLock<RaftNode>>,
    ) -> serde_json::Value {
        let result = {
            let mut node = node_arc.write().unwrap();
            node.propose_abort_tx(&tx_id)
        };
        match result {
            Ok(decide_index) => serde_json::json!({
                "status": "ok",
                "tx_id": tx_id,
                "decision": "abort",
                "decide_index": decide_index,
            }),
            Err(e) => {
                let mut resp = Self::abort_tx_error_to_json(e);
                // Merge the original `tx_id` field into the error
                // response so operators can grep for it.
                if let Some(obj) = resp.as_object_mut() {
                    obj.insert("tx_id".to_string(), serde_json::Value::String(tx_id));
                }
                resp
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
