use crate::config::Config;
use crate::protocol::{Command, LogEntry};
use crate::raft::rpc::{AppendEntriesArgs, AppendReplyArgs, RequestVoteArgs, RpcClient, VoteResponseArgs};
use crate::raft::storage::RaftStorage;
use crate::state_machine::StateMachine;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum NodeState {
    Follower,
    Candidate,
    Leader
}

pub struct RaftNode {
    /// The underlying Key-Value store (State Machine)
    pub state_machine: Arc<RwLock<StateMachine>>,

    // --- Persistent state on all servers ---
    pub current_term: u64,
    pub vote_for: Option<String>,
    pub storage: RaftStorage,
    pub log: Vec<LogEntry>,

    // --- Volatile state on all servers ---
    pub state: NodeState,
    node_id: String,
    peers: Vec<String>,

    pub commit_index: u64,
    /// Index of highest log entry applied to state machine
    pub last_applied: u64,

    /// Last time a valid heartbeat or election was initiated
    pub last_heartbeat: Instant,

    // --- Volatile state on leaders ---
    pub next_index: HashMap<String, u64>,
    pub match_index: HashMap<String, u64>,
}

impl RaftNode {
    // inside impl RaftNode
    pub fn new(
        raft_addr: String,
        peers: Vec<String>,
        state_machine: Arc<RwLock<StateMachine>>
    ) -> Self {
        // 1. Initialize Storage component (paths come from global Config)
        let storage = RaftStorage::new();

        // Delegate to the test-friendly constructor so logic lives in one place.
        Self::new_with_storage(raft_addr, peers, state_machine, storage)
    }

    /// Construct a `RaftNode` with an explicit `RaftStorage` instance.
    /// Tests use this to isolate on-disk state in a temp dir without touching
    /// the global `Config`.
    pub fn new_with_storage(
        raft_addr: String,
        peers: Vec<String>,
        state_machine: Arc<RwLock<StateMachine>>,
        storage: RaftStorage,
    ) -> Self {
        // Load persistent state from disk
        let (term, vote, logs) = storage.load_initial_state();

        Self {
            storage,
            state_machine,

            // Populate from restored data
            current_term: term,
            vote_for: vote,
            log: logs,

            // Volatile state (resets on restart)
            state: NodeState::Follower,
            node_id: raft_addr,
            peers,
            commit_index: 0,
            last_applied: 0,
            last_heartbeat: Instant::now(),
            next_index: HashMap::new(),
            match_index: HashMap::new(),
        }
    }

    /// Helper to get the last log's index and term
    fn get_last_log_info(&self) -> (u64, u64) {
        self.log.last().map_or((0, 0), |entry| (entry.index as u64, entry.term))
    }

    pub fn handle_request_vote(&mut self, args: &RequestVoteArgs) -> VoteResponseArgs {
        // 1. Term check: Reject if candidate's term is older
        if args.term < self.current_term {
            return VoteResponseArgs {
                term: self.current_term,
                vote_granted: false,
            };
        }

        // 2. State transition: If candidate's term is newer, step down to Follower
        if args.term > self.current_term {
            self.current_term = args.term;
            self.state = NodeState::Follower;
            self.vote_for = None;

            if let Err(e) = self.storage.save_meta(self.current_term.clone(), self.vote_for.clone()) {
                eprintln!("[Critical] Failed to save metadata after term update: {}", e);
            }
        }

        // 3. Log safety check (Election Restriction)
        let (my_last_log_index, my_last_log_term) = self.get_last_log_info();

        // Log is up-to-date if:
        // (a) Candidate has a higher term in last log entry
        // (b) Same term, but candidate's log is at least as long as ours
        let is_log_up_to_date = (args.last_log_term > my_last_log_term) ||
            (args.last_log_term == my_last_log_term && args.last_log_index >= my_last_log_index);

        // 4. Voting decision
        let can_vote = self.vote_for.is_none() || self.vote_for == Some(args.candidate_id.clone());

        if can_vote && is_log_up_to_date {
            self.vote_for = Some(args.candidate_id.clone());
            let _ = self.storage.save_meta(self.current_term.clone(), self.vote_for.clone());
            self.last_heartbeat = Instant::now();

            VoteResponseArgs {
                term: self.current_term,
                vote_granted: true,
            }
        } else {
            VoteResponseArgs {
                term: self.current_term,
                vote_granted: false,
            }
        }
    }

    pub fn propose(&mut self, command: Command) -> bool {
        if self.state != NodeState::Leader {
            return false;
        }

        // 1. Create and append new log entry
        let new_index = self.log.len() as u64 + 1;
        let entry = LogEntry {
            term: self.current_term,
            index: new_index as usize,
            command,
        };

        if let Err(e) = self.storage.append_wal_log(&entry) {
            eprintln!("[Error] Failed to append wal log: {}", e);
            return false;
        }

        self.log.push(entry);
        true
    }

    pub fn sync_logs(raft_node: Arc<RwLock<Self>>) {
        let (current_term, node_id, commit_index, peers, log_len) = {
            let n = raft_node.read().unwrap();
            (n.current_term, n.node_id.clone(), n.commit_index, n.peers.clone(), n.log.len() as u64)
        };

        for peer_addr in peers {
            let raft_clone = raft_node.clone();
            let peer_addr_clone = peer_addr.clone();

            let (prev_log_index, prev_log_term, entries) = {
                let n = raft_node.read().unwrap();
                let next = *n.next_index.get(&peer_addr_clone).unwrap_or(&(log_len + 1));
                let prev_idx = next - 1;
                let prev_term = if prev_idx == 0 {
                    0
                } else {
                    // Logic: prev_idx 1 maps to log index 0
                    n.log.get(prev_idx as usize - 1).map(|e| e.term).unwrap_or(0)
                };
                let ents = if next <= log_len {
                    n.log[next as usize - 1..].to_vec()
                } else {
                    vec![]
                };
                (prev_idx, prev_term, ents)
            };

            let args = AppendEntriesArgs {
                term: current_term,
                leader_id: node_id.clone(),
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit: commit_index,
            };

            tokio::spawn(async move {
                match RpcClient::send_append_entries_rpc(peer_addr_clone.clone(), args.clone()).await {
                    Ok(reply) => {
                        let mut n = raft_clone.write().unwrap();
                        if reply.success {
                            let last_idx = args.prev_log_index + args.entries.len() as u64;
                            n.match_index.insert(peer_addr_clone.clone(), last_idx);
                            n.next_index.insert(peer_addr_clone.clone(), last_idx + 1);
                            n.maybe_commit();
                        } else if reply.term > n.current_term {
                            n.current_term = reply.term;
                            n.state = NodeState::Follower;
                            n.vote_for = None;
                            let _ = n.storage.save_meta(n.current_term.clone(), n.vote_for.clone());
                        } else {
                            // Log inconsistency: decrement next_index and retry
                            let next = n.next_index.get(&peer_addr_clone).cloned().unwrap_or(1);
                            if next > 1 {
                                n.next_index.insert(peer_addr_clone, next - 1);
                            }
                        }
                    }
                    Err(e) => eprintln!("[Network] RPC error with {}: {}", peer_addr_clone, e),
                }
            });
        }
    }

    pub fn maybe_commit(&mut self) {
        if self.state != NodeState::Leader { return; }

        let mut match_indices: Vec<u64> = self.match_index.values().cloned().collect();
        match_indices.push(self.log.len() as u64); // Include self
        match_indices.sort_by(|a, b| b.cmp(a));

        // Find the majority consensus index
        let quorum_idx = match_indices.len() / 2;
        let n = match_indices[quorum_idx];

        if n > self.commit_index {
            // Safety: Leader can only commit entries from its current term
            let log_term = self.log.get((n - 1) as usize).map(|e| e.term).unwrap_or(0);
            if log_term == self.current_term {
                self.commit_index = n;
                println!("🚀 [Commit] Majority reached! Commit Index advanced to {}", n);
                self.apply_logs();
            }
        }
    }

    pub fn apply_logs(&mut self) {
        while self.last_applied < self.commit_index {
            let log_idx_to_apply = self.last_applied as usize;

            if let Some(entry) = self.log.get(log_idx_to_apply) {
                let mut state_machine = self.state_machine.write().unwrap();
                match &entry.command {
                    Command::Set { key, value } => {
                        let _ = state_machine.set(&*key.clone(), &*value.clone());
                        println!("✅ [Apply] Index {}: SET {} = {}", entry.index, key, value);
                    }
                    Command::Delete { key } => {
                        let _ = state_machine.delete(key);
                        println!("✅ [Apply] Index {}: DELETE {}", entry.index, key);
                    }
                    _ => println!("🔍 [Apply] Index {}: No-op", entry.index),
                }
                self.last_applied += 1;
            } else {
                eprintln!("[Critical] Log entry {} not found during apply", self.last_applied + 1);
                break;
            }
        }
    }

    pub fn replay_logs(&mut self) {
        let mut state_machine = self.state_machine.write().unwrap();
        for entry in &self.log {
            match &entry.command {
                Command::Set { key, value } => { let _ = state_machine.set(&*key.clone(), &*value.clone()); }
                Command::Delete { key } => { let _ = state_machine.delete(key); }
                _ => {}
            }
        }
        self.last_applied = self.log.len() as u64;
        println!("✅ [Replay] Successfully replayed {} logs to state machine", self.log.len());
    }

    pub async fn run_heartbeat_loop(node_arc: Arc<RwLock<RaftNode>>) {
        let mut interval = tokio::time::interval(Duration::from_millis(Config::heartbeat_interval_ms()));
        loop {
            interval.tick().await;
            let is_leader = node_arc.read().unwrap().state == NodeState::Leader;
            if is_leader {
                Self::sync_logs(node_arc.clone());
            }
        }
    }

    pub fn become_candidate(raft_node: Arc<RwLock<Self>>) {
        let mut node = raft_node.write().unwrap();
        node.current_term += 1;
        node.state = NodeState::Candidate;
        node.vote_for = Some(node.node_id.clone());
        let _ =  node.storage.save_meta(node.current_term.clone(), node.vote_for.clone());
        node.last_heartbeat = Instant::now();

        println!("🗳️ Node {} candidate for Term {}", node.node_id, node.current_term);
        drop(node);
        Self::request_votes(raft_node);
    }

    pub fn request_votes(raft_arc: Arc<RwLock<Self>>) {
        let (peers, term, candidate_id, last_idx, last_term) = {
            let node = raft_arc.read().unwrap();
            let (li, lt) = node.get_last_log_info();
            (node.peers.clone(), node.current_term, node.node_id.clone(), li, lt)
        };

        let total_nodes = peers.len() + 1;
        let votes_received = Arc::new(std::sync::atomic::AtomicUsize::new(1));

        for peer_addr in peers {
            let raft_clone = raft_arc.clone();
            let votes_clone = votes_received.clone();
            let cid = candidate_id.clone();

            tokio::spawn(async move {
                let args = RequestVoteArgs { term, candidate_id: cid, last_log_index: last_idx, last_log_term: last_term };
                if let Ok(reply) = RpcClient::send_request_vote_rpc(&peer_addr, args).await {
                    if reply.vote_granted {
                        let count = votes_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if count > total_nodes / 2 {
                            let mut n = raft_clone.write().unwrap();
                            if n.state == NodeState::Candidate && n.current_term == term {
                                n.become_leader();
                            }
                        }
                    } else if reply.term > term {
                        let mut n = raft_clone.write().unwrap();
                        n.current_term = reply.term;
                        n.state = NodeState::Follower;
                        n.vote_for = None;
                        let _ = n.storage.save_meta(n.current_term.clone(), n.vote_for.clone());
                    }
                }
            });
        }
    }

    pub fn become_leader(&mut self) {
        if self.state == NodeState::Leader { return; }
        println!("👑 [Leader] Node {} elected for Term {}", self.node_id, self.current_term);
        self.state = NodeState::Leader;
        let next_idx = self.log.len() as u64 + 1;
        self.next_index = self.peers.iter().map(|p| (p.clone(), next_idx)).collect();
        self.last_heartbeat = Instant::now();
    }

    pub fn handle_append_entries(&mut self, args: &AppendEntriesArgs) -> AppendReplyArgs {
        if args.term < self.current_term {
            return AppendReplyArgs { term: self.current_term, success: false };
        }

        if args.term > self.current_term {
            self.current_term = args.term;
            self.vote_for = None;
            let _ = self.storage.save_meta(self.current_term.clone(), self.vote_for.clone());
        }
        self.state = NodeState::Follower;
        self.last_heartbeat = Instant::now();

        // Consistent check
        if args.prev_log_index > 0 {
            let local_term = self.log.get((args.prev_log_index - 1) as usize).map(|e| e.term);
            if local_term != Some(args.prev_log_term) {
                return AppendReplyArgs { term: self.current_term, success: false };
            }
        }

        // Append entries and resolve conflicts
        for entry in &args.entries {
            let idx = (entry.index - 1) as usize;
            if idx < self.log.len() {
                if self.log[idx].term != entry.term {
                    self.log.truncate(idx);
                    self.log.push(entry.clone());
                    let _ = self.storage.append_wal_log(entry);
                }
            } else {
                self.log.push(entry.clone());
                let _ = self.storage.append_wal_log(entry);
            }
        }

        if args.leader_commit > self.commit_index {
            self.commit_index = std::cmp::min(args.leader_commit, self.log.len() as u64);
            self.apply_logs();
        }

        AppendReplyArgs { term: self.current_term, success: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Command, LogEntry};
    use crate::raft::rpc::{AppendEntriesArgs, RequestVoteArgs};
    use crate::raft::storage::RaftStorage;
    use crate::state_machine::StateMachine;
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    /// Build a `RaftNode` rooted in a fresh temp dir so each test is isolated
    /// on disk and does not depend on the global `Config`.
    fn make_node(node_id: &str, peers: Vec<String>) -> (TempDir, RaftNode) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join(format!("{node_id}.wal")).to_str().unwrap().to_string();
        let meta = dir.path().join(format!("{node_id}_meta.json")).to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta);
        let sm = Arc::new(RwLock::new(StateMachine::open().unwrap()));
        let node = RaftNode::new_with_storage(node_id.to_string(), peers, sm, storage);
        (dir, node)
    }

    fn vote_args(term: u64, candidate: &str, last_idx: u64, last_term: u64) -> RequestVoteArgs {
        RequestVoteArgs {
            term,
            candidate_id: candidate.to_string(),
            last_log_index: last_idx,
            last_log_term: last_term,
        }
    }

    fn append_args(term: u64, leader: &str, prev_idx: u64, prev_term: u64,
                   entries: Vec<LogEntry>, leader_commit: u64) -> AppendEntriesArgs {
        AppendEntriesArgs {
            term,
            leader_id: leader.to_string(),
            prev_log_index: prev_idx,
            prev_log_term: prev_term,
            entries,
            leader_commit,
        }
    }

    fn make_entry(term: u64, index: usize, key: &str, value: &str) -> LogEntry {
        LogEntry {
            term,
            index,
            command: Command::Set {
                key: key.to_string(),
                value: value.to_string(),
            },
        }
    }

    fn assert_node_state(node: &RaftNode, key: &str, expected: Option<&str>) {
        let sm = node.state_machine.read().unwrap();
        assert_eq!(sm.get(key), expected.map(|s| s.to_string()).as_ref(),
            "state machine mismatch for key={key}");
    }

    // ---------- handle_request_vote ----------

    #[test]
    fn vote_rejects_when_candidate_term_is_older() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;

        let reply = node.handle_request_vote(&vote_args(4, "n2", 0, 0));

        assert!(!reply.vote_granted);
        assert_eq!(reply.term, 5);
        // Local state should be untouched.
        assert_eq!(node.current_term, 5);
        assert!(node.vote_for.is_none());
    }

    #[test]
    fn vote_steps_down_and_grants_on_newer_term_when_log_is_up_to_date() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Candidate;
        node.current_term = 1;
        node.vote_for = Some("n1".into());
        // Local log: one entry at term=1, index=1.
        node.log = vec![make_entry(1, 1, "k", "v")];

        let reply = node.handle_request_vote(&vote_args(2, "n2", 1, 1));

        assert!(reply.vote_granted);
        assert_eq!(reply.term, 2);
        // Stepped down to Follower and persisted the vote.
        assert_eq!(node.state, NodeState::Follower);
        assert_eq!(node.current_term, 2);
        assert_eq!(node.vote_for.as_deref(), Some("n2"));
    }

    #[test]
    fn vote_denies_when_already_voted_for_someone_else_in_same_term() {
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.current_term = 3;
        node.vote_for = Some("n2".into());

        let reply = node.handle_request_vote(&vote_args(3, "n3", 0, 0));

        assert!(!reply.vote_granted);
        // vote_for must not be silently overwritten.
        assert_eq!(node.vote_for.as_deref(), Some("n2"));
    }

    #[test]
    fn vote_grants_when_voted_for_same_candidate_in_same_term() {
        // Idempotency: a duplicate RequestVote from the same candidate should
        // re-grant (no harm done).
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 2;
        node.vote_for = Some("n2".into());

        let reply = node.handle_request_vote(&vote_args(2, "n2", 0, 0));

        assert!(reply.vote_granted);
    }

    #[test]
    fn vote_denies_when_candidate_log_is_behind_election_restriction() {
        // §5.4.1: only candidates whose log is at least as up-to-date may be elected.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;
        // Local log has a term=3 entry at index=2 (newer term but longer log).
        node.log = vec![make_entry(2, 1, "a", "1"), make_entry(3, 2, "b", "2")];

        // Candidate's last entry is term=2, index=1 — strictly behind us.
        let reply = node.handle_request_vote(&vote_args(5, "n2", 1, 2));

        assert!(!reply.vote_granted);
        assert!(node.vote_for.is_none(), "must not vote when log is stale");
    }

    #[test]
    fn vote_grants_when_candidate_log_is_strictly_ahead() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 3;
        node.log = vec![make_entry(2, 1, "a", "1")];

        // Candidate has term=3 at index=2 — strictly ahead (newer term + longer).
        let reply = node.handle_request_vote(&vote_args(3, "n2", 2, 3));

        assert!(reply.vote_granted);
    }

    #[test]
    fn vote_persists_term_bump_to_disk() {
        let (d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 1;
        node.log = vec![make_entry(1, 1, "k", "v")];

        let _ = node.handle_request_vote(&vote_args(2, "n2", 1, 1));

        // After vote, the meta file on disk should reflect the new term + vote.
        let meta_path = d.path().join("n1_meta.json");
        let raw = std::fs::read_to_string(&meta_path).expect("meta exists");
        assert!(raw.contains("\"current_term\":2"), "raw was: {raw}");
        assert!(raw.contains("\"n2\""), "raw was: {raw}");
    }

    // ---------- handle_append_entries ----------

    #[test]
    fn append_rejects_stale_term() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;

        let reply = node.handle_append_entries(&append_args(4, "n2", 0, 0, vec![], 0));

        assert!(!reply.success);
        assert_eq!(reply.term, 5);
    }

    #[test]
    fn append_steps_down_on_newer_term() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Candidate;
        node.current_term = 1;
        node.vote_for = Some("n1".into());

        let reply = node.handle_append_entries(&append_args(2, "n2", 0, 0, vec![], 0));

        assert!(reply.success);
        assert_eq!(node.state, NodeState::Follower);
        assert_eq!(node.current_term, 2);
    }

    #[test]
    fn append_heartbeat_with_empty_entries_succeeds() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 2;
        node.log = vec![make_entry(2, 1, "k", "v")];

        let reply = node.handle_append_entries(&append_args(2, "n2", 1, 2, vec![], 0));

        assert!(reply.success);
        assert_eq!(node.log.len(), 1, "no entries should be added");
    }

    #[test]
    fn append_rejects_when_prev_log_term_mismatches() {
        // Consistency check: if the follower's entry at prev_log_index has a
        // different term, the leader must back off.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 2;
        node.log = vec![make_entry(1, 1, "a", "1"), make_entry(1, 2, "b", "2")];

        // Leader claims prev_log_index=2 has term=2, but local has term=1.
        let reply = node.handle_append_entries(&append_args(2, "n2", 2, 2, vec![], 0));

        assert!(!reply.success);
    }

    #[test]
    fn append_truncates_conflicting_entries_then_appends_new() {
        // Figure 2 scenario: leader and follower disagree on a middle entry.
        // The follower's tail after prev_log_index must be discarded and replaced.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 2;
        node.log = vec![
            make_entry(1, 1, "a", "1"),
            make_entry(1, 2, "b", "2"), // <-- conflict at index 2 (term 1 vs leader's term 2)
        ];

        let new_entries = vec![
            LogEntry { term: 2, index: 2, command: Command::Set { key: "b".into(), value: "v2".into() } },
            LogEntry { term: 2, index: 3, command: Command::Set { key: "c".into(), value: "v3".into() } },
        ];

        let reply = node.handle_append_entries(&append_args(2, "n2", 1, 1, new_entries, 0));

        assert!(reply.success);
        assert_eq!(node.log.len(), 3);
        assert_eq!(node.log[1].term, 2, "conflicting entry must be replaced");
        assert_eq!(node.log[2].term, 2, "new entry must be appended");
    }

    #[test]
    fn append_advances_commit_index_and_applies_logs() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 2;
        node.log = vec![make_entry(1, 1, "a", "1")];

        let entries = vec![
            LogEntry { term: 2, index: 2, command: Command::Set { key: "b".into(), value: "B".into() } },
            LogEntry { term: 2, index: 3, command: Command::Set { key: "c".into(), value: "C".into() } },
        ];

        let _ = node.handle_append_entries(&append_args(2, "n2", 1, 1, entries, 2));

        assert_eq!(node.commit_index, 2);
        assert_eq!(node.last_applied, 2);
        // The two committed SETs must be reflected in the state machine.
        assert_node_state(&node, "b", Some("B"));
    }

    #[test]
    fn append_does_not_advance_commit_beyond_local_log() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 1;
        node.log = vec![make_entry(1, 1, "a", "1")];

        // Leader claims commit=10 but log only has 1 entry.
        let _ = node.handle_append_entries(&append_args(1, "n2", 0, 0, vec![], 10));

        // Should clamp to log length, not over-advance.
        assert_eq!(node.commit_index, 1);
    }

    // ---------- maybe_commit ----------

    #[test]
    fn commit_advances_when_majority_replicates() {
        // 3-node cluster: self + 2 peers. Majority = 2.
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.state = NodeState::Leader;
        node.current_term = 2;
        node.log = vec![
            make_entry(2, 1, "a", "1"),
            make_entry(2, 2, "b", "2"),
        ];
        // One peer has replicated up to index 2, the other only to 1.
        node.match_index.insert("n2".into(), 2);
        node.match_index.insert("n3".into(), 1);

        node.maybe_commit();

        // Self + n2 agree on index 2 → majority quorum.
        assert_eq!(node.commit_index, 2);
        assert_eq!(node.last_applied, 2);
    }

    #[test]
    fn commit_does_not_advance_without_majority() {
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.state = NodeState::Leader;
        node.current_term = 1;
        node.log = vec![make_entry(1, 1, "a", "1"), make_entry(1, 2, "b", "2")];
        // Neither peer has replicated yet.
        node.match_index.insert("n2".into(), 0);
        node.match_index.insert("n3".into(), 0);

        node.maybe_commit();

        assert_eq!(node.commit_index, 0, "no peer replication means no commit");
    }

    #[test]
    fn commit_safety_does_not_commit_previous_term_entries() {
        // §5.4.2 / Figure 2 safety: a leader must not commit entries from previous
        // terms by counting replicas alone. Only entries from the leader's current
        // term can be directly committed.
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.state = NodeState::Leader;
        node.current_term = 2;
        // Log: term=1 entries, then a single term=2 entry at index=3.
        node.log = vec![
            make_entry(1, 1, "a", "1"),
            make_entry(1, 2, "b", "2"),
            make_entry(2, 3, "c", "3"),
        ];
        node.match_index.insert("n2".into(), 3);
        node.match_index.insert("n3".into(), 3);

        node.maybe_commit();

        // Only index 3 (term=2) can be committed; index 2 (term=1) must wait.
        assert_eq!(node.commit_index, 3, "must skip ahead to current-term entry");
        assert_eq!(node.last_applied, 3);
    }

    #[test]
    fn commit_noop_when_not_leader() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Follower;
        node.current_term = 1;
        node.log = vec![make_entry(1, 1, "a", "1")];
        node.match_index.insert("n2".into(), 1);

        node.maybe_commit();

        assert_eq!(node.commit_index, 0);
    }

    // ---------- propose ----------

    #[test]
    fn propose_rejects_when_not_leader() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Follower;

        let accepted = node.propose(Command::Set {
            key: "k".into(),
            value: "v".into(),
        });

        assert!(!accepted);
        assert!(node.log.is_empty());
    }

    #[test]
    fn propose_appends_entry_and_writes_wal() {
        let (d, mut node) = make_node("n1", vec![]);
        node.state = NodeState::Leader;
        node.current_term = 1;

        let accepted = node.propose(Command::Set {
            key: "k".into(),
            value: "v".into(),
        });

        assert!(accepted);
        assert_eq!(node.log.len(), 1);
        assert_eq!(node.log[0].index, 1);
        assert_eq!(node.log[0].term, 1);

        // WAL must reflect the new entry — proves durability path works.
        let wal_path = d.path().join("n1.wal");
        let bytes = std::fs::read(&wal_path).expect("wal file");
        assert!(!bytes.is_empty(), "WAL must have been written to disk");
    }

    #[test]
    fn propose_increments_index_per_call() {
        let (_d, mut node) = make_node("n1", vec![]);
        node.state = NodeState::Leader;
        node.current_term = 1;

        node.propose(Command::Set { key: "a".into(), value: "1".into() }).then_some(()).unwrap();
        node.propose(Command::Set { key: "b".into(), value: "2".into() }).then_some(()).unwrap();
        node.propose(Command::Set { key: "c".into(), value: "3".into() }).then_some(()).unwrap();

        assert_eq!(node.log.len(), 3);
        assert_eq!(node.log[0].index, 1);
        assert_eq!(node.log[1].index, 2);
        assert_eq!(node.log[2].index, 3);
    }

    // ---------- apply_logs / replay_logs ----------

    #[test]
    fn apply_logs_skips_compact_noops() {
        let (_d, mut node) = make_node("n1", vec![]);
        node.log = vec![LogEntry {
            term: 1,
            index: 1,
            command: Command::Compact,
        }];
        node.commit_index = 1;

        node.apply_logs();

        assert_eq!(node.last_applied, 1);
        // Nothing should have been written to the state machine.
        assert_node_state(&node, "k", None);
    }

    #[test]
    fn replay_logs_restores_state_machine_from_scratch() {
        let sm = Arc::new(RwLock::new(StateMachine::open().unwrap()));
        let node_id = "replay_node";
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join(format!("{node_id}.wal")).to_str().unwrap().to_string();
        let meta = dir.path().join(format!("{node_id}_meta.json")).to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta);

        // Simulate a fresh WAL with three committed entries.
        storage.append_wal_log(&make_entry(1, 1, "a", "1")).unwrap();
        storage.append_wal_log(&make_entry(1, 2, "b", "2")).unwrap();
        storage.append_wal_log(&make_entry(1, 3, "c", "3")).unwrap();

        // Construct node from disk; it should replay all three.
        let mut node = RaftNode::new_with_storage(
            node_id.to_string(),
            vec![],
            sm.clone(),
            storage,
        );
        node.replay_logs();

        assert_eq!(node.last_applied, 3);
        let sm_read = sm.read().unwrap();
        assert_eq!(sm_read.get("a"), Some(&"1".to_string()));
        assert_eq!(sm_read.get("b"), Some(&"2".to_string()));
        assert_eq!(sm_read.get("c"), Some(&"3".to_string()));
    }

    #[test]
    fn apply_logs_handles_delete_command() {
        let (_d, mut node) = make_node("n1", vec![]);
        node.log = vec![
            make_entry(1, 1, "k", "v"),
            LogEntry { term: 1, index: 2, command: Command::Delete { key: "k".into() } },
        ];
        node.commit_index = 2;

        node.apply_logs();

        assert_eq!(node.last_applied, 2);
        assert_node_state(&node, "k", None);
    }

    // ---------- get_last_log_info ----------

    #[test]
    fn last_log_info_on_empty_log_is_zero_zero() {
        let (_d, node) = make_node("n1", vec![]);
        let (idx, term) = node.get_last_log_info();
        assert_eq!((idx, term), (0, 0));
    }

    #[test]
    fn last_log_info_reflects_most_recent_entry() {
        let (_d, mut node) = make_node("n1", vec![]);
        node.log = vec![make_entry(1, 1, "a", "1"), make_entry(3, 2, "b", "2")];

        let (idx, term) = node.get_last_log_info();
        assert_eq!(idx, 2);
        assert_eq!(term, 3);
    }

    // ---------- become_leader ----------

    #[test]
    fn become_leader_initializes_next_index_to_log_tail_plus_one() {
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.log = vec![make_entry(1, 1, "a", "1"), make_entry(1, 2, "b", "2")];

        node.become_leader();

        assert_eq!(node.state, NodeState::Leader);
        // next_index for each peer should point just past our log tail.
        assert_eq!(node.next_index.get("n2"), Some(&3));
        assert_eq!(node.next_index.get("n3"), Some(&3));
    }

    #[test]
    fn become_leader_is_idempotent() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.become_leader();
        let next_idx_before = *node.next_index.get("n2").unwrap();

        // Calling again must not reset next_index (which would force a full log resend).
        node.become_leader();
        assert_eq!(node.next_index.get("n2"), Some(&next_idx_before));
    }
}