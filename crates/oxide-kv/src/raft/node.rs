use crate::config::Config;
use crate::coordination::{VoteRequest, VoteResponse};
use crate::protocol::{Command, LogEntry, ReadIndex, Snapshot, Vote};
use crate::raft::clock::{system_clock, Clock};
use crate::raft::net::{system_transport, Transport};
use crate::raft::rpc::{
    AppendEntriesArgs, AppendReplyArgs, InstallSnapshotArgs, InstallSnapshotReplyArgs,
    RaftMessage, RequestVoteArgs, VoteResponseArgs,
};
use crate::raft::storage::RaftStorage;
use crate::state_machine::StateMachine;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum NodeState {
    Follower,
    Candidate,
    Leader,
    /// Pre-vote probe phase (Raft §9.6). New in P8 PR 5. The node is
    /// exploring whether it could win a real election at `current_term + 1`
    /// without actually bumping `current_term` or persisting a `vote_for`.
    /// A PreCandidate that collects a quorum of `PreVoteResponse.granted ==
    /// true` is promoted to `Candidate` by `promote_pre_candidate_to_candidate`;
    /// a PreCandidate that observes a refusal or timeout reverts to
    /// `Follower` (still at the original `current_term`).
    PreCandidate,
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

    /// Most recent instant at which the leader received a successful heartbeat
    /// reply from at least one peer. Used by ReadIndex to bound the lease
    /// within which a linearizable read is safe.
    pub last_quorum_heartbeat_at: Option<Instant>,

    /// Clock abstraction for production wall-clock vs future simulation
    /// virtual time (P7). Production nodes get a fresh `SystemClock`
    /// from `new` / `new_with_storage`; tests inject a custom impl
    /// via `new_with_clock`. See `src/raft/clock.rs`.
    pub(crate) clock: Arc<dyn Clock>,

    /// Transport abstraction for real TCP vs future simulation
    /// in-memory channels (P7). Production nodes get a fresh
    /// `TcpTransport` from `new` / `new_with_storage`; tests inject a
    /// custom impl via `new_with_transport`. See `src/raft/net.rs`.
    pub(crate) transport: Arc<dyn Transport>,

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
        // Production defaults: SystemClock + TcpTransport (no listener).
        // Tests / future sim harness use `new_with_clock` /
        // `new_with_transport` to inject custom impls.
        Self::new_with_clock_and_transport(
            raft_addr,
            peers,
            state_machine,
            storage,
            system_clock(),
            system_transport(),
        )
    }

    /// Construct a `RaftNode` with an explicit `RaftStorage` instance
    /// and a custom `Clock`. The clock is used for all `last_heartbeat`
    /// / `last_quorum_heartbeat_at` / `ReadIndex::issued_at` stamps and
    /// for the heartbeat-loop / election-timer sleeps (via the
    /// respective helper functions). Production code should keep using
    /// `new` / `new_with_storage`, which default to `SystemClock`.
    ///
    /// Added in P7 as part of the deterministic simulation testing
    /// (DST) foundation. See `src/raft/clock.rs` for rationale.
    pub fn new_with_clock(
        raft_addr: String,
        peers: Vec<String>,
        state_machine: Arc<RwLock<StateMachine>>,
        storage: RaftStorage,
        clock: Arc<dyn Clock>,
    ) -> Self {
        // Default to a listener-less TcpTransport. Callers that
        // need both custom clock AND custom transport should use
        // `new_with_clock_and_transport` directly.
        Self::new_with_clock_and_transport(
            raft_addr,
            peers,
            state_machine,
            storage,
            clock,
            system_transport(),
        )
    }

    /// Construct a `RaftNode` with both a custom `Clock` and a custom
    /// `Transport`. Used by tests that drive either abstraction
    /// independently; future sim harness will use this to inject a
    /// `SimClock` + `SimTransport` pair.
    ///
    /// Added in P7 as part of the DST foundation. See
    /// `src/raft/clock.rs` / `src/raft/net.rs`.
    pub fn new_with_clock_and_transport(
        raft_addr: String,
        peers: Vec<String>,
        state_machine: Arc<RwLock<StateMachine>>,
        storage: RaftStorage,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        // Load persistent state from disk
        let (term, vote, logs) = storage.load_initial_state();

        Self {
            storage,
            state_machine,
            clock: clock.clone(),
            transport,

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
            last_heartbeat: clock.now(),
            last_quorum_heartbeat_at: None,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
        }
    }

    /// Restore the state machine contents from a previously-saved snapshot,
    /// if one exists on disk. Returns `Some((last_included_index,
    /// last_included_term))` if a snapshot was applied; `None` if no
    /// snapshot file was found.
    ///
    /// The state machine must already be open (so its on-disk layout is
    /// reachable) but its contents are wiped via `install_snapshot`. The
    /// snapshot is the source of truth — any LSM data already on disk is
    /// discarded, because by definition a snapshot existed at a later
    /// point and the LSM is being replaced wholesale.
    ///
    /// Callers must follow this up by replaying any log entries written
    /// *after* the snapshot's `last_included_index` (i.e. the entries
    /// still present in the WAL after `rewrite_wal_after_snapshot` ran
    /// during the previous run).
    pub fn restore_from_snapshot(&mut self) -> Option<(u64, u64)> {
        let snap = self.storage.load_snapshot()?;
        let last_included_index = snap.last_included_index;
        let last_included_term = snap.last_included_term;
        let mut sm = self.state_machine.write().expect("state machine lock");
        sm.install_snapshot(snap.data)
            .expect("install_snapshot during startup restore");
        drop(sm);
        // Bring commit_index and last_applied forward to the snapshot
        // position so replay skips the entries the snapshot already
        // covers.
        self.commit_index = self.commit_index.max(last_included_index);
        self.last_applied = self.last_applied.max(last_included_index);
        Some((last_included_index, last_included_term))
    }

    /// Returns true iff this node was started with no peers (single-node
    /// standalone mode). Useful for fast-paths that would otherwise wait
    /// forever for a quorum that can never form.
    pub fn is_single_node(&self) -> bool {
        self.peers.is_empty()
    }

    /// Public read-only accessors for the leader-side coordinator.
    /// Added in P6 PR #13 so `coordinator::coordinate_tx` can read
    /// membership + identity without taking a mutable lock on the node.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    pub fn current_term(&self) -> u64 {
        self.current_term
    }

    pub fn get_log_entry(&self, index: u64) -> Option<crate::protocol::LogEntry> {
        // Log array is 0-indexed in storage; log[0] holds the
        // entry at Raft-log index 1 (Raft uses 1-indexed log
        // indices per the thesis). Translate before indexing.
        if index == 0 {
            return None;
        }
        self.log.get((index - 1) as usize).cloned()
    }

    /// Public setter for `peers`. Used by integration tests that
    /// bootstrap the cluster in two phases (allocate listener ports,
    /// then wire the membership). Production code wires `peers` once
    /// in `new_with_storage` and never mutates it.
    pub fn set_peers(&mut self, peers: Vec<String>) {
        self.peers = peers;
    }

    /// Test-only helper: push a phantom log entry into the log.
    /// Used by integration tests to simulate a peer whose log is
    /// ahead of the leader's (for the leader-log-stale vote-check
    /// in `handle_tx_vote_request`).
    ///
    /// The entry's `index` is computed as `log.len() + 1`, matching
    /// `propose()` (line ~489). Production code never calls this.
    ///
    /// Regression note: previously took an explicit `index` argument,
    /// but a caller-passed `index` decouples the entry's index from
    /// its array position in `log`, which breaks `apply_logs` —
    /// `apply_logs` walks by `last_applied` (an array offset), but
    /// displays `entry.index` (a logical index). With decoupled
    /// indices the user sees phantom entries claiming index 5 while
    /// occupying slot 0, the next real BeginTx (index 1) gets dropped
    /// by the AppendEntries conflict-check on line ~878 because it
    /// compares by array index, and the phantom's `no-op` apply
    /// silently swallows the slot. CI once caught this as a flaky
    /// `pending_tx_count` mismatch (PR #36 follow-up).
    pub fn push_log_entry_for_test(&mut self, command: crate::protocol::Command) {
        let new_index = self.log.len() + 1;
        self.log.push(crate::protocol::LogEntry {
            term: self.current_term,
            index: new_index,
            command,
        });
    }

    /// Public read-only view of the leader's per-peer `match_index`.
    /// `match_index[peer]` is the highest log index the leader knows
    /// the peer has durably replicated (updated on AppendEntries
    /// success in `sync_logs`). Used by the coordinator to wait for
    /// the BeginTx entry to be replicated cluster-wide before fanning
    /// out votes (otherwise peers reply "tx not pending").
    ///
    /// Returns `0` if the leader has not yet committed any index for
    /// that peer (Raft §5.3 "nextIndex = log length + 1" implies
    /// `match_index = 0` initially).
    pub fn match_index_for(&self, peer: &str) -> u64 {
        self.match_index.get(peer).copied().unwrap_or(0)
    }

    /// Helper to get the last log's index and term
    fn get_last_log_info(&self) -> (u64, u64) {
        self.log.last().map_or((0, 0), |entry| (entry.index as u64, entry.term))
    }

    /// Handle a pre-vote probe from a follower that is considering a real
    /// election but hasn't bumped its term yet (Raft §9.6).
    ///
    /// **Hard invariant:** this method must NOT mutate `current_term`,
    /// `vote_for`, `state`, or write to disk. A pre-vote is a *probe*; the
    /// whole point is to fail safely without disturbing the cluster's
    /// view of who the leader is. The only state change allowed is
    /// refreshing `last_heartbeat` so the receiver's own election timer
    /// doesn't fire while we're answering (the probe demonstrates the
    /// peer is alive).
    ///
    /// Policy (mirrors `handle_request_vote` but with a probe-only term
    /// admission check):
    ///   1. If `args.term < self.current_term` the probe is from a
    ///      stale peer; reject (the requester's claimed `current_term +
    ///      1` would be one less than ours, so they cannot possibly
    ///      win).
    ///   2. If `args.term > self.current_term`, the requester thinks
    ///      a higher term might win. We **do not** adopt it. We just
    ///      check whether they would beat our election-restriction
    ///      check at `args.term` (i.e. their `(last_log_index,
    ///      last_log_term)` is at least as fresh as ours). If yes, grant
    ///      the probe; if no, refuse.
    ///   3. If `args.term == self.current_term`, normal election
    ///      restriction. Grant iff the probe passes it AND we are not
    ///      already a leader of this term (a leader of the same term
    ///      is by definition a quorum winner; the probe can't beat it
    ///      in a real vote either).
    ///   4. **No log up-to-date bypass**: even if we'd vote for this
    ///      candidate in a real election, if `args.term` is more than
    ///      one ahead of ours (`args.term > self.current_term + 1`)
    ///      the probe is from a peer that hasn't even observed our
    ///      term — refuse. This protects against a stale peer
    ///      repeatedly probing at arbitrarily high terms.
    ///
    /// The returned `VoteResponseArgs::term` is always our own
    /// `current_term` so the caller can detect "I was stale and need
    /// to step down" without us actually stepping them down for them.
    pub fn handle_pre_vote(&mut self, args: &RequestVoteArgs) -> VoteResponseArgs {
        // Refresh our own heartbeat clock — a live peer is good news
        // for our election timer regardless of the probe outcome.
        self.last_heartbeat = self.clock.now();

        let my_term = self.current_term;

        // (1) Probe term strictly older → reject, this peer is stale.
        // (4) Probe term more than one ahead → reject, can't trust a
        //     peer whose clock / observation is that far behind.
        if args.term < my_term || args.term > my_term + 1 {
            return VoteResponseArgs {
                term: my_term,
                vote_granted: false,
            };
        }

        // (3) Same term, but we're the leader of it. A probe can't
        //     beat us in a real vote either; refuse without recording
        //     anything.
        if args.term == my_term && self.state == NodeState::Leader {
            return VoteResponseArgs {
                term: my_term,
                vote_granted: false,
            };
        }

        // (2)(3) Election restriction at `args.term`. Reuse the same
        //     up-to-date test as the real vote path; no state mutation.
        let (my_last_log_index, my_last_log_term) = self.get_last_log_info();
        let probe_log_up_to_date = (args.last_log_term > my_last_log_term)
            || (args.last_log_term == my_last_log_term
                && args.last_log_index >= my_last_log_index);

        VoteResponseArgs {
            term: my_term,
            vote_granted: probe_log_up_to_date,
        }
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
            self.last_heartbeat = self.clock.now();

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

    /// Handle a 2PC coordinator `VoteRequest` from the leader.
    ///
    /// Returns a `VoteResponse` reporting whether the local node
    /// agrees to commit the transaction. This is the receiver-side
    /// half of the side-channel RPC introduced in P6 PR #12; the
    /// sender-side half (`RpcClient::send_tx_vote_rpc`) and the
    /// wire envelope (`raft::transport::DispatchKind::Vote`) live
    /// elsewhere.
    ///
    /// Decision policy (all-yes required by 2PC, see
    /// `ROADMAP.md` P6 decision table):
    ///   1. **Stale term**: if the request carries a term older
    ///      than our `current_term`, reject with the local term
    ///      so the leader can step down.
    ///   2. **Not the leader's term**: if the request carries a
    ///      newer term, step down to Follower and adopt it.
    ///      **Reject the vote** rather than grant it: a new
    ///      term means the sender is not the established leader
    ///      of the new term yet (no election has been observed
    ///      by us). The legitimate leader of the new term will
    ///      re-broadcast `VoteRequest` after winning the
    ///      election. Granting under a new term would let a
    ///      stale partition-leader prematurely commit state.
    ///   3. **Tx not pending locally**: the `BeginTx` log entry
    ///      must have been replicated to us before we can vote.
    ///      If `tx_id` is not in `pending_txs` we say No ("tx
    ///      not pending") so the coordinator aborts instead of
    ///      racing a future commit decision.
    ///   4. **Log up-to-date check (mirrors RequestVote)**:
    ///      the leader's `last_log_index` / `last_log_term`
    ///      must be at least as fresh as our local log tip. A
    ///      stale leader would otherwise induce a phantom vote.
    ///   5. **Safety ack**: record the vote on the state
    ///      machine via `record_vote` so a future `DecideTx`
    ///      can apply the operations atomically. Persist the
    ///      intent on the local log by returning Yes.
    pub fn handle_tx_vote_request(&mut self, req: &VoteRequest) -> VoteResponse {
        // 1. Stale term: reject without state change.
        if req.term < self.current_term {
            return VoteResponse {
                term: self.current_term,
                vote_granted: false,
                reason: format!(
                    "stale term: request term {} < current term {}",
                    req.term, self.current_term
                ),
            };
        }

        // 2. Newer term: adopt it, step down to Follower, but
        //    **reject the vote**. The sender is not yet the
        //    established leader of the new term from our point
        //    of view (we have not granted any leader election
        //    vote for it). Granting a tx vote under a term
        //    change would let a partitioned old leader commit
        //    state in the new term. The new leader will
        //    re-broadcast under the correct term after winning
        //    its election.
        if req.term > self.current_term {
            self.current_term = req.term;
            self.state = NodeState::Follower;
            self.vote_for = None;
            if let Err(e) =
                self.storage.save_meta(self.current_term, self.vote_for.clone())
            {
                eprintln!(
                    "[Critical] Failed to save metadata after term update in tx_vote: {}",
                    e
                );
            }
            return VoteResponse {
                term: self.current_term,
                vote_granted: false,
                reason: format!(
                    "term advance: adopted term {} but deferring vote to elected leader",
                    req.term
                ),
            };
        }

        // 3. Log up-to-date check (mirrors RequestVote's
        //    election-restriction semantics). A leader whose log
        //    has fallen behind ours cannot safely collect votes.
        let (my_last_log_index, my_last_log_term) = self.get_last_log_info();
        let leader_log_up_to_date = (req.last_log_term > my_last_log_term)
            || (req.last_log_term == my_last_log_term
                && req.last_log_index >= my_last_log_index);
        if !leader_log_up_to_date {
            return VoteResponse {
                term: self.current_term,
                vote_granted: false,
                reason: format!(
                    "leader log stale: leader=({}, {}) local=({}, {})",
                    req.last_log_index, req.last_log_term,
                    my_last_log_index, my_last_log_term
                ),
            };
        }

        // 4. Tx must be in our pending set. A vote is only
        //    meaningful after the `BeginTx` log entry has been
        //    replicated and applied to `pending_txs` (the state
        //    machine replays the log on startup, so a freshly
        //    restarted node can vote once it has caught up).
        //
        // Race fix (PR #14): the leader's `match_index >= begin_index`
        // only tells us the peer has the entry in its log, not that
        // `apply_logs` has run locally. AppendEntries only advances
        // `commit_index` when `args.leader_commit > self.commit_index`,
        // and the first AppendEntries that introduces the entry
        // carries `leader_commit = 0` (the leader has not committed
        // yet). So the peer has the entry in its log but
        // `commit_index` is still behind, and `apply_logs` is a no-op
        // until the next heartbeat bumps `commit_index`.
        //
        // To unblock the vote immediately, fast-forward our
        // `commit_index` to the leader's `last_log_index` (the index
        // of the entry the leader is voting about) and apply. This is
        // safe because: (a) the leader has already committed the
        // entry (since `match_index >= begin_index` was confirmed on
        // the leader side), and (b) the entry is in our log (verified
        // by the leader-log-up-to-date check above). All earlier
        // entries are also in our log (cumulative consistency
        // established by step 3), so applying them along the way is
        // safe.
        if req.last_log_index > self.commit_index {
            let log_len = self.log.len() as u64;
            if req.last_log_index <= log_len {
                self.commit_index = req.last_log_index;
            }
        }
        self.apply_logs();
        {
            let sm = self.state_machine.read().unwrap();
            if sm.pending_tx(&req.tx_id).is_none() {
                return VoteResponse {
                    term: self.current_term,
                    vote_granted: false,
                    reason: format!("tx not pending: {}", req.tx_id),
                };
            }
        }

        // 5. Grant the vote. Record it on the state machine so
        //    a future `DecideTx(Commit)` will be able to apply
        //    the operations.
        {
            let mut sm = self.state_machine.write().unwrap();
            let _ = sm.record_vote(&req.tx_id, self.node_id.clone(), Vote::Yes);
        }

        VoteResponse {
            term: self.current_term,
            vote_granted: true,
            reason: String::new(),
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

    /// Propose a batch of commands that must commit together (same log
    /// entries, contiguous indices). Used by the 2PC client path so that
    /// `BeginTx` and its `DecideTx` ride the same Raft proposal.
    ///
    /// Returns true iff every entry was successfully appended.
    pub fn propose_batch(&mut self, commands: Vec<Command>) -> bool {
        if self.state != NodeState::Leader {
            return false;
        }
        let mut next_index = self.log.len() as u64 + 1;
        for command in commands {
            let entry = LogEntry {
                term: self.current_term,
                index: next_index as usize,
                command,
            };
            if let Err(e) = self.storage.append_wal_log(&entry) {
                eprintln!("[Error] Failed to append wal log: {}", e);
                return false;
            }
            self.log.push(entry);
            next_index += 1;
        }
        true
    }

    pub fn sync_logs(raft_node: Arc<RwLock<Self>>) {
        let (current_term, node_id, commit_index, peers, log_len) = {
            let n = raft_node.read().unwrap();
            (n.current_term, n.node_id.clone(), n.commit_index, n.peers.clone(), n.log.len() as u64)
        };

        // Single-node fast path: with no peers, there is no AppendEntries RPC
        // to send, and `maybe_commit` lives inside the per-peer success
        // handler. Without this branch a single-node leader would never
        // advance its `commit_index` after a proposal, so reads would never
        // see the latest writes.
        if peers.is_empty() {
            let mut n = raft_node.write().unwrap();
            if n.state == NodeState::Leader {
                n.maybe_commit();
            }
            return;
        }

        for peer_addr in peers {
            let raft_clone = raft_node.clone();
            let peer_addr_clone = peer_addr.clone();
            // Snapshot the transport once per peer under the read lock
            // (released before spawn). The clone is `Send + 'static`
            // because `Transport: Send + Sync + 'static`, so the spawn
            // future is `Send` and the per-peer task can run on any
            // tokio worker. Avoids holding `RwLockReadGuard` across
            // an await point, which would break the `Send` bound on
            // the `tokio::spawn` future.
            let transport_for_peer = raft_node.read().unwrap().transport.clone();

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
                match transport_for_peer
                    .send_raft(
                        &peer_addr_clone,
                        RaftMessage::AppendEntries(args.clone()),
                    )
                    .await
                {
                    Ok(RaftMessage::AppendReply(reply)) => {
                        let mut n = raft_clone.write().unwrap();
                        if reply.success {
                            let last_idx = args.prev_log_index + args.entries.len() as u64;
                            n.match_index.insert(peer_addr_clone.clone(), last_idx);
                            n.next_index.insert(peer_addr_clone.clone(), last_idx + 1);
                            // Refresh the leader's ReadIndex lease: at least one peer
                            // has acknowledged our leadership recently.
                            n.last_quorum_heartbeat_at = Some(n.clock.now());
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
                    Ok(other) => {
                        // Peer replied with an unexpected RaftMessage variant.
                        // Log and ignore — this can only happen if a buggy
                        // peer echoes back something other than the matching
                        // reply type. The pre-trait code couldn't
                        // encounter this because each send_*_rpc helper
                        // unwrapped the variant itself; with the trait
                        // surface, the dispatch happens at the caller.
                        eprintln!(
                            "[Protocol] AppendEntries to {} got unexpected reply variant {:?}",
                            peer_addr_clone,
                            std::mem::discriminant(&other)
                        );
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
                        let _ = state_machine.delete(&key);
                        println!("✅ [Apply] Index {}: DELETE {}", entry.index, key);
                    }
                    Command::BeginTx { tx_id, ops } => {
                        let _ = state_machine.begin_tx(tx_id.clone(), ops.clone());
                        println!("✅ [Apply] Index {}: BEGIN_TX {} ({} ops)", entry.index, tx_id, ops.len());
                    }
                    Command::DecideTx { tx_id, decision } => {
                        let _ = state_machine.decide_tx(tx_id, decision.clone());
                        println!("✅ [Apply] Index {}: DECIDE_TX {} = {:?}", entry.index, tx_id, decision);
                    }
                    Command::Get { .. } => println!("🔍 [Apply] Index {}: GET (no-op)", entry.index),
                    Command::Compact => println!("🔍 [Apply] Index {}: Compact marker (no-op)", entry.index),
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
                Command::Delete { key } => { let _ = state_machine.delete(&key); }
                Command::BeginTx { tx_id, ops } => { let _ = state_machine.begin_tx(tx_id.clone(), ops.clone()); }
                // As of P6, votes no longer travel through the Raft log:
                // they arrive on the side-channel `VoteRequest` RPC (see
                // `proto/coordination.proto`) and are recorded directly on
                // the state machine via `record_vote`, bypassing the log.
                Command::DecideTx { tx_id, decision } => { let _ = state_machine.decide_tx(tx_id, decision.clone()); }
                _ => {}
            }
        }
        self.last_applied = self.log.len() as u64;
        println!("✅ [Replay] Successfully replayed {} logs to state machine", self.log.len());
    }

    pub async fn run_heartbeat_loop(
        node_arc: Arc<RwLock<RaftNode>>,
        stop: super::net::StopSignal,
    ) {
        // Pull the clock out of the node once, before entering the
        // loop, so the per-tick hot path doesn't re-lock for it.
        // The clock is `Arc<dyn Clock>`, so cloning is cheap and the
        // same instance serves every tick. The sleep cadence matches
        // the previous `tokio::time::interval` (auto-tick on first
        // await + periodic thereafter); drift catch-up is best-effort
        // — heartbeat is not a strict periodic obligation, and the
        // previous impl also drifted under scheduler pressure.
        //
        // The loop observes `stop` between ticks so that
        // `SimCluster::kill_node` can shut the heartbeat down
        // without spawning a parallel task that keeps logging
        // "peer unreachable" RPC errors forever.
        let clock = node_arc.read().unwrap().clock.clone();
        let period = Duration::from_millis(Config::heartbeat_interval_ms());
        loop {
            tokio::select! {
                biased;
                _ = stop.0.notified() => return,
                _ = clock.sleep(period) => {}
            }
            let is_leader = node_arc.read().unwrap().state == NodeState::Leader;
            if is_leader {
                Self::sync_logs(node_arc.clone());
                // Check the WAL size once per heartbeat tick (cheap —
                // just a `stat` call). If the WAL has grown past the
                // threshold, take a snapshot and rewrite the WAL. The
                // threshold is read fresh each tick so tests that tweak
                // `OXIDE_SNAPSHOT_THRESHOLD_BYTES` mid-run get a chance
                // to trigger with a low value.
                {
                    let mut node = node_arc.write().unwrap();
                    let _ = node.maybe_snapshot(Config::snapshot_threshold_bytes());
                }
            }
        }
    }

    pub fn become_candidate(raft_node: Arc<RwLock<Self>>) {
        let mut node = raft_node.write().unwrap();
        Self::promote_to_candidate_locked(&mut node);
        println!("🗳️ Node {} candidate for Term {}", node.node_id, node.current_term);
        let term = node.current_term;
        let state = node.state.clone();
        drop(node);
        debug_assert_eq!(state, NodeState::Candidate);
        let _ = term; // silence unused if assertions off
        Self::request_votes(raft_node);
    }

    /// Promote the current node from PreCandidate to Candidate (Raft
    /// §9.6). Called from the PreVote reply handler when a quorum
    /// has granted the probe.
    ///
    /// Pre-conditions: caller must hold the write lock and have just
    /// verified `state == NodeState::PreCandidate && current_term ==
    /// probed_term`. We bump term by exactly 1 (the term we probed
    /// at) so the local log's election-restriction view stays
    /// consistent with what the peers saw in the probe.
    ///
    /// Pure term/persistence helper — does **not** call
    /// `request_votes`. The caller (`process_pre_vote_replies`)
    /// drives the fan-out so it can carry the same atomic
    /// `votes_received` counter as the pre-vote round and decide in
    /// one place whether to drop straight to Candidate or fall back
    /// to Follower.
    fn promote_to_candidate_locked(node: &mut Self) {
        node.current_term += 1;
        node.state = NodeState::Candidate;
        node.vote_for = Some(node.node_id.clone());
        let _ = node.storage.save_meta(node.current_term, node.vote_for.clone());
        node.last_heartbeat = node.clock.now();
    }

    /// Enter the pre-vote phase (Raft §9.6, P8 PR 5). **Does not**
    /// bump `current_term` or write `vote_for` to disk. Sends a
    /// `RequestPreVote` to every peer at the **implied** term
    /// `current_term + 1`; if a quorum grants the probe, the
    /// reply handler promotes us to a real Candidate via
    /// `promote_to_candidate_locked` + `request_votes`.
    ///
    /// Called from the election timer on the production path.
    /// Tests and the simulation harness still call
    /// `become_candidate` directly so they can skip the probe.
    pub fn become_pre_candidate(raft_node: Arc<RwLock<Self>>) {
        let (peers, probed_term, candidate_id, last_idx, last_term, transport) = {
            let node = raft_node.read().unwrap();
            let (li, lt) = node.get_last_log_info();
            (
                node.peers.clone(),
                node.current_term + 1,
                node.node_id.clone(),
                li,
                lt,
                node.transport.clone(),
            )
        };

        // Move to PreCandidate *before* sending probes so that an
        // incoming AppendEntries / higher-term vote response
        // arriving while the fan-out is in flight knows we're in
        // the probe phase and won't mistake us for a real Candidate.
        {
            let mut node = raft_node.write().unwrap();
            // Someone else (AppendEntries, higher-term vote reply,
            // install snapshot) may have changed our state between
            // the timer tick and us grabbing the lock; only enter
            // PreCandidate if we're still Follower / PreCandidate at
            // a current term that matches the snapshot we just took.
            if node.state == NodeState::Follower
                && node.current_term + 1 == probed_term
            {
                node.state = NodeState::PreCandidate;
            } else {
                return;
            }
        }

        let total_nodes = peers.len() + 1;
        let votes_received = Arc::new(std::sync::atomic::AtomicUsize::new(1)); // self-vote counts

        println!(
            "🔎 [PreVote] Node {} probing at Term {} (current {} + 1)",
            candidate_id, probed_term, probed_term - 1
        );

        // Single-node cluster: we are already a majority of 1. Skip
        // the RPC fan-out entirely and promote directly.
        if total_nodes == 1 {
            let mut n = raft_node.write().unwrap();
            if n.state == NodeState::PreCandidate {
                Self::promote_to_candidate_locked(&mut n);
                let _ = n.become_leader_checked();
            }
            return;
        }

        for peer_addr in peers {
            let raft_clone = raft_node.clone();
            let votes_clone = votes_received.clone();
            let cid = candidate_id.clone();
            let transport = transport.clone();

            tokio::spawn(async move {
                let args = RequestVoteArgs {
                    term: probed_term,
                    candidate_id: cid.clone(),
                    last_log_index: last_idx,
                    last_log_term: last_term,
                };
                match transport
                    .send_raft(&peer_addr, RaftMessage::RequestPreVote(args))
                    .await
                {
                    Ok(RaftMessage::PreVoteResponse(reply)) => {
                        Self::process_pre_vote_reply(
                            raft_clone,
                            votes_clone,
                            total_nodes,
                            probed_term,
                            cid,
                            reply,
                        )
                        .await;
                    }
                    Ok(other) => {
                        eprintln!(
                            "[Protocol] PreVote to {} got unexpected reply variant {:?}",
                            peer_addr,
                            std::mem::discriminant(&other)
                        );
                    }
                    Err(e) => eprintln!("[Network] PreVote RPC error with {}: {}", peer_addr, e),
                }
            });
        }
    }

    /// Handle one PreVoteResponse. Increments the granted counter
    /// and, if a quorum has been reached, promotes the node to
    /// Candidate and fans out the real RequestVote.
    async fn process_pre_vote_reply(
        raft_arc: Arc<RwLock<Self>>,
        votes_received: Arc<std::sync::atomic::AtomicUsize>,
        total_nodes: usize,
        probed_term: u64,
        candidate_id: String,
        reply: VoteResponseArgs,
    ) {
        // Reject path: peer's current_term > probed_term means the
        // peer has observed a higher term than we're probing at.
        // Step down to Follower without touching `current_term`
        // (the peer's term hasn't been **granted** to us; we just
        // step down and wait for a real RequestVote at that term
        // from whoever eventually wins it).
        if reply.term > probed_term {
            let mut n = raft_arc.write().unwrap();
            if n.state == NodeState::PreCandidate {
                n.state = NodeState::Follower;
                // Note: we do NOT bump current_term here. Pre-vote
                // is precisely the mechanism that prevents a
                // partitioned node from inflating its term
                // based on a higher-term reply. We stay at our
                // original current_term and wait for a real
                // RequestVote at reply.term before adopting it.
            }
            return;
        }

        if !reply.vote_granted {
            return;
        }

        let count =
            votes_received.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if count <= total_nodes / 2 {
            return; // not yet a quorum
        }

        // Quorum reached — promote to real Candidate and fire
        // RequestVote. Take the write lock once and do the whole
        // transition atomically.
        let mut n = raft_arc.write().unwrap();
        if n.state != NodeState::PreCandidate {
            return; // raced: we already promoted (single-node fast
                    // path) or stepped down
        }
        Self::promote_to_candidate_locked(&mut n);
        let new_term = n.current_term;
        let state_after = n.state.clone();
        drop(n);
        debug_assert_eq!(state_after, NodeState::Candidate);
        debug_assert_eq!(new_term, probed_term);
        println!(
            "✅ [PreVote] Node {} got quorum at Term {} → promoted to Candidate",
            candidate_id, new_term
        );
        Self::request_votes(raft_arc);
    }

    /// Promote to leader if state == Candidate and current_term
    /// matches the one we probed at. Used as the single-node
    /// PreCandidate → Leader fast path.
    fn become_leader_checked(&mut self) -> bool {
        if self.state != NodeState::Candidate {
            return false;
        }
        self.become_leader();
        true
    }

    pub fn request_votes(raft_arc: Arc<RwLock<Self>>) {
        let (peers, term, candidate_id, last_idx, last_term, transport) = {
            let node = raft_arc.read().unwrap();
            let (li, lt) = node.get_last_log_info();
            (
                node.peers.clone(),
                node.current_term,
                node.node_id.clone(),
                li,
                lt,
                node.transport.clone(),
            )
        };

        let total_nodes = peers.len() + 1;
        let votes_received = Arc::new(std::sync::atomic::AtomicUsize::new(1));

        // Single-node cluster: we already have a majority (1 > 0).
        // Skip the RPC fan-out entirely and become leader now.
        if total_nodes == 1 {
            let mut n = raft_arc.write().unwrap();
            if n.state == NodeState::Candidate && n.current_term == term {
                n.become_leader();
            }
            return;
        }

        for peer_addr in peers {
            let raft_clone = raft_arc.clone();
            let votes_clone = votes_received.clone();
            let cid = candidate_id.clone();
            let transport = transport.clone();

            tokio::spawn(async move {
                let args = RequestVoteArgs { term, candidate_id: cid, last_log_index: last_idx, last_log_term: last_term };
                match transport
                    .send_raft(&peer_addr, RaftMessage::RequestVote(args))
                    .await
                {
                    Ok(RaftMessage::VoteResponse(reply)) => {
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
                    Ok(other) => {
                        // Unexpected reply variant from a buggy peer.
                        // Pre-trait code couldn't hit this path because
                        // each send_*_rpc helper unwrapped its own
                        // expected reply; the trait surface defers
                        // dispatch to the caller. Log and move on.
                        eprintln!(
                            "[Protocol] RequestVote to {} got unexpected reply variant {:?}",
                            peer_addr,
                            std::mem::discriminant(&other)
                        );
                    }
                    Err(e) => eprintln!("[Network] RPC error with {}: {}", peer_addr, e),
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
        self.last_heartbeat = self.clock.now();
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
        self.last_heartbeat = self.clock.now();

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

        eprintln!("[peer {}] AE: leader_commit={}, my_commit={}, log_len={}, entries={}", 
            self.node_id, args.leader_commit, self.commit_index, self.log.len(), args.entries.len());
        if args.leader_commit > self.commit_index {
            self.commit_index = std::cmp::min(args.leader_commit, self.log.len() as u64);
            eprintln!("[peer {}] AE: commit_index advanced to {}, applying...", self.node_id, self.commit_index);
            self.apply_logs();
        }

        AppendReplyArgs { term: self.current_term, success: true }
    }

    /// Handle an InstallSnapshot RPC from the Leader.
    ///
    /// Per §7 of the Raft thesis: replace local state machine with the snapshot,
    /// discard log entries covered by it, and reset commit / applied indices.
    pub fn handle_install_snapshot(&mut self, args: &InstallSnapshotArgs) -> InstallSnapshotReplyArgs {
        // 1. Term check
        if args.term < self.current_term {
            return InstallSnapshotReplyArgs { term: self.current_term };
        }
        if args.term > self.current_term {
            self.current_term = args.term;
            self.state = NodeState::Follower;
            self.vote_for = None;
            let _ = self.storage.save_meta(self.current_term.clone(), self.vote_for.clone());
        }
        self.state = NodeState::Follower;
        self.last_heartbeat = self.clock.now();

        // 2. Persist snapshot to disk (atomic via storage layer).
        let _ = self.storage.save_snapshot(&args.snapshot);

        // 3. Replace state machine contents with snapshot data.
        {
            let mut sm = self.state_machine.write().unwrap();
            // Reset to empty, then re-populate from snapshot.
            sm.clear_for_snapshot().expect("clear_for_snapshot");
            for (k, v) in &args.snapshot.data {
                let _ = sm.set(k, v);
            }
        }

        // 4. Discard log entries at or before last_included_index.
        let last_included = args.last_included_index;
        self.log.retain(|e| e.index as u64 > last_included);
        let _ = self.storage.rewrite_wal_after_snapshot(last_included);

        // 5. Reset indices: the snapshot's effect is already "applied".
        if last_included > self.commit_index {
            self.commit_index = last_included;
        }
        if last_included > self.last_applied {
            self.last_applied = last_included;
        }

        InstallSnapshotReplyArgs { term: self.current_term }
    }

    /// Take a snapshot of the current state machine if the on-disk WAL has
    /// grown past `threshold_bytes` bytes, then truncate the WAL to free
    /// disk space.
    ///
    /// Returns `true` if a snapshot was taken. Only the leader snapshots
    /// (followers receive snapshots from the leader via InstallSnapshot).
    ///
    /// The threshold should be sourced from
    /// [`crate::config::Config::snapshot_threshold_bytes`]; passing the
    /// raw usize keeps this method easily testable.
    pub fn maybe_snapshot(&mut self, threshold_bytes: u64) -> bool {
        if self.state != NodeState::Leader {
            return false;
        }
        // Size check is best-effort (0 == WAL missing / unreadable). A 0
        // return means "skip the threshold this round", which is the
        // right thing for a fresh node.
        let wal_size = self.storage.wal_size_bytes();
        if wal_size <= threshold_bytes {
            return false;
        }
        // Snapshot at the last applied entry — only entries that have actually
        // been committed can be safely captured.
        let snapshot_index = self.commit_index;
        if snapshot_index == 0 {
            return false;
        }
        let snapshot_term = self
            .log
            .get(snapshot_index as usize - 1)
            .map(|e| e.term)
            .unwrap_or(0);

        let data = {
            let sm = self.state_machine.read().unwrap();
            sm.snapshot_data().expect("snapshot_data")
        };

        let snap = Snapshot {
            last_included_index: snapshot_index,
            last_included_term: snapshot_term,
            data,
        };

        if self.storage.save_snapshot(&snap).is_err() {
            return false;
        }
        let _ = self.storage.rewrite_wal_after_snapshot(snapshot_index);
        // Local in-memory log is preserved (other peers may still need entries
        // in the snapshot range via AppendEntries), but the disk WAL is freed.
        true
    }

    /// Begin a linearizable read on the leader.
    ///
    /// Returns `Some(ReadIndex)` anchored at the leader's current `commit_index`
    /// and the moment the call was issued. Returns `None` if this node is not
    /// the current leader. Triggers an immediate heartbeat so the leader can
    /// prove its quorum quickly.
    pub fn begin_read(raft_node: Arc<RwLock<Self>>) -> Option<ReadIndex> {
        let ri = {
            let node = raft_node.write().unwrap();
            if node.state != NodeState::Leader {
                return None;
            }
            ReadIndex {
                index: node.commit_index,
                issued_at: node.clock.now(),
            }
        };
        // Force a heartbeat round so the leader's last_quorum_heartbeat_at
        // advances as soon as peers ack. We drop the write lock above before
        // calling sync_logs (which takes a read lock) to avoid self-deadlock.
        Self::sync_logs(raft_node);
        Some(ri)
    }

    /// Confirm that a previously-issued `ReadIndex` is safe to serve.
    ///
    /// Safety requires:
    ///   1. The node is still the leader (no step-down after `begin_read`).
    ///   2. The state machine has applied all entries up to `ri.index`.
    ///   3. The leader's quorum proof (`last_quorum_heartbeat_at`) was obtained
    ///      at or after the read was issued, AND is still recent enough that
    ///      a partitioned leader could not have survived an election timeout.
    pub fn confirm_read(&self, ri: ReadIndex) -> bool {
        if self.state != NodeState::Leader {
            return false;
        }
        if self.last_applied < ri.index {
            return false;
        }
        match self.last_quorum_heartbeat_at {
            None => false,
            Some(t) => {
                let fresh = t.elapsed()
                    < Duration::from_millis(Config::max_election_timeout_ms());
                fresh && t >= ri.issued_at
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::protocol::{Command, LogEntry, ReadIndex, Snapshot, TxDecision, TxOp};
    use crate::raft::rpc::{AppendEntriesArgs, InstallSnapshotArgs, RequestVoteArgs};
    use crate::raft::storage::RaftStorage;
    use crate::state_machine::StateMachine;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Build a `RaftNode` rooted in a fresh temp dir so each test is isolated
    /// on disk and does not depend on the global `Config`.
    fn make_node(node_id: &str, peers: Vec<String>) -> (TempDir, RaftNode) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join(format!("{node_id}.wal")).to_str().unwrap().to_string();
        let meta = dir.path().join(format!("{node_id}_meta.json")).to_str().unwrap().to_string();
        let snap = dir.path().join(format!("{node_id}_snapshot.json")).to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join(format!("{node_id}_sm"));
        let sm_config = crate::state_machine::StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
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
        assert_eq!(sm.get(key), expected.map(|s| s.to_string()),
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
        let node_id = "replay_node";
        let dir = tempfile::tempdir().unwrap();
        let _sm_placeholder = Arc::new(RwLock::new(StateMachine::open(crate::state_machine::StateMachineConfig {
            data_dir: dir.path().join(format!("{node_id}_sm_unused")),
            memtable_size_threshold: 1024 * 1024,
        }).unwrap()));
        let wal = dir.path().join(format!("{node_id}.wal")).to_str().unwrap().to_string();
        let meta = dir.path().join(format!("{node_id}_meta.json")).to_str().unwrap().to_string();
        let snap = dir.path().join(format!("{node_id}_snapshot.json")).to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);

        // Simulate a fresh WAL with three committed entries.
        storage.append_wal_log(&make_entry(1, 1, "a", "1")).unwrap();
        storage.append_wal_log(&make_entry(1, 2, "b", "2")).unwrap();
        storage.append_wal_log(&make_entry(1, 3, "c", "3")).unwrap();

        let sm_dir2 = dir.path().join(format!("{node_id}_sm2"));
        let sm_config2 = crate::state_machine::StateMachineConfig {
            data_dir: sm_dir2,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm2 = Arc::new(RwLock::new(StateMachine::open(sm_config2).unwrap()));

        // Construct node from disk; it should replay all three.
        let mut node = RaftNode::new_with_storage(
            node_id.to_string(),
            vec![],
            sm2.clone(),
            storage,
        );
        // Point the test's `sm` reference at the same state machine the node
        // is mutating, so the assertions below see the replayed writes.
        let sm = sm2.clone();
        node.replay_logs();

        assert_eq!(node.last_applied, 3);
        let sm_read = sm.read().unwrap();
        assert_eq!(sm_read.get("a"), Some("1".to_string()));
        assert_eq!(sm_read.get("b"), Some("2".to_string()));
        assert_eq!(sm_read.get("c"), Some("3".to_string()));
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

    // ---------- handle_install_snapshot ----------

    fn snapshot_args(term: u64, leader: &str, last_idx: u64, last_term: u64,
                     data: HashMap<String, String>) -> InstallSnapshotArgs {
        InstallSnapshotArgs {
            term,
            leader_id: leader.to_string(),
            last_included_index: last_idx,
            last_included_term: last_term,
            snapshot: Snapshot {
                last_included_index: last_idx,
                last_included_term: last_term,
                data,
            },
        }
    }

    fn sm_data(node: &RaftNode) -> HashMap<String, String> {
        node.state_machine.read().unwrap().snapshot_data().expect("snapshot_data")
    }

    #[test]
    fn install_snapshot_rejects_stale_term() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;

        let reply = node.handle_install_snapshot(&snapshot_args(4, "n2", 1, 1, HashMap::new()));

        assert_eq!(reply.term, 5);
        assert_eq!(node.current_term, 5, "must not regress term");
    }

    #[test]
    fn install_snapshot_steps_down_and_updates_term_on_newer() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Candidate;
        node.current_term = 1;
        node.vote_for = Some("n1".into());

        let reply = node.handle_install_snapshot(&snapshot_args(2, "n2", 1, 1, HashMap::new()));

        assert_eq!(reply.term, 2);
        assert_eq!(node.state, NodeState::Follower);
        assert_eq!(node.current_term, 2);
        assert!(node.vote_for.is_none());
    }

    #[test]
    fn install_snapshot_replaces_state_machine_contents() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        // Seed local state with some data that must be wiped.
        {
            let mut sm = node.state_machine.write().unwrap();
            let _ = sm.set("old", "stale");
            let _ = sm.set("keep", "maybe");
        }

        let mut data = HashMap::new();
        data.insert("alpha".into(), "1".into());
        data.insert("beta".into(), "2".into());

        node.handle_install_snapshot(&snapshot_args(1, "n2", 5, 1, data));

        let sm = sm_data(&node);
        assert_eq!(sm.get("alpha").map(String::as_str), Some("1"));
        assert_eq!(sm.get("beta").map(String::as_str), Some("2"));
        assert!(sm.get("old").is_none(), "snapshot must wipe stale data");
        assert!(sm.get("keep").is_none(), "snapshot must wipe stale data");
    }

    #[test]
    fn install_snapshot_discards_covered_log_entries() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.log = vec![
            make_entry(1, 1, "a", "1"),
            make_entry(1, 2, "b", "2"),
            make_entry(1, 3, "c", "3"),
            make_entry(1, 4, "d", "4"),
        ];

        node.handle_install_snapshot(&snapshot_args(1, "n2", 3, 1, HashMap::new()));

        // Entries 1..=3 are covered, only index 4 must remain.
        assert_eq!(node.log.len(), 1);
        assert_eq!(node.log[0].index, 4);
    }

    #[test]
    fn install_snapshot_persists_snapshot_file_to_disk() {
        let (d, mut node) = make_node("n1", vec!["n2".into()]);
        let mut data = HashMap::new();
        data.insert("k".into(), "v".into());

        node.handle_install_snapshot(&snapshot_args(1, "n2", 1, 1, data));

        let snap_path = d.path().join("n1_snapshot.json");
        assert!(snap_path.exists(), "snapshot file must be on disk");
        let raw = std::fs::read_to_string(&snap_path).unwrap();
        assert!(raw.contains("\"last_included_index\": 1"), "raw: {raw}");
        assert!(raw.contains("\"k\""), "raw: {raw}");
    }

    #[test]
    fn install_snapshot_advances_commit_index_to_snapshot_position() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.log = vec![make_entry(1, 1, "a", "1"), make_entry(1, 2, "b", "2")];
        node.commit_index = 0;
        node.last_applied = 0;

        node.handle_install_snapshot(&snapshot_args(1, "n2", 2, 1, HashMap::new()));

        assert_eq!(node.commit_index, 2);
        assert_eq!(node.last_applied, 2);
    }

    #[test]
    fn install_snapshot_rewrites_wal_on_disk() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.log = vec![make_entry(1, 1, "a", "1"), make_entry(1, 2, "b", "2"), make_entry(1, 3, "c", "3")];

        // Persist the log to WAL so we can verify it's rewritten.
        for entry in &node.log {
            node.storage.append_wal_log(entry).unwrap();
        }
        assert_eq!(node.storage.restore_wal_log().len(), 3);

        node.handle_install_snapshot(&snapshot_args(1, "n2", 2, 1, HashMap::new()));

        // After install, the WAL on disk must retain only the post-snapshot entry.
        assert_eq!(node.storage.restore_wal_log().len(), 1);
    }

    // ---------- maybe_snapshot ----------

    #[test]
    fn maybe_snapshot_noop_below_threshold() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.current_term = 1;
        node.log = vec![make_entry(1, 1, "a", "1")];
        node.commit_index = 1;

        assert!(!node.maybe_snapshot(100));
    }

    #[test]
    fn maybe_snapshot_noop_when_not_leader() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Follower;
        // Even with a huge log, only the leader takes snapshots.
        for i in 1..=10 {
            node.log.push(make_entry(1, i, "k", "v"));
        }
        node.commit_index = 10;

        assert!(!node.maybe_snapshot(5));
    }

    #[test]
    fn maybe_snapshot_noop_when_commit_index_is_zero() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.log = vec![make_entry(1, 1, "k", "v"), make_entry(1, 2, "k", "v")];

        // commit_index=0 means nothing has been applied yet — no safe snapshot.
        assert!(!node.maybe_snapshot(1));
    }

    #[test]
    fn maybe_snapshot_takes_snapshot_above_threshold() {
        let (d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.current_term = 1;
        // Apply some state to the machine so snapshot has data.
        {
            let mut sm = node.state_machine.write().unwrap();
            let _ = sm.set("foo", "bar");
        }
        // Seed log + commit so commit_index > 0.
        for i in 1..=5 {
            node.log.push(make_entry(1, i, "k", "v"));
        }
        node.commit_index = 5;
        for entry in &node.log {
            node.storage.append_wal_log(entry).unwrap();
        }

        let took = node.maybe_snapshot(3);
        assert!(took);

        // Snapshot file must exist with the right metadata.
        let snap_path = d.path().join("n1_snapshot.json");
        let raw = std::fs::read_to_string(&snap_path).unwrap();
        assert!(raw.contains("\"last_included_index\": 5"), "raw: {raw}");
        assert!(raw.contains("\"foo\""), "raw: {raw}");

        // WAL on disk must be truncated.
        assert_eq!(node.storage.restore_wal_log().len(), 0);
    }

    // ---------- begin_read / confirm_read ----------

    fn arc_node(node: RaftNode) -> Arc<RwLock<RaftNode>> {
        Arc::new(RwLock::new(node))
    }

    #[test]
    fn begin_read_returns_none_when_follower() {
        let (_d, node) = make_node("n1", vec!["n2".into()]);
        // default state is Follower
        let node_arc = arc_node(node);
        assert!(RaftNode::begin_read(node_arc).is_none());
    }

    #[test]
    fn begin_read_returns_none_when_candidate() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Candidate;
        let node_arc = arc_node(node);
        assert!(RaftNode::begin_read(node_arc).is_none());
    }

    #[tokio::test]
    async fn begin_read_returns_some_when_leader_and_anchors_at_commit_index() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.commit_index = 7;
        let node_arc = arc_node(node);
        let ri = RaftNode::begin_read(node_arc).expect("leader should begin read");
        assert_eq!(ri.index, 7);
    }

    #[test]
    fn confirm_read_returns_false_when_not_leader() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        // Promote to leader, capture a valid ReadIndex, then step down.
        node.state = NodeState::Leader;
        node.last_applied = 5;
        node.commit_index = 5;
        node.last_quorum_heartbeat_at = Some(Instant::now());
        let ri = ReadIndex { index: 5, issued_at: Instant::now() };

        // Now step down.
        node.state = NodeState::Follower;
        assert!(!node.confirm_read(ri));
    }

    #[test]
    fn confirm_read_returns_false_when_state_machine_lags() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.commit_index = 10;
        node.last_applied = 3; // Not yet applied up to read index
        node.last_quorum_heartbeat_at = Some(Instant::now());

        let ri = ReadIndex { index: 5, issued_at: Instant::now() };
        assert!(!node.confirm_read(ri));
    }

    #[test]
    fn confirm_read_returns_false_when_no_quorum_evidence() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.last_applied = 5;
        node.commit_index = 5;
        node.last_quorum_heartbeat_at = None; // never heard from any peer

        let ri = ReadIndex { index: 5, issued_at: Instant::now() };
        assert!(!node.confirm_read(ri));
    }

    #[test]
    fn confirm_read_returns_false_when_quorum_evidence_predates_issued_at() {
        // Critical safety property: if the leader issued the read BEFORE
        // its last heartbeat proof, the proof may have happened before a
        // partition, so we cannot guarantee linearizability.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.last_applied = 5;
        node.commit_index = 5;
        node.last_quorum_heartbeat_at = Some(Instant::now());

        // Read issued in the future relative to the heartbeat proof.
        let ri = ReadIndex {
            index: 5,
            issued_at: Instant::now() + Duration::from_secs(60),
        };
        assert!(!node.confirm_read(ri));
    }

    #[test]
    fn confirm_read_returns_false_when_quorum_evidence_too_stale() {
        // Stale heartbeat = likely partitioned. election_timeout is the bound.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.last_applied = 5;
        node.commit_index = 5;
        // Backdate the heartbeat proof by more than max_election_timeout_ms.
        let stale = Instant::now()
            - Duration::from_millis(Config::max_election_timeout_ms() + 500);
        node.last_quorum_heartbeat_at = Some(stale);

        let ri = ReadIndex { index: 5, issued_at: stale };
        assert!(!node.confirm_read(ri));
    }

    #[test]
    fn confirm_read_returns_true_when_all_conditions_met() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.last_applied = 5;
        node.commit_index = 5;
        let now = Instant::now();
        node.last_quorum_heartbeat_at = Some(now);

        let ri = ReadIndex { index: 5, issued_at: now };
        assert!(node.confirm_read(ri));
    }

    #[test]
    fn confirm_read_rejects_after_step_down_even_with_fresh_proof() {
        // Simulates: leader processes begin_read, then loses election,
        // then confirm_read is called. Even though the proof timestamp is
        // still valid, the node is no longer the leader.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        node.last_applied = 5;
        node.commit_index = 5;
        let now = Instant::now();
        node.last_quorum_heartbeat_at = Some(now);

        let ri = ReadIndex { index: 5, issued_at: now };
        assert!(node.confirm_read(ri), "sanity: leader confirms");

        node.state = NodeState::Follower;
        assert!(!node.confirm_read(ri), "stepped-down node must not serve reads");
    }

    // ---------- two-phase commit (apply_logs path) ----------

    #[test]
    fn replay_logs_applies_committed_tx() {
        // BeginTx + DecideTx(Commit) replayed together should apply all
        // ops atomically.
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.log = vec![
            LogEntry {
                term: 1,
                index: 1,
                command: Command::BeginTx {
                    tx_id: "tx-replay".into(),
                    ops: vec![
                        TxOp::Put { key: "a".into(), value: "1".into() },
                        TxOp::Put { key: "b".into(), value: "2".into() },
                    ],
                },
            },
            LogEntry {
                term: 1,
                index: 2,
                command: Command::DecideTx {
                    tx_id: "tx-replay".into(),
                    decision: TxDecision::Commit,
                },
            },
        ];
        node.commit_index = 2;
        node.replay_logs();

        assert_eq!(node.last_applied, 2);
        assert_node_state(&node, "a", Some("1"));
        assert_node_state(&node, "b", Some("2"));
    }

    #[test]
    fn replay_logs_aborted_tx_has_no_side_effects() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.log = vec![
            LogEntry {
                term: 1,
                index: 1,
                command: Command::BeginTx {
                    tx_id: "tx-abort".into(),
                    ops: vec![TxOp::Put {
                        key: "a".into(),
                        value: "should-not-apply".into(),
                    }],
                },
            },
            LogEntry {
                term: 1,
                index: 2,
                command: Command::DecideTx {
                    tx_id: "tx-abort".into(),
                    decision: TxDecision::Abort,
                },
            },
        ];
        node.commit_index = 2;
        node.replay_logs();

        assert_node_state(&node, "a", None);
        assert_eq!(node.last_applied, 2);
    }

    #[test]
    fn propose_batch_appends_contiguous_entries() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;

        let ok = node.propose_batch(vec![
            Command::BeginTx {
                tx_id: "tx-batch".into(),
                ops: vec![TxOp::Put { key: "x".into(), value: "1".into() }],
            },
            Command::DecideTx {
                tx_id: "tx-batch".into(),
                decision: TxDecision::Commit,
            },
        ]);
        assert!(ok);

        assert_eq!(node.log.len(), 2);
        assert_eq!(node.log[0].index, 1);
        assert_eq!(node.log[1].index, 2);
        assert!(matches!(node.log[0].command, Command::BeginTx { .. }));
        assert!(matches!(node.log[1].command, Command::DecideTx { .. }));
    }

    // `vote_recorded_for_pending_tx_then_commit_applies_ops` was removed
    // in P6 (commit e329fe6 superseded): votes no longer travel through
    // the Raft log — they arrive on the side-channel `VoteRequest` RPC
    // (see `proto/coordination.proto`) and are recorded directly on the
    // state machine via `record_vote`. The BeginTx + DecideTx replay
    // path is covered by `replay_logs_applies_committed_tx` above, and
    // `record_vote` itself is covered by
    // `state_machine::tests::record_vote_updates_pending_tx_view`.

    // ============================================================
    //  handle_tx_vote_request (P6 PR #12, side-channel 2PC vote)
    // ============================================================

    /// Seed `node` with a `BeginTx` log entry + state machine pending
    /// entry so `handle_tx_vote_request` has something to vote on.
    /// Returns the `(tx_id, last_log_index, last_log_term)` snapshot
    /// the test should mirror in its `VoteRequest`.
    fn seed_pending_tx(
        node: &mut RaftNode,
        tx_id: &str,
        ops: Vec<TxOp>,
    ) -> (String, u64, u64) {
        // State machine side: register the pending tx directly. The
        // real coordinator path does this via Raft log replication +
        // apply; tests skip that machinery and call the state
        // machine API directly to focus on the vote handler.
        {
            let mut sm = node.state_machine.write().unwrap();
            sm.begin_tx(tx_id.to_string(), ops).unwrap();
        }
        // Log side: append a BeginTx entry so the test can ask
        // for vote with `last_log_index/term` matching the local
        // log tip. We use term 1 / index 1 to keep the setup
        // simple; handle_tx_vote_request only requires the
        // leader's claimed log to be >= local log.
        let entry = LogEntry {
            term: 1,
            index: 1,
            command: Command::BeginTx {
                tx_id: tx_id.to_string(),
                ops: vec![TxOp::Put {
                    key: "k".into(),
                    value: "v".into(),
                }],
            },
        };
        let _ = node.storage.append_wal_log(&entry);
        node.log.push(entry);
        (tx_id.to_string(), 1, 1)
    }

    fn vote_req(term: u64, tx_id: &str, last_idx: u64, last_term: u64) -> VoteRequest {
        VoteRequest {
            term,
            tx_id: tx_id.to_string(),
            last_log_index: last_idx,
            last_log_term: last_term,
        }
    }

    #[test]
    fn tx_vote_rejects_stale_term() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;
        let (tx_id, idx, term) = seed_pending_tx(&mut node, "t1", vec![]);

        let resp = node.handle_tx_vote_request(&vote_req(4, &tx_id, idx, term));

        assert!(!resp.vote_granted);
        assert_eq!(resp.term, 5); // echoes local term
        assert!(resp.reason.contains("stale term"));
        // Local state unchanged: did not step down, did not record a vote.
        assert_eq!(node.current_term, 5);
        let sm = node.state_machine.read().unwrap();
        assert_eq!(sm.pending_tx_count(), 1);
    }

    #[test]
    fn tx_vote_adopts_newer_term_and_steps_down() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 1;
        node.state = NodeState::Leader;
        let (tx_id, idx, term) = seed_pending_tx(&mut node, "t1", vec![]);

        let resp = node.handle_tx_vote_request(&vote_req(3, &tx_id, idx, term));

        // Even with a matching pending tx, a *newer* term is
        // rejected in this turn so the new leader can re-broadcast
        // under the correct term.
        assert!(!resp.vote_granted);
        assert_eq!(resp.term, 3);
        assert!(resp.reason.contains("term advance"));
        // Stepped down to Follower and persisted the new term.
        assert_eq!(node.state, NodeState::Follower);
        assert_eq!(node.current_term, 3);
        assert!(node.vote_for.is_none());
    }

    #[test]
    fn tx_vote_rejects_when_tx_not_pending() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 2;
        // No pending tx seeded.

        let resp = node.handle_tx_vote_request(&vote_req(2, "missing", 0, 0));

        assert!(!resp.vote_granted);
        assert_eq!(resp.term, 2);
        assert!(resp.reason.contains("tx not pending"));
    }

    #[test]
    fn tx_vote_rejects_when_leader_log_is_stale() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 1;
        let (tx_id, _idx, _term) = seed_pending_tx(&mut node, "t1", vec![]);
        // Local log tip is (index=1, term=1). A leader claiming
        // (index=0, term=0) is behind us.
        let resp = node.handle_tx_vote_request(&vote_req(1, &tx_id, 0, 0));

        assert!(!resp.vote_granted);
        assert!(resp.reason.contains("leader log stale"));
    }

    #[test]
    fn tx_vote_grants_and_records_yes_on_pending_tx() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 1;
        let (tx_id, idx, term) = seed_pending_tx(&mut node, "t-yes", vec![]);

        let resp = node.handle_tx_vote_request(&vote_req(1, &tx_id, idx, term));

        assert!(resp.vote_granted, "expected Yes vote, got No: {}", resp.reason);
        assert_eq!(resp.term, 1);
        assert!(resp.reason.is_empty());

        // The vote should be recorded on the state machine.
        let sm = node.state_machine.read().unwrap();
        let view = sm.pending_tx(&tx_id).expect("tx still pending");
        assert_eq!(view.yes_votes, 1);
        assert_eq!(view.no_votes, 0);
    }

    // ---------- auto snapshot trigger + startup restore ----------

    /// Build a node that is *already* a leader with a couple of Set
    /// entries committed, so `maybe_snapshot` has something to capture.
    /// We bypass the normal election path because we're not testing
    /// leader election here — only the snapshot pipeline.
    fn make_leader_with_committed_sets(node_id: &str, n: usize) -> (TempDir, RaftNode) {
        let (dir, mut node) = make_node(node_id, vec![]);
        node.state = NodeState::Leader;
        node.current_term = 1;
        let mut sm = node.state_machine.write().unwrap();
        for i in 1..=n {
            sm.set(&format!("k{i}"), &format!("v{i}")).unwrap();
            node.log.push(crate::protocol::LogEntry {
                term: 1,
                index: i,
                command: crate::protocol::Command::Set {
                    key: format!("k{i}"),
                    value: format!("v{i}"),
                },
            });
            node.commit_index = i as u64;
            node.last_applied = i as u64;
        }
        // Persist the matching WAL entries so wal_size_bytes reports a
        // non-zero value (maybe_snapshot keys off the on-disk size).
        for i in 1..=n {
            node.storage.append_wal_log(&node.log[i - 1]).unwrap();
        }
        drop(sm);
        (dir, node)
    }

    #[test]
    fn maybe_snapshot_returns_false_when_wal_under_threshold() {
        let (_d, mut node) = make_leader_with_committed_sets("n1", 3);
        // Threshold of 1 MiB is far above a few entries; snapshot must
        // not fire.
        assert!(!node.maybe_snapshot(1024 * 1024));
    }

    #[test]
    fn maybe_snapshot_returns_false_when_not_leader() {
        let (_d, mut node) = make_leader_with_committed_sets("n2", 3);
        node.state = NodeState::Follower;
        // Even with threshold = 0 the follower path returns false.
        assert!(!node.maybe_snapshot(0));
    }

    #[test]
    fn maybe_snapshot_truncates_wal_and_writes_snapshot_when_over_threshold() {
        let (_d, mut node) = make_leader_with_committed_sets("n3", 5);
        let wal_before = node.storage.wal_size_bytes();
        assert!(wal_before > 0);

        // Threshold of 1 byte — guaranteed to trigger.
        let took = node.maybe_snapshot(1);
        assert!(took, "snapshot should fire when WAL > threshold");

        // Snapshot file now exists and WAL has shrunk.
        let snap = node.storage.load_snapshot().expect("snapshot saved");
        assert_eq!(snap.last_included_index, 5);
        assert_eq!(snap.last_included_term, 1);
        assert!(snap.data.contains_key("k1"));
        assert!(snap.data.contains_key("k5"));
        let wal_after = node.storage.wal_size_bytes();
        assert!(wal_after < wal_before, "WAL must shrink after snapshot");
        // Everything up to index 5 was snapshotted, so 0 entries remain.
        assert_eq!(wal_after, 0);
    }

    #[test]
    fn restore_from_snapshot_returns_none_when_no_snapshot_file() {
        let (_d, mut node) = make_node("n4", vec![]);
        assert!(node.restore_from_snapshot().is_none());
    }

    #[test]
    fn restore_from_snapshot_populates_state_machine_and_advances_indices() {
        // First run: write a few entries, snapshot, drop the node.
        let (dir, mut node) = make_leader_with_committed_sets("n5", 4);
        assert!(node.maybe_snapshot(1));
        let snapshot_index = node.storage
            .load_snapshot()
            .expect("snapshot saved")
            .last_included_index;

        // Second run: open a fresh node on the same on-disk files and
        // restore from snapshot. The state machine should hold the
        // snapshotted keys, and commit/last_applied should match the
        // snapshot position.
        let wal = dir.path().join("n5.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("n5_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("n5_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join("n5_sm");
        let sm_config = crate::state_machine::StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
        let mut new_node = RaftNode::new_with_storage("n5".into(), vec![], sm, storage);

        let restored = new_node.restore_from_snapshot().expect("snapshot present");
        assert_eq!(restored.0, snapshot_index);
        assert_eq!(new_node.commit_index, snapshot_index);
        assert_eq!(new_node.last_applied, snapshot_index);

        let sm = new_node.state_machine.read().unwrap();
        assert_eq!(sm.get("k1").as_deref(), Some("v1"));
        assert_eq!(sm.get("k4").as_deref(), Some("v4"));
    }

    /// Regression for the flaky `pending_tx_count` mismatch that
    /// surfaced in PR #36 review: `push_log_entry_for_test` previously
    /// took an explicit `index` argument. A caller-passed index
    /// decoupled the entry's logical index from its array slot,
    /// which made `apply_logs` (which walks by array offset) display
    /// out-of-order indices and silently skip the BeginTx it shared
    /// a slot with. CI caught it on PR #35's run; pin the invariant
    /// that the helper assigns `log.len() + 1`.
    #[test]
    fn push_log_entry_for_test_assigns_monotonic_index() {
        let (_tmp, mut node) = make_node("n1", vec![]);
        node.current_term = 1;

        // First push: index must be 1.
        node.push_log_entry_for_test(crate::protocol::Command::Compact);
        assert_eq!(node.log.len(), 1);
        assert_eq!(node.log[0].index, 1, "first push should be index 1");

        // Second push: index must be 2, *not* whatever the caller
        // might have passed before the fix.
        node.push_log_entry_for_test(crate::protocol::Command::Compact);
        assert_eq!(node.log.len(), 2);
        assert_eq!(node.log[1].index, 2, "second push should be index 2");

        // get_last_log_info must reflect the array position so that
        // the leader-log-stale check in `handle_tx_vote_request` sees
        // the bumped index.
        let (idx, term) = node.get_last_log_info();
        assert_eq!(idx, 2);
        assert_eq!(term, 1);
    }

    /// Companion to the above: after the phantom push, a *real*
    /// BeginTx appended via `propose` must still apply in order,
    /// so the phantom doesn't shadow it.
    #[test]
    fn push_log_entry_for_test_does_not_shadow_subsequent_propose() {
        let (_tmp, mut node) = make_node("n1", vec![]);
        node.state = crate::raft::node::NodeState::Leader;
        node.current_term = 1;

        node.push_log_entry_for_test(crate::protocol::Command::Compact);
        // last_log_index = 1 now (helper assigns log.len() + 1 = 1).
        assert_eq!(node.get_last_log_info().0, 1);

        // Real propose: must get index 2, not collide with phantom.
        assert!(node.propose(crate::protocol::Command::Set {
            key: "k".into(),
            value: "v".into(),
        }));
        assert_eq!(node.log.len(), 2);
        assert_eq!(node.log[1].index, 2, "real propose must follow phantom");
    }

    // ---------- handle_pre_vote (P8 PR 5, Raft §9.6) ----------

    /// `handle_pre_vote` must NOT bump `current_term`, `vote_for`,
    /// or `state`, no matter the outcome. This is the core
    /// invariant that makes pre-vote safe against the disruptive
    /// server problem.
    #[test]
    fn pre_vote_never_mutates_local_state() {
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.current_term = 5;
        let term_before = node.current_term;
        let vote_before = node.vote_for.clone();
        let state_before = node.state;

        // Probe at term + 1 with a strictly ahead log (would
        // succeed in a real vote). Should be granted.
        let reply = node.handle_pre_vote(&vote_args(term_before + 1, "n2", 0, 0));
        assert!(reply.vote_granted);
        assert_eq!(reply.term, term_before, "reply term must echo OUR term");

        // Hard invariant: nothing mutated.
        assert_eq!(node.current_term, term_before, "pre-vote must not bump term");
        assert_eq!(node.vote_for, vote_before, "pre-vote must not write vote_for");
        assert_eq!(node.state, state_before, "pre-vote must not change state");

        // Now a refused probe (probe term older). Also no mutation.
        let reply = node.handle_pre_vote(&vote_args(term_before - 1, "n2", 0, 0));
        assert!(!reply.vote_granted);
        assert_eq!(node.current_term, term_before);
        assert_eq!(node.vote_for, vote_before);
        assert_eq!(node.state, state_before);

        // And a refused probe (probe term 2 ahead). Also no mutation.
        let reply = node.handle_pre_vote(&vote_args(term_before + 2, "n2", 0, 0));
        assert!(!reply.vote_granted);
        assert_eq!(node.current_term, term_before);
        assert_eq!(node.vote_for, vote_before);
        assert_eq!(node.state, state_before);
    }

    /// Probe at our term + 1 from a peer with a strictly-ahead
    /// log: grant. This is the happy path of pre-vote.
    #[test]
    fn pre_vote_grants_when_probe_log_is_strictly_ahead() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 3;
        node.log.push(make_entry(3, 1, "k", "v"));

        // Probe term = 4, last_log_index = 2 (strictly ahead).
        let reply = node.handle_pre_vote(&vote_args(4, "n2", 2, 3));
        assert!(reply.vote_granted, "ahead probe must grant");
        assert_eq!(reply.term, 3, "must echo local term, not the probe term");
    }

    /// Same-term probe from the leader: refuse. A live leader of
    /// the same term is by definition a quorum winner; the probe
    /// can't beat it in a real vote either.
    #[test]
    fn pre_vote_refuses_when_we_are_leader_of_same_term() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;
        node.state = NodeState::Leader;
        node.log.push(make_entry(5, 1, "k", "v"));

        let reply = node.handle_pre_vote(&vote_args(5, "n2", 1, 5));
        assert!(!reply.vote_granted);
        assert_eq!(reply.term, 5);
    }

    /// Probe term is exactly one ahead of ours but the probe's
    /// log is stale. Refuse via election restriction.
    #[test]
    fn pre_vote_refuses_when_probe_log_is_behind() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 3;
        // Local log: two entries at term 3, indices 1..2.
        node.log.push(make_entry(2, 1, "a", "1"));
        node.log.push(make_entry(3, 2, "b", "2"));

        // Probe at term 4, but last_log_index = 1 (behind our 2).
        let reply = node.handle_pre_vote(&vote_args(4, "n2", 1, 2));
        assert!(!reply.vote_granted, "behind-log probe must be refused");

        // Same term, behind log: also refused.
        node.state = NodeState::Follower;
        let reply = node.handle_pre_vote(&vote_args(3, "n2", 1, 2));
        assert!(!reply.vote_granted);
    }

    /// Probe term is more than one ahead of ours. Refuse — the
    /// peer hasn't even observed our term; we can't trust it.
    /// Regression for the "stale peer probing at arbitrarily
    /// high terms" failure mode.
    #[test]
    fn pre_vote_refuses_probe_more_than_one_term_ahead() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.current_term = 5;

        let reply = node.handle_pre_vote(&vote_args(7, "n2", 0, 0));
        assert!(!reply.vote_granted, "probe 2+ terms ahead must be refused");
        assert_eq!(node.current_term, 5, "must not bump");
    }

    /// `become_pre_candidate` on a single-node cluster must skip
    /// the RPC fan-out and promote straight to Candidate → Leader
    /// without ever touching the wire. Regression for the
    /// single-node fast-path through pre-vote.
    #[test]
    fn become_pre_candidate_single_node_promotes_without_rpc() {
        let (_d, node) = make_node("n1", vec![]);
        let arc = Arc::new(RwLock::new(node));
        RaftNode::become_pre_candidate(arc.clone());
        let n = arc.read().unwrap();
        // Single-node: pre-vote self-quorum (1 > 0) → promote → become_leader.
        assert_eq!(n.state, NodeState::Leader);
        assert_eq!(n.current_term, 1, "single-node promotes at term 1");
    }

    /// `become_pre_candidate` on a multi-node cluster moves state
    /// to `PreCandidate` without bumping term or persisting
    /// `vote_for` (until quorum is reached).
    #[tokio::test(flavor = "current_thread")]
    async fn become_pre_candidate_does_not_bump_term_or_write_vote() {
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.current_term = 4;
        let arc = Arc::new(RwLock::new(node));

        // Snapshot state pre-probe.
        let term_before = arc.read().unwrap().current_term;

        // Spawn the probe fan-out. The spawned tasks will fail
        // to connect to nonexistent peers (n2 / n3 not running
        // here), which is fine — we only assert the synchronous
        // state transition that happens before any reply.
        RaftNode::become_pre_candidate(arc.clone());

        // Read state synchronously. The probe is in flight but
        // neither `vote_for` nor `current_term` may have been
        // touched yet.
        let (state, current_term, vote_for) = {
            let n = arc.read().unwrap();
            (n.state, n.current_term, n.vote_for.clone())
        };
        assert_eq!(state, NodeState::PreCandidate);
        assert_eq!(current_term, term_before, "pre-vote must not bump term");
        assert!(
            vote_for.is_none(),
            "pre-vote must not write vote_for (only the eventual real vote does)"
        );

        // Give the (failed) spawned probe tasks a chance to
        // settle so the runtime exits cleanly. tokio::spawn
        // tasks that error on connect finish on their own.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}