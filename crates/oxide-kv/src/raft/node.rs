use crate::config::Config;
use crate::coordination::{VoteRequest, VoteResponse};
use crate::protocol::{config_quorum_reached_index, AbortTxError, Command, Configuration, LogEntry, MembershipError, ReadIndex, ServerId, Snapshot, TxDecision, Vote};
use crate::raft::clock::{system_clock, Clock};
use crate::raft::net::{system_transport, Transport};
use crate::raft::rpc::{
    AppendEntriesArgs, AppendReplyArgs, InstallSnapshotArgs, InstallSnapshotReplyArgs,
    JoinClusterRequest, JoinClusterResponse, RaftMessage, RequestVoteArgs, VoteResponseArgs,
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

    // ---- Cluster membership (Raft thesis §6) ----
    //
    // The active cluster configuration. Updated by applying committed
    // `Command::InstallConfiguration` log entries (which the leader's
    // `MembershipCoordinator` produces in response to client
    // `Command::AddNode` / `Command::RemoveNode` commands).
    //
    // `peers` above is derived from `current_config.all_servers()`:
    // when membership changes, the leader's `apply_logs` updates
    // `peers` to match. The two fields are kept in sync; if they
    // diverge that's a bug.
    //
    // v1 simplification: `ServerId.node_id == ServerId.addr` for every
    // member. The codebase still keys `match_index` /
    // `next_index` / `vote_for` bookkeeping by the dial address
    // (i.e. the same string under both fields). A future PR can
    // split them properly once we need real
    // network-identity-vs-machine-identity separation.
    pub current_config: Configuration,

    /// Leader-only: the `Configuration::Simple(new)` entry to
    /// auto-propose as soon as a pending `Configuration::Joint` entry
    /// commits. Set by `propose_add_node` /
    /// `propose_remove_node`; consumed by `apply_logs` when the
    /// leader installs a Joint configuration. Reset to `None` after
    /// the Simple entry is appended so we don't double-propose.
    ///
    /// Why this is leader-only: the second phase of joint consensus
    /// only needs to be proposed once, and only the leader can
    /// append log entries. Followers don't need this field.
    pub(crate) pending_post_joint_simple: Option<Configuration>,

    /// Optional Prometheus metrics handle (P8 PR 8). `None` means
    /// "this node is not exporting metrics" — every transition
    /// hook then becomes a no-op. Production nodes set this via
    /// [`RaftNode::set_metrics`] in `main.rs`; tests leave it
    /// `None` to avoid the cost of running a registry.
    pub(crate) metrics: Option<crate::observability::MetricsHandle>,
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

        // Build the initial Configuration::Simple set before moving
        // `peers` into the struct. v1 simplification:
        // `node_id == addr` for every server, so we can build it
        // directly from the peer addrs + the local addr.
        let mut initial_servers: Vec<ServerId> = Vec::with_capacity(peers.len() + 1);
        initial_servers.push(ServerId {
            node_id: raft_addr.clone(),
            addr: raft_addr.clone(),
        });
        for p in &peers {
            initial_servers.push(ServerId {
                node_id: p.clone(),
                addr: p.clone(),
            });
        }

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
            node_id: raft_addr.clone(),
            peers,
            commit_index: 0,
            last_applied: 0,
            last_heartbeat: clock.now(),
            last_quorum_heartbeat_at: None,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            // Initial membership: see `initial_servers` above. The
            // Configuration enum is the source of truth; `peers` is
            // derived from it. Committed membership log entries will
            // mutate this field; a future PR will allow real
            // `node_id != addr` separation.
            current_config: Configuration::Simple(initial_servers),
            // Leader-only (see field doc). New nodes start as
            // followers so this stays `None` until the node is
            // elected leader and a client sends a membership change.
            pending_post_joint_simple: None,
            // Metrics off by default; production wires this in via
            // `set_metrics` after construction. Tests leave it
            // `None` so they don't pay the registry construction
            // cost.
            metrics: None,
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

    /// Borrow the node's outbound `Transport` (used to send Raft
    /// RPCs to peers / candidates). Returns a clone of the
    /// `Arc<dyn Transport>` so callers can issue requests without
    /// holding the `RwLock`.
    ///
    /// Exposed for integration tests that drive a candidate's
    /// `JoinCluster` RPC through the real wire path. Production
    /// code should not need this — the heartbeat / election timer
    /// paths issue their own RPCs internally.
    pub fn transport_handle(&self) -> Arc<dyn crate::raft::net::Transport> {
        self.transport.clone()
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
        // Re-derive `current_config` so the quorum rule matches the
        // new peer set. Used by integration tests to wire up peer
        // connections after nodes have started (since the OS assigns
        // the listen port and the peer list isn't knowable at spawn
        // time). v1 simplification: node_id == addr for every server.
        let mut servers: Vec<ServerId> = Vec::with_capacity(peers.len() + 1);
        servers.push(ServerId {
            node_id: self.node_id.clone(),
            addr: self.node_id.clone(),
        });
        for p in &peers {
            servers.push(ServerId {
                node_id: p.clone(),
                addr: p.clone(),
            });
        }
        self.current_config = Configuration::Simple(servers);
        self.peers = peers;
    }

    /// Wire the Prometheus metrics handle into this node. Called
    /// once from `main.rs` after constructing the node. Idempotent:
    /// calling it again replaces the previous handle.
    ///
    /// Once set, every state-transition hook (`become_leader`,
    /// `apply_logs`, `maybe_snapshot`, ...) eagerly updates the
    /// corresponding gauge so `/metrics` scraping never blocks on
    /// the `RaftNode` lock.
    pub fn set_metrics(&mut self, metrics: crate::observability::MetricsHandle) {
        // Seed the snapshot_age_seconds + snapshot_bytes gauges
        // from the on-disk snapshot file (if any). The caller did
        // `restore_from_snapshot` *before* this point, so the file
        // is still on disk (the snapshot isn't deleted on restore —
        // it's left in place so a crash before compaction doesn't
        // lose the last good state). We only read metadata here;
        // no state mutation. If there's no snapshot file, the
        // sentinel `-1` values set by `Metrics::new` stay.
        let path = crate::config::Config::global().snapshot_path();
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified()
                && let Ok(age) = modified.elapsed()
            {
                metrics
                    .raft_snapshot_age_seconds
                    .set(age.as_secs() as i64);
            }
            metrics.raft_snapshot_bytes.set(meta.len() as i64);
        }
        self.metrics = Some(metrics);
    }

    /// Refresh the eagerly-updated gauges that mirror RaftNode
    /// state. Called from every transition (`become_leader`,
    /// `become_candidate`, `become_pre_candidate`, `apply_logs`,
    /// `propose`, `sync_logs` reply handler, `maybe_snapshot`,
    /// `set_peers`).
    ///
    /// No-op when `self.metrics` is `None` (the test default),
    /// so the existing test surface stays untouched. When
    /// `Some`, this updates:
    ///
    /// - `raft_term`            = `current_term`
    /// - `raft_commit_index`    = `commit_index`
    /// - `raft_last_applied`    = `last_applied`
    /// - `raft_log_length`      = `log.len()`
    /// - `raft_role`            = `state` (encoded)
    ///
    /// `peer_match_index` / `peer_next_index` are updated by the
    /// AppendEntries reply handler (it has the per-peer value).
    /// `tx_pending_count` is updated by `ClientHandler` (it has
    /// the state machine lock).
    pub fn refresh_metrics(&self) {
        if let Some(m) = &self.metrics {
            m.raft_term.set(self.current_term as i64);
            m.raft_commit_index.set(self.commit_index as i64);
            m.raft_last_applied.set(self.last_applied as i64);
            m.raft_log_length.set(self.log.len() as i64);
            m.raft_role.set(match self.state {
                NodeState::Follower => 0,
                NodeState::Candidate => 1,
                NodeState::Leader => 2,
                NodeState::PreCandidate => 3,
            });
        }
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

    /// P8 PR 6a: cold-new-server catch-up.
    ///
    /// A brand-new server (with empty `peers`) cannot be reached via
    /// the normal AppendEntries path because no one in the cluster
    /// knows its address. The new server therefore initiates contact
    /// by sending `JoinClusterRequest` to a *hint* address it learned
    /// out-of-band (DNS / systemd env / manual bootstrap).
    ///
    /// This handler is **leader-only**. Non-leaders reject so the
    /// candidate can re-route to the leader (or fall back to a
    /// different hint if the cluster has moved on). The leader's
    /// reply carries the current peer list so the candidate can
    /// populate its `peers` field before `propose_add_node` runs
    /// (see P8 PR 6's `propose_add_node`).
    ///
    /// Validation:
    /// - `state != Leader` → reject with `accepted=false` (candidate
    ///   should re-route).
    /// - `candidate_addr == self.node_id` → reject (the hint landed
    ///   on the candidate itself; not a sensible cluster membership).
    /// - `candidate_addr` already in `current_config.all_servers()`
    ///   → reject (the candidate is a member of an existing cluster
    ///   or already joined this one; idempotent retry safety net).
    /// - Otherwise accept and return `peer_addrs = all_servers \ {self, candidate}`.
    ///
    /// This is **not** a log entry. `JoinCluster` does not participate
    /// in quorum or replication. The actual cluster membership change
    /// happens later via the Joint consensus path (P8 PR 6).
    pub fn handle_join_cluster(&self, req: &JoinClusterRequest) -> JoinClusterResponse {
        let reject = |reason: &str| JoinClusterResponse {
            accepted: false,
            term: self.current_term,
            leader_addr: self.node_id.clone(),
            peer_addrs: Vec::new(),
            reason: reason.to_string(),
        };

        if self.state != NodeState::Leader {
            return reject("not leader");
        }

        if req.candidate_addr == self.node_id {
            return reject("candidate_addr is the leader itself");
        }

        // Membership check: candidate_addr must NOT already be in
        // the cluster. `all_servers()` covers Simple (== self.peers
        // ∪ {self}) and Joint (== union of old ∪ new). Same check
        // applies regardless of config shape.
        let already_member = self
            .current_config
            .all_servers()
            .iter()
            .any(|s| s.addr == req.candidate_addr || s.node_id == req.candidate_addr);

        if already_member {
            return reject("candidate_addr is already a cluster member");
        }

        // Build peer_addrs = all_servers \ {self, candidate_addr}.
        // In v1, server.node_id == server.addr for every server.
        let peer_addrs: Vec<String> = self
            .current_config
            .all_servers()
            .into_iter()
            .map(|s| s.addr)
            .filter(|addr| addr != &self.node_id && addr != &req.candidate_addr)
            .collect();

        JoinClusterResponse {
            accepted: true,
            term: self.current_term,
            leader_addr: self.node_id.clone(),
            peer_addrs,
            reason: String::new(),
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
        // Refresh so `raft_log_length` tracks the appended entry
        // immediately. `apply_logs` will refresh again once the
        // entry commits, but eager updates help dashboards
        // distinguish "the leader proposed" from "the entry
        // committed".
        self.refresh_metrics();
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
        // See `propose` for rationale; the batch path needs the
        // same single refresh at the end, not per-iteration.
        self.refresh_metrics();
        true
    }

    /// P8 PR 6 (Raft thesis §6): leader-side membership change.
    ///
    /// Translates a client `AddNode` request into the two-phase
    /// joint-consensus log sequence:
    ///   1. `InstallConfiguration { config: Joint { old, new ∪ {server} } }`
    ///      — commits under the dual-majority quorum rule.
    ///   2. `InstallConfiguration { config: Simple(new ∪ {server}) }`
    ///      — commits under the new-majority quorum rule.
    ///
    /// Only the first entry is appended by this method. The second
    /// is appended automatically by `apply_logs` once the leader
    /// installs the Joint config (it pops
    /// `pending_post_joint_simple`).
    ///
    /// Returns the index of the Joint entry the caller can wait on,
    /// or `Err(NotLeader)` / `Err(AlreadyMember)` if the request
    /// can't be processed.
    pub fn propose_add_node(
        &mut self,
        server: ServerId,
    ) -> Result<u64, MembershipError> {
        if self.state != NodeState::Leader {
            return Err(MembershipError::NotLeader);
        }
        // Refuse if the server is already in `current_config` (no-op
        // for both Simple and Joint).
        if self.current_config.contains(&server.node_id) {
            return Err(MembershipError::AlreadyMember(server.node_id));
        }
        // Cold-new-server catch-up: if the joining server has no
        // log entries (we have no way to know its log state
        // without an RPC, so this is best-effort), rely on the
        // existing AppendEntries path: the new server will be in
        // `current_config.new`, so the leader will start sending
        // AppendEntries to it. If its log is empty, the leader
        // will catch it up entry-by-entry. If its log is *very*
        // far behind (e.g. snapshot boundary), the leader's
        // snapshot-installation path (InstallSnapshot) covers
        // that — currently only triggered when next_index falls
        // below snapshot's last_included_index.
        //
        // We don't try to verify the new server is reachable here;
        // the user is responsible for standing up the new node
        // before sending this command.
        let old = self.current_config.all_servers();
        let mut new = old.clone();
        new.push(server);
        let joint = Configuration::Joint {
            old: old.clone(),
            new: new.clone(),
        };
        let simple = Configuration::Simple(new);

        // Append the Joint entry.
        let joint_index = self.log.len() as u64 + 1;
        let entry = LogEntry {
            term: self.current_term,
            index: joint_index as usize,
            command: Command::InstallConfiguration { config: joint },
        };
        if let Err(e) = self.storage.append_wal_log(&entry) {
            eprintln!("[Error] Failed to append Joint config: {}", e);
            return Err(MembershipError::StorageError(e.to_string()));
        }
        self.log.push(entry);
        // Record the Simple entry to be auto-proposed when the
        // Joint commits.
        self.pending_post_joint_simple = Some(simple);
        Ok(joint_index)
    }

    /// P8 PR 6: leader-side membership removal. Mirror of
    /// `propose_add_node`. Translates to `Joint { old, new \ {node_id} }`
    /// followed by `Simple(new \ {node_id})`.
    pub fn propose_remove_node(
        &mut self,
        node_id: &str,
    ) -> Result<u64, MembershipError> {
        if self.state != NodeState::Leader {
            return Err(MembershipError::NotLeader);
        }
        // Can't remove ourselves.
        if node_id == self.node_id {
            return Err(MembershipError::CannotRemoveSelf);
        }
        if !self.current_config.contains(node_id) {
            return Err(MembershipError::NotMember(node_id.to_string()));
        }
        let old = self.current_config.all_servers();
        let new: Vec<ServerId> = old
            .iter()
            .filter(|s| s.node_id != node_id)
            .cloned()
            .collect();
        // Refuse to remove the last server (would leave a cluster
        // with no quorum).
        if new.is_empty() {
            return Err(MembershipError::CannotRemoveLastServer);
        }
        let joint = Configuration::Joint {
            old: old.clone(),
            new: new.clone(),
        };
        let simple = Configuration::Simple(new);

        let joint_index = self.log.len() as u64 + 1;
        let entry = LogEntry {
            term: self.current_term,
            index: joint_index as usize,
            command: Command::InstallConfiguration { config: joint },
        };
        if let Err(e) = self.storage.append_wal_log(&entry) {
            eprintln!("[Error] Failed to append Joint config: {}", e);
            return Err(MembershipError::StorageError(e.to_string()));
        }
        self.log.push(entry);
        self.pending_post_joint_simple = Some(simple);
        Ok(joint_index)
    }

    /// Admin RPC: force-abort a stuck 2PC transaction by `tx_id`.
    ///
    /// P8 PR 7 closes the coordinator-crash hole: if the leader
    /// dies mid-2PC, the BeginTx entry is left in every follower's
    /// `pending_txs` table forever (no DecideTx comes through).
    /// This method proposes a `DecideTx(Abort)` log entry for the
    /// stuck tx, which replicates to every follower through the
    /// normal AppendEntries path; followers apply it through
    /// `apply_logs` and purge the pending entry.
    ///
    /// Same entry point serves two callers:
    ///   1. The client-facing JSON `AbortTx` command (manual ops
    ///      recovery) — see `client.rs::abort_tx`.
    ///   2. The coordinator-side timeout sweep (automatic recovery)
    ///      — see `raft/coordinator.rs::run_tx_timeout_loop`.
    ///
    /// Validation:
    ///   - **Leader-only.** A non-leader cannot guarantee replication,
    ///     so this returns `AbortTxError::NotLeader` for any
    ///     non-Leader state. The JSON dispatch guard already rejects
    ///     non-leaders, so this is a belt-and-braces check.
    ///   - **Tx must be in `pending_txs`.** Aborting a tx that was
    ///     already decided (Commit / Abort) is a no-op at best and
    ///     misleading at worst — return `AbortTxError::NotFound` so
    ///     the operator gets a clear error.
    ///
    /// On success, returns the log index of the new `DecideTx(Abort)`
    /// entry (the caller may want this for log correlation).
    pub fn propose_abort_tx(&mut self, tx_id: &str) -> Result<u64, AbortTxError> {
        if self.state != NodeState::Leader {
            return Err(AbortTxError::NotLeader);
        }
        if !self.state_machine.read().unwrap().is_tx_pending(tx_id) {
            return Err(AbortTxError::NotFound(tx_id.to_string()));
        }
        // Translate to a `DecideTx(Abort)` log entry. The log
        // entry is the only thing that flows through Raft — the
        // client-facing `AbortTx` command never goes on the wire.
        let new_index = self.log.len() as u64 + 1;
        let entry = LogEntry {
            term: self.current_term,
            index: new_index as usize,
            command: Command::DecideTx {
                tx_id: tx_id.to_string(),
                decision: TxDecision::Abort,
            },
        };
        if let Err(e) = self.storage.append_wal_log(&entry) {
            return Err(AbortTxError::StorageError(e.to_string()));
        }
        self.log.push(entry);
        self.refresh_metrics();
        // Trigger replication immediately rather than waiting
        // for the next heartbeat tick. The operator's workflow
        // expects a fast ack ("aborted at log index N").
        Ok(new_index)
    }

    /// Leader-only accessor: `tx_id`s of pending txs older than
    /// `threshold`. Used by the coordinator-side timeout sweep
    /// (P8 PR 7). Returns an empty vec on a non-leader (callers
    /// should check `is_leader()` first to avoid silent no-ops).
    pub fn pending_txs_older_than(&self, threshold: std::time::Duration) -> Vec<String> {
        self.state_machine
            .read()
            .unwrap()
            .pending_txs_older_than(threshold)
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
                let next = *n.next_index.get(&peer_addr_clone).unwrap_or(&1);
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
                            // Per-peer metric refresh — both gauges
                            // update together because they're
                            // coupled (match_index + 1 == next_index
                            // in steady state, see `apply_logs`).
                            if let Some(m) = &n.metrics {
                                m.set_peer_match_index(&peer_addr_clone, last_idx as i64);
                                m.set_peer_next_index(&peer_addr_clone, (last_idx + 1) as i64);
                            }
                            n.maybe_commit();
                        } else if reply.term > n.current_term {
                            n.current_term = reply.term;
                            n.state = NodeState::Follower;
                            n.vote_for = None;
                            let _ = n.storage.save_meta(n.current_term.clone(), n.vote_for.clone());
                            n.refresh_metrics();
                        } else {
                            // Log inconsistency: decrement next_index and retry
                            let next = n.next_index.get(&peer_addr_clone).cloned().unwrap_or(1);
                            if next > 1 {
                                n.next_index.insert(peer_addr_clone.clone(), next - 1);
                                if let Some(m) = &n.metrics {
                                    m.set_peer_next_index(&peer_addr_clone, (next - 1) as i64);
                                }
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

        // P8 PR 6 (Raft thesis §6): quorum rule depends on the
        // active configuration. `Simple` is plain majority;
        // `Joint { old, new }` requires majority of BOTH old and
        // new. The committed entry must satisfy both majorities
        // simultaneously — that's the rule that prevents the
        // disjoint-majorities bug.
        //
        // We compute the highest index that satisfies the current
        // configuration's quorum rule. Entries below that index
        // that didn't satisfy quorum are not advanced.
        //
        // Walking the log backward from `self.log.len()` is O(n)
        // per call but correct; in practice `maybe_commit` is only
        // called on heartbeat ack / sync completion, so the cost is
        // amortized. A future PR can optimize by tracking the
        // highest quorum-satisfying index incrementally.
        let self_index = self.log.len() as u64;
        let highest_quorum_index = config_quorum_reached_index(
            &self.current_config,
            &self.match_index,
            &self.node_id,
            self_index,
        );

        if highest_quorum_index > self.commit_index {
            // Safety: Leader can only commit entries from its current term.
            // (Raft §5.4.2.)
            let log_term = self
                .log
                .get((highest_quorum_index - 1) as usize)
                .map(|e| e.term)
                .unwrap_or(0);
            if log_term == self.current_term {
                self.commit_index = highest_quorum_index;
                println!(
                    "🚀 [Commit] Majority reached under {:?}! Commit Index advanced to {}",
                    self.current_config, highest_quorum_index
                );
                self.apply_logs();
            }
        }
    }

    pub fn apply_logs(&mut self) {
        while self.last_applied < self.commit_index {
            let log_idx_to_apply = self.last_applied as usize;

            let Some(entry) = self.log.get(log_idx_to_apply) else {
                eprintln!("[Critical] Log entry {} not found during apply", self.last_applied + 1);
                break;
            };
            // Snapshot the command first so we can drop the
            // borrow on `self.log` before mutating state machine
            // + RaftNode state.
            let cmd = entry.command.clone();
            let entry_idx = entry.index;

            // Partition commands into "state machine effects" (need
            // the state machine lock) and "membership effects" (need
            // `&mut self` to mutate `current_config` /
            // `pending_post_joint_simple`). We process the former
            // with the state-machine lock held, then drop the lock
            // before processing the latter.
            let needs_sm = matches!(
                cmd,
                Command::Set { .. }
                    | Command::Delete { .. }
                    | Command::BeginTx { .. }
                    | Command::DecideTx { .. }
            );
            if needs_sm {
                let mut state_machine = self.state_machine.write().unwrap();
                match &cmd {
                    Command::Set { key, value } => {
                        let _ = state_machine.set(&*key.clone(), &*value.clone());
                        println!("✅ [Apply] Index {}: SET {} = {}", entry_idx, key, value);
                    }
                    Command::Delete { key } => {
                        let _ = state_machine.delete(&key);
                        println!("✅ [Apply] Index {}: DELETE {}", entry_idx, key);
                    }
                    Command::BeginTx { tx_id, ops } => {
                        let _ = state_machine.begin_tx(tx_id.clone(), ops.clone());
                        println!("✅ [Apply] Index {}: BEGIN_TX {} ({} ops)", entry_idx, tx_id, ops.len());
                    }
                    Command::DecideTx { tx_id, decision } => {
                        let _ = state_machine.decide_tx(tx_id, decision.clone());
                        println!("✅ [Apply] Index {}: DECIDE_TX {} = {:?}", entry_idx, tx_id, decision);
                    }
                    _ => unreachable!("needs_sm implies one of the above"),
                }
            } else {
                match &cmd {
                    Command::Get { .. } => println!("🔍 [Apply] Index {}: GET (no-op)", entry_idx),
                    Command::Compact => println!("🔍 [Apply] Index {}: Compact marker (no-op)", entry_idx),
                    Command::InstallConfiguration { config } => {
                        // P8 PR 6: install the new membership configuration.
                        // Update `current_config`, derive `peers` from
                        // `all_servers()`, and (leader-only) propose
                        // the Simple(new) entry that follows a Joint.
                        println!(
                            "📐 [Apply] Index {}: InstallConfiguration {}",
                            entry_idx,
                            match config {
                                Configuration::Simple(s) => format!("Simple({} servers)", s.len()),
                                Configuration::Joint { old, new } => format!("Joint(old:{}, new:{})", old.len(), new.len()),
                            }
                        );
                        self.install_configuration(config);
                    }
                    // `AddNode` / `RemoveNode` are *client-facing*
                    // membership commands; the leader's
                    // MembershipCoordinator converts them to
                    // `InstallConfiguration` entries before
                    // replication. They should never appear in a
                    // committed log entry we observe. Treat as a
                    // no-op and warn (a missing warn-then-noop
                    // silently skips).
                    Command::AddNode { .. } | Command::RemoveNode { .. } => {
                        eprintln!(
                            "[WARN] Index {}: client-facing {:?} appeared in committed log; \
                             this should have been translated to InstallConfiguration",
                            entry_idx,
                            std::mem::discriminant(&cmd)
                        );
                    }
                    _ => {}
                }
            }
            self.last_applied += 1;
        }
        // Refresh gauges after the apply loop. Either nothing
        // happened (`commit_index == last_applied`, no-op below)
        // or `last_applied` / `log.len()` advanced.
        self.refresh_metrics();
    }

    /// Install a committed `Configuration` entry.
    ///
    /// Updates `self.current_config` and re-derives `self.peers`
    /// from `config.all_servers()`. On the leader, if `config` is
    /// `Joint` and `pending_post_joint_simple` is set, proposes the
    /// second phase (a `Simple(new)` entry) so the membership
    /// change completes automatically.
    fn install_configuration(&mut self, config: &Configuration) {
        // 1. Derive the new `peers` dial list from `all_servers()`.
        let new_peers: Vec<String> = config
            .all_servers()
            .into_iter()
            .filter(|s| s.node_id != self.node_id)
            .map(|s| s.addr)
            .collect();
        // 2. Install the new active configuration.
        self.current_config = config.clone();
        self.peers = new_peers;

        // 3. Leader-only: append the Simple(new) entry that follows
        // a Joint(old, new). On Follower this branch is dormant —
        // the leader's AppendEntries will eventually deliver the
        // Simple entry, and `apply_logs` will install it then.
        if self.state == NodeState::Leader
            && let Some(simple_new) = self.pending_post_joint_simple.take()
        {
            // Only propose if `simple_new` matches the just-installed
            // joint's `new`. (If a new membership request arrived
            // while we were committing the previous joint, the
            // newer request would have overwritten
            // `pending_post_joint_simple`.)
            let matches = matches!(
                config,
                Configuration::Joint { new, .. } if new == &simple_new.all_servers()
            ) || matches!(
                &simple_new,
                Configuration::Simple(servers) if matches!(
                    config,
                    Configuration::Simple(c) if c == servers
                )
            );
            if matches {
                let new_index = self.log.len() as u64 + 1;
                let entry = LogEntry {
                    term: self.current_term,
                    index: new_index as usize,
                    command: Command::InstallConfiguration {
                        config: simple_new,
                    },
                };
                if let Err(e) = self.storage.append_wal_log(&entry) {
                    eprintln!(
                        "[Error] Failed to append Simple(new) log entry: {}",
                        e
                    );
                    return;
                }
                self.log.push(entry);
                // The next sync_logs / heartbeat cycle will replicate
                // this entry to peers. We don't trigger sync_logs
                // synchronously here — apply_logs is called from
                // commit advancement paths that already have a sync
                // in flight, and triggering another one synchronously
                // risks lock contention.
                println!(
                    "📐 [Membership] Leader auto-proposed Simple(new) \
                     at index {} after Joint commit",
                    new_index
                );
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
                Command::InstallConfiguration { config } => {
                    // Replay path: when restoring after restart, the
                    // membership configuration must reflect the
                    // *last* committed Configuration entry (Joint or
                    // Simple). We update current_config + peers but
                    // do NOT trigger the post-joint hook — replay is
                    // for crash recovery, and proposing a new entry
                    // here would be wrong (the Simple(new) entry that
                    // follows a Joint is, if it was committed before
                    // the crash, already in the WAL below).
                    //
                    // We just install the latest configuration we
                    // see. If both Joint and Simple are present, the
                    // Simple overwrites the Joint — which is the
                    // correct final state.
                    self.current_config = config.clone();
                    self.peers = config
                        .all_servers()
                        .into_iter()
                        .filter(|s| s.node_id != self.node_id)
                        .map(|s| s.addr)
                        .collect();
                }
                _ => {}
            }
        }
        self.last_applied = self.log.len() as u64;
        println!(
            "✅ [Replay] Successfully replayed {} logs to state machine \
             (current membership: {:?})",
            self.log.len(),
            self.current_config
        );
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
        node.refresh_metrics();
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
                node.refresh_metrics();
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
        self.refresh_metrics();
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
        // Refresh snapshot_age_seconds / snapshot_bytes (follower
        // received this snapshot from the leader). Same pattern
        // as `maybe_snapshot`.
        if let Some(m) = &self.metrics {
            m.raft_snapshot_age_seconds.set(0);
            let path = crate::config::Config::global().snapshot_path();
            if let Ok(meta) = std::fs::metadata(&path) {
                m.raft_snapshot_bytes.set(meta.len() as i64);
            }
        }
        self.refresh_metrics();

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
        // Refresh snapshot_age_seconds / snapshot_bytes. The
        // metrics handle is best-effort — if `set_metrics` was
        // never called (e.g. unit tests), `self.metrics` is
        // `None` and we silently skip the update.
        if let Some(m) = &self.metrics {
            m.raft_snapshot_age_seconds.set(0);
            let path = crate::config::Config::global().snapshot_path();
            if let Ok(meta) = std::fs::metadata(&path) {
                m.raft_snapshot_bytes.set(meta.len() as i64);
            }
        }
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
    use crate::protocol::{config_quorum_reached, Command, LogEntry, ReadIndex, Snapshot, TxDecision, TxOp};
    use crate::raft::rpc::{AppendEntriesArgs, InstallSnapshotArgs, RequestVoteArgs};
    use crate::raft::storage::RaftStorage;
    use crate::state_machine::{StateMachine, StateMachineConfig};
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

    // =====================================================================
    // P8 PR 6: joint consensus membership change (Raft thesis §6)
    // =====================================================================
    //
    // Tests pin the invariants of the new membership layer:
    //   - `Configuration` math: `all_servers`, `addr_of`, `contains`,
    //     `config_quorum_reached` (joint vs simple quorum).
    //   - `current_config` initialization from the peer list.
    //   - `propose_add_node` / `propose_remove_node` correctness.
    //   - `install_configuration` updates `peers` and `current_config`.
    //   - Joint consensus dual-majority: 3-node cluster refuses a
    //     commit that doesn't have BOTH old and new majorities.

    #[test]
    fn configuration_simple_contains_and_addr_of() {
        let cfg = Configuration::Simple(vec![
            ServerId { node_id: "n1".into(), addr: "127.0.0.1:9001".into() },
            ServerId { node_id: "n2".into(), addr: "127.0.0.1:9002".into() },
        ]);
        assert!(cfg.contains("n1"));
        assert!(cfg.contains("n2"));
        assert!(!cfg.contains("n3"));
        assert_eq!(cfg.addr_of("n2"), Some("127.0.0.1:9002".to_string()));
        assert_eq!(cfg.addr_of("n3"), None);
        assert_eq!(cfg.size(), 2);
    }

    #[test]
    fn configuration_joint_all_servers_is_union_without_dupes() {
        let cfg = Configuration::Joint {
            old: vec![
                ServerId { node_id: "n1".into(), addr: "127.0.0.1:9001".into() },
                ServerId { node_id: "n2".into(), addr: "127.0.0.1:9002".into() },
            ],
            new: vec![
                ServerId { node_id: "n1".into(), addr: "127.0.0.1:9001".into() },
                ServerId { node_id: "n2".into(), addr: "127.0.0.1:9002".into() },
                ServerId { node_id: "n3".into(), addr: "127.0.0.1:9003".into() },
            ],
        };
        let all = cfg.all_servers();
        // n1 and n2 are in both; the union should dedupe.
        assert_eq!(all.len(), 3);
        let ids: Vec<&str> = all.iter().map(|s| s.node_id.as_str()).collect();
        assert!(ids.contains(&"n1"));
        assert!(ids.contains(&"n2"));
        assert!(ids.contains(&"n3"));
        assert_eq!(cfg.size(), 3); // max(old.len, new.len)
    }

    #[test]
    fn config_quorum_simple_majority() {
        // 3 servers, 2 of them replicated index 5 -> quorum reached.
        let cfg = Configuration::Simple(vec![
            ServerId { node_id: "n1".into(), addr: "a".into() },
            ServerId { node_id: "n2".into(), addr: "b".into() },
            ServerId { node_id: "n3".into(), addr: "c".into() },
        ]);
        let mut mi = HashMap::new();
        mi.insert("n2".into(), 5);
        mi.insert("n3".into(), 3);
        // n1 self has 5. count = 2 (n1 self + n2), len/2 = 1. 2 > 1 -> quorum.
        assert!(config_quorum_reached(
            &cfg,
            &mi,
            "n1",
            5,
            5
        ));
        // Now only n1 has replicated: count = 1, len/2 = 1. 1 > 1 is false.
        mi.insert("n2".into(), 0);
        mi.insert("n3".into(), 0);
        assert!(!config_quorum_reached(
            &cfg,
            &mi,
            "n1",
            5,
            5
        ));
    }

    #[test]
    fn config_quorum_joint_requires_both_majorities() {
        // 3-node -> 4-node transition: old = {n1, n2, n3}, new = {n1, n2, n3, n4}.
        let cfg = Configuration::Joint {
            old: vec![
                ServerId { node_id: "n1".into(), addr: "a".into() },
                ServerId { node_id: "n2".into(), addr: "b".into() },
                ServerId { node_id: "n3".into(), addr: "c".into() },
            ],
            new: vec![
                ServerId { node_id: "n1".into(), addr: "a".into() },
                ServerId { node_id: "n2".into(), addr: "b".into() },
                ServerId { node_id: "n3".into(), addr: "c".into() },
                ServerId { node_id: "n4".into(), addr: "d".into() },
            ],
        };
        let mut mi = HashMap::new();
        // n2 + n3 replicated, n4 (new-only) not yet -> old has 3/3, new has 3/4.
        mi.insert("n2".into(), 5);
        mi.insert("n3".into(), 5);
        // old majority: n1 + n2 + n3 = 3 > 3/2 = 1 ✓
        // new majority: n1 + n2 + n3 = 3 > 4/2 = 2 ✓
        assert!(config_quorum_reached(
            &cfg,
            &mi,
            "n1",
            5,
            5
        ));
        // Now drop n3: only n2 replicated + self.
        mi.insert("n3".into(), 0);
        // old: n1 + n2 = 2 > 1 ✓
        // new: n1 + n2 = 2 > 2 ✗  (need 3 of 4)
        assert!(!config_quorum_reached(
            &cfg,
            &mi,
            "n1",
            5,
            5
        ));
    }

    #[test]
    fn initial_current_config_is_simple_self_plus_peers() {
        let (_d, node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        let cfg = &node.current_config;
        assert!(matches!(cfg, Configuration::Simple(_)));
        assert_eq!(cfg.size(), 3);
        assert!(cfg.contains("n1"));
        assert!(cfg.contains("n2"));
        assert!(cfg.contains("n3"));
        // v1 simplification: node_id == addr.
        assert_eq!(cfg.addr_of("n1"), Some("n1".to_string()));
    }

    #[test]
    fn propose_add_node_requires_leader() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        // Follower (not leader).
        let server = ServerId { node_id: "n3".into(), addr: "n3".into() };
        let result = node.propose_add_node(server);
        assert_eq!(result, Err(MembershipError::NotLeader));
    }

    #[test]
    fn propose_add_node_rejects_already_member() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        let server = ServerId { node_id: "n2".into(), addr: "n2".into() };
        let result = node.propose_add_node(server);
        assert_eq!(
            result,
            Err(MembershipError::AlreadyMember("n2".to_string()))
        );
    }

    #[test]
    fn propose_add_node_appends_joint_entry_and_queues_simple() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        let server = ServerId { node_id: "n3".into(), addr: "n3".into() };
        let joint_idx = node.propose_add_node(server).expect("leader should propose");
        // One log entry should be appended (the Joint).
        assert_eq!(joint_idx, 1);
        assert_eq!(node.log.len(), 1);
        match &node.log[0].command {
            Command::InstallConfiguration { config } => match config {
                Configuration::Joint { old, new } => {
                    assert_eq!(old.len(), 2); // n1 + n2
                    assert_eq!(new.len(), 3); // n1 + n2 + n3
                }
                _ => panic!("expected Joint, got Simple"),
            },
            _ => panic!("expected InstallConfiguration log entry"),
        }
        // pending_post_joint_simple should be set to Simple({n1, n2, n3}).
        match &node.pending_post_joint_simple {
            Some(Configuration::Simple(s)) => {
                assert_eq!(s.len(), 3);
                assert!(s.iter().any(|x| x.node_id == "n3"));
            }
            _ => panic!("expected pending Simple(n1,n2,n3)"),
        }
    }

    #[test]
    fn propose_remove_node_rejects_self() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        let result = node.propose_remove_node("n1");
        assert_eq!(result, Err(MembershipError::CannotRemoveSelf));
    }

    #[test]
    fn propose_remove_node_rejects_last_server() {
        let (_d, mut node) = make_node("solo", vec![]);
        node.state = NodeState::Leader;
        // No peers: removing "solo" would leave zero servers.
        let result = node.propose_remove_node("solo");
        assert_eq!(result, Err(MembershipError::CannotRemoveSelf));
    }

    #[test]
    fn propose_remove_node_appends_joint_with_smaller_new() {
        let (_d, mut node) = make_node("n1", vec!["n2".into(), "n3".into()]);
        node.state = NodeState::Leader;
        let joint_idx = node.propose_remove_node("n3").expect("ok");
        assert_eq!(joint_idx, 1);
        match &node.log[0].command {
            Command::InstallConfiguration { config } => match config {
                Configuration::Joint { old, new } => {
                    assert_eq!(old.len(), 3);
                    assert_eq!(new.len(), 2);
                    assert!(!new.iter().any(|x| x.node_id == "n3"));
                }
                _ => panic!("expected Joint"),
            },
            _ => panic!("expected InstallConfiguration"),
        }
    }

    #[test]
    fn install_configuration_updates_peers_and_current_config() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        let new_cfg = Configuration::Simple(vec![
            ServerId { node_id: "n1".into(), addr: "n1".into() },
            ServerId { node_id: "n2".into(), addr: "n2".into() },
            ServerId { node_id: "n3".into(), addr: "n3".into() },
        ]);
        node.install_configuration(&new_cfg);
        assert_eq!(node.current_config, new_cfg);
        // `peers` should now include n3 (excludes self).
        assert_eq!(node.peers, vec!["n2".to_string(), "n3".to_string()]);
    }

    #[test]
    fn install_configuration_on_leader_proposes_simple_after_joint() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        // Stage the pending Simple entry as if `propose_add_node` had set it.
        let pending = Configuration::Simple(vec![
            ServerId { node_id: "n1".into(), addr: "n1".into() },
            ServerId { node_id: "n2".into(), addr: "n2".into() },
            ServerId { node_id: "n3".into(), addr: "n3".into() },
        ]);
        node.pending_post_joint_simple = Some(pending.clone());
        // Now install the matching Joint entry.
        let joint = Configuration::Joint {
            old: vec![
                ServerId { node_id: "n1".into(), addr: "n1".into() },
                ServerId { node_id: "n2".into(), addr: "n2".into() },
            ],
            new: pending.all_servers(),
        };
        node.install_configuration(&joint);
        // The pending Simple should now have been auto-proposed.
        assert!(node.pending_post_joint_simple.is_none());
        assert_eq!(node.log.len(), 1);
        match &node.log[0].command {
            Command::InstallConfiguration { config } => {
                assert_eq!(*config, pending);
            }
            _ => panic!("expected InstallConfiguration log entry"),
        }
    }

    #[test]
    fn apply_logs_handles_install_configuration() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        node.state = NodeState::Leader;
        // Append a Simple(n1, n2, n3) entry and commit it.
        let new_cfg = Configuration::Simple(vec![
            ServerId { node_id: "n1".into(), addr: "n1".into() },
            ServerId { node_id: "n2".into(), addr: "n2".into() },
            ServerId { node_id: "n3".into(), addr: "n3".into() },
        ]);
        let entry = LogEntry {
            term: 1,
            index: 1,
            command: Command::InstallConfiguration { config: new_cfg.clone() },
        };
        node.log.push(entry);
        node.commit_index = 1;
        node.apply_logs();
        // current_config should be installed; peers should include n3.
        assert_eq!(node.current_config, new_cfg);
        assert_eq!(node.peers, vec!["n2".to_string(), "n3".to_string()]);
        assert_eq!(node.last_applied, 1);
    }

    #[test]
    fn replay_logs_installs_final_configuration() {
        let (_d, mut node) = make_node("n1", vec!["n2".into()]);
        // Build a log with Joint(n1, n2, n3) followed by Simple(n1, n2, n3).
        let joint = Configuration::Joint {
            old: vec![
                ServerId { node_id: "n1".into(), addr: "n1".into() },
                ServerId { node_id: "n2".into(), addr: "n2".into() },
            ],
            new: vec![
                ServerId { node_id: "n1".into(), addr: "n1".into() },
                ServerId { node_id: "n2".into(), addr: "n2".into() },
                ServerId { node_id: "n3".into(), addr: "n3".into() },
            ],
        };
        let simple = Configuration::Simple(vec![
            ServerId { node_id: "n1".into(), addr: "n1".into() },
            ServerId { node_id: "n2".into(), addr: "n2".into() },
            ServerId { node_id: "n3".into(), addr: "n3".into() },
        ]);
        node.log.push(LogEntry {
            term: 1, index: 1,
            command: Command::InstallConfiguration { config: joint.clone() },
        });
        node.log.push(LogEntry {
            term: 1, index: 2,
            command: Command::InstallConfiguration { config: simple.clone() },
        });
        node.replay_logs();
        // Final config should be Simple (the second entry overwrites the Joint).
        assert_eq!(node.current_config, simple);
        assert_eq!(node.peers, vec!["n2".to_string(), "n3".to_string()]);
    }

    // -----------------------------------------------------------------
    // handle_join_cluster tests (P8 PR 6a)
    // -----------------------------------------------------------------

    /// Helper: build a fresh `RaftNode` with a 3-node Simple config
    /// (n1=self, n2/n3=peers) and drive it to Leader at term 1.
    fn make_join_cluster_leader() -> RaftNode {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join("n1.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("n1_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("n1_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join("n1_sm");
        let sm_config = crate::state_machine::StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
        let mut node = RaftNode::new_with_storage(
            "n1".to_string(),
            vec!["n2".to_string(), "n3".to_string()],
            sm,
            storage,
        );
        node.state = NodeState::Leader;
        node.current_term = 1;
        node
    }

    #[test]
    fn handle_join_cluster_accepts_candidate_on_leader_and_returns_peer_list() {
        let node = make_join_cluster_leader();
        let resp = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n4".into(),
        });
        assert!(resp.accepted, "expected accept; reason={}", resp.reason);
        assert_eq!(resp.term, 1);
        assert_eq!(resp.leader_addr, "n1");
        // Peer list should be all_servers \ {self, candidate} =
        // {n1,n2,n3} \ {n1, n4} = {n2, n3}.
        let mut got = resp.peer_addrs.clone();
        got.sort();
        assert_eq!(got, vec!["n2".to_string(), "n3".to_string()]);
        assert!(resp.reason.is_empty());
    }

    #[test]
    fn handle_join_cluster_rejects_when_not_leader() {
        let mut node = make_join_cluster_leader();
        node.state = NodeState::Follower;
        let resp = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n4".into(),
        });
        assert!(!resp.accepted);
        assert_eq!(resp.term, 1);
        assert_eq!(resp.reason, "not leader");
        assert!(resp.peer_addrs.is_empty());
    }

    #[test]
    fn handle_join_cluster_rejects_when_not_leader_candidate_too() {
        let mut node = make_join_cluster_leader();
        node.state = NodeState::Candidate;
        let resp = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n4".into(),
        });
        assert!(!resp.accepted);
        assert_eq!(resp.reason, "not leader");
    }

    #[test]
    fn handle_join_cluster_rejects_candidate_addr_equal_to_leader_addr() {
        let node = make_join_cluster_leader();
        // Hint landed on the candidate itself (e.g. candidate dialed
        // its own address via a stale DNS round-robin).
        let resp = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n1".into(),
        });
        assert!(!resp.accepted);
        assert_eq!(resp.reason, "candidate_addr is the leader itself");
        assert!(resp.peer_addrs.is_empty());
    }

    #[test]
    fn handle_join_cluster_rejects_candidate_addr_already_member() {
        let mut node = make_join_cluster_leader();
        // Idempotent retry safety net: n2 is already in the cluster.
        let resp = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n2".into(),
        });
        assert!(!resp.accepted);
        assert_eq!(resp.reason, "candidate_addr is already a cluster member");
        assert!(resp.peer_addrs.is_empty());
    }

    #[test]
    fn handle_join_cluster_returns_term_even_when_rejected() {
        let mut node = make_join_cluster_leader();
        node.current_term = 7;
        node.state = NodeState::Follower;
        let resp = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n4".into(),
        });
        assert!(!resp.accepted);
        assert_eq!(resp.term, 7);
        assert_eq!(resp.leader_addr, "n1");
    }

    #[test]
    fn handle_join_cluster_works_under_joint_config_too() {
        let mut node = make_join_cluster_leader();
        // Install a Joint { old, new } config that includes n1,n2,n3
        // as old and n1,n2,n3,n4 as new — i.e. mid-flight AddNode.
        let joint = Configuration::Joint {
            old: vec![
                ServerId { node_id: "n1".into(), addr: "n1".into() },
                ServerId { node_id: "n2".into(), addr: "n2".into() },
                ServerId { node_id: "n3".into(), addr: "n3".into() },
            ],
            new: vec![
                ServerId { node_id: "n1".into(), addr: "n1".into() },
                ServerId { node_id: "n2".into(), addr: "n2".into() },
                ServerId { node_id: "n3".into(), addr: "n3".into() },
                ServerId { node_id: "n4".into(), addr: "n4".into() },
            ],
        };
        node.install_configuration(&joint);
        // n4 is in new, so a fresh JoinCluster for n5 must reject
        // (n4 is also a member, so a candidate picking n4's addr must
        // be rejected for that).
        let resp_reject = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n4".into(),
        });
        assert!(!resp_reject.accepted);
        assert_eq!(resp_reject.reason, "candidate_addr is already a cluster member");
        // A genuinely-new candidate (n5) is accepted; peer list is
        // all_servers \ {self, candidate} = {n1,n2,n3,n4} \ {n1,n5}
        // = {n2,n3,n4}.
        let resp_accept = node.handle_join_cluster(&JoinClusterRequest {
            candidate_addr: "n5".into(),
        });
        assert!(resp_accept.accepted);
        let mut got = resp_accept.peer_addrs.clone();
        got.sort();
        assert_eq!(
            got,
            vec!["n2".to_string(), "n3".to_string(), "n4".to_string()]
        );
    }

    // ---- P8 PR 7: tx timeout + admin-driven abort ----

    /// Helper: build a single-node leader with `tx_id` already in
    /// `pending_txs`. Used by the `propose_abort_tx` unit tests
    /// below so each test starts from a known state.
    fn make_leader_with_pending_tx(tx_id: &str) -> RaftNode {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir
            .path()
            .join(format!("{tx_id}.wal"))
            .to_str()
            .unwrap()
            .to_string();
        let meta = dir
            .path()
            .join(format!("{tx_id}_meta.json"))
            .to_str()
            .unwrap()
            .to_string();
        let snap = dir
            .path()
            .join(format!("{tx_id}_snapshot.json"))
            .to_str()
            .unwrap()
            .to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join(format!("{tx_id}_sm"));
        let sm_config = StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(StateMachine::open(sm_config).unwrap()));
        let mut node = RaftNode::new_with_storage(
            tx_id.to_string(),
            vec![],
            sm.clone(),
            storage,
        );
        node.state = NodeState::Leader;
        // Seed pending_txs via apply_logs path so the test mirrors
        // what happens after a BeginTx log entry commits.
        node.log.push(crate::protocol::LogEntry {
            term: 1,
            index: 1,
            command: Command::BeginTx {
                tx_id: tx_id.to_string(),
                ops: vec![crate::protocol::TxOp::Put {
                    key: "k".into(),
                    value: "v".into(),
                }],
            },
        });
        node.commit_index = 1;
        node.apply_logs();
        // Suppress unused warning for dir (TempDir keeps the dir alive
        // only via its handle; the closure below converts it into a
        // 'static via Box::leak so the spawned node doesn't try to
        // access a dropped dir).
        std::mem::forget(dir);
        node
    }

    #[test]
    fn propose_abort_tx_on_leader_appends_decide_tx_abort_entry() {
        // Happy path: leader with a pending tx proposes
        // DecideTx(Abort), which appends to log and returns the new
        // index. The pending entry should still be in pending_txs
        // (apply_logs purges it once DecideTx commits).
        let mut node = make_leader_with_pending_tx("tx-happy");
        assert!(node.state_machine.read().unwrap().is_tx_pending("tx-happy"));
        let log_len_before = node.log.len();

        let decide_index = node
            .propose_abort_tx("tx-happy")
            .expect("leader should accept abort");

        // The DecideTx entry was appended.
        assert_eq!(node.log.len(), log_len_before + 1);
        assert_eq!(node.log.last().unwrap().index, decide_index as usize);
        match &node.log.last().unwrap().command {
            Command::DecideTx { tx_id, decision } => {
                assert_eq!(tx_id, "tx-happy");
                assert_eq!(*decision, TxDecision::Abort);
            }
            other => panic!("expected DecideTx, got {:?}", other),
        }
    }

    #[test]
    fn propose_abort_tx_rejects_non_leader() {
        // Same as the happy path but state != Leader. The error must
        // be NotLeader so the JSON dispatch guard and the operator
        // both see the right code.
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join("nl.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("nl_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("nl_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join("nl_sm");
        let sm_config = StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(
            StateMachine::open(sm_config).unwrap(),
        ));
        let mut node =
            RaftNode::new_with_storage("nl".into(), vec![], sm.clone(), storage);
        node.state = NodeState::Follower;
        // Seed a pending tx so the NotLeader check fires before the
        // NotFound check.
        sm.write()
            .unwrap()
            .begin_tx("nl-tx".into(), vec![crate::protocol::TxOp::Put {
                key: "k".into(),
                value: "v".into(),
            }])
            .unwrap();
        let err = node.propose_abort_tx("nl-tx").unwrap_err();
        assert_eq!(err, AbortTxError::NotLeader);
        std::mem::forget(dir);
    }

    #[test]
    fn propose_abort_tx_returns_not_found_for_unknown_tx_id() {
        // tx_id is NOT in pending_txs → NotFound. This protects
        // against the operator typo / re-trying an already-aborted
        // tx from spawning a confusing empty DecideTx entry.
        let mut node = make_leader_with_pending_tx("alive");
        let err = node
            .propose_abort_tx("ghost")
            .unwrap_err();
        assert_eq!(
            err,
            AbortTxError::NotFound("ghost".to_string())
        );
    }

    #[test]
    fn pending_txs_older_than_through_node_uses_state_machine_view() {
        // Integration check that `RaftNode::pending_txs_older_than`
        // correctly delegates to the state machine. Seed an old tx
        // via apply_logs and verify the threshold-based filter
        // works.
        let mut node = make_leader_with_pending_tx("node-old");
        // Hand-poke the begin_unix_ms back by 1 second so it's
        // "older than now".
        {
            let mut sm = node.state_machine.write().unwrap();
            let now = crate::state_machine::now_unix_ms();
            // Re-insert with a synthetic old timestamp via the
            // public `begin_tx_at` after wiping the existing entry.
            let view = sm.pending_tx("node-old").unwrap();
            let ops = (0..view.op_count)
                .map(|_| crate::protocol::TxOp::Put {
                    key: "k".into(),
                    value: "v".into(),
                })
                .collect();
            // `decide_tx` purges the pending entry without applying
            // anything (Abort). Then re-insert with the old
            // timestamp.
            sm.decide_tx("node-old", TxDecision::Abort).unwrap();
            sm.begin_tx_at(
                "node-old".into(),
                ops,
                now.saturating_sub(1_000),
            )
            .unwrap();
        }
        let stale =
            node.pending_txs_older_than(std::time::Duration::from_millis(100));
        assert_eq!(stale, vec!["node-old".to_string()]);
    }
}