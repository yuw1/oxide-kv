# Changelog

All notable changes to Oxide-KV will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added (P7 fuzz 2PC coverage)
- The fuzz harness now drives real two-phase-commit rounds.
  New `SubmitTx { tx_id, ops }` action in `tests/raft_fuzz.rs`
  calls the production coordinator (`raft::coordinator::
  coordinate_tx`) through a new test bridge
  `SimCluster::run_tx`, so every scenario exercises `BeginTx`
  → vote fan-out → `DecideTx` under partitions and node kills.
  This is what actually feeds the 2PC-atomicity invariant
  (`check_2pc_atomicity`) — previously it was wired into
  `assert_invariants` but never saw a transaction. Action
  distribution is now 30% plain ops, 12% 2PC tx, 18%
  kill/restart, 18% partition, 12% election, 10% yield.
- `SimCluster::run_tx(leader_idx, tx_id, ops, bound)` — a
  bounded, cancellable bridge to the `pub(crate)` coordinator
  so integration tests can drive 2PC. The bound is applied to
  the `coordinate_tx` future directly (not a spawned handle),
  so a timeout *cancels* the round rather than detaching it —
  no background coordinator can land a late `DecideTx` after a
  caller's cross-check.
- `ReferenceModel` now models 2PC: `BeginTx` stages ops
  (invisible to reads), `DecideTx(Commit)` applies them
  atomically, `DecideTx(Abort)` discards them — mirroring
  `StateMachine::begin_tx` / `decide_tx` exactly. Without this,
  the leader-vs-reference-model cross-check would false-positive
  on any committed transaction. +6 unit tests for the tx paths.

### Changed (P7 fuzz 2PC coverage)
- `ReferenceModel` gains a `pending` map for staged
  transactions; `reset()` clears it alongside `state`.

### Added (P7 fuzz harness)
- New `tests/raft_fuzz.rs` (1 integration test file, 5 test
  functions, ~750 lines): seeded-RNG scenario fuzz with three
  cross-check oracles (invariants + reference model + per-node
  log consistency). Action vocabulary: `SubmitSet`,
  `SubmitDelete`, `DriveElection`, `KillNode`, `RestartNode`,
  `PartitionLink`, `HealPartitions`, `Yield`. Distribution:
  40% ops, 20% kill/restart, 20% partition, 10% election,
  10% yield.
- 5 fuzz tests:
  - `fuzz_default_seeds_0_to_200` — 200 scenarios × 25
    actions, seeds 0..200.
  - `fuzz_default_seeds_1000_to_1200` — 200 scenarios ×
    25 actions, seeds 1000..1200.
  - `fuzz_long_seeds_2000_to_2100` — 100 scenarios ×
    50 actions, seeds 2000..2100.
  - `fuzz_short_seeds_3000_to_3100` — 100 scenarios ×
    5 actions, seeds 3000..3100.
  - `fuzz_smoke_single_seed` — seed 42 with 0 actions
    (sanity check).
- Each scenario has a 15s wall-clock deadline; each scenario
  gets a fresh `SimCluster` so failures are isolated.

### Fixed (P7 fuzz harness)
- `RaftNode::run_heartbeat_loop` was running forever even
  after `kill_node`. Each killed node kept its heartbeat
  loop alive and continued to spam `rpc timeout after 2s`
  errors against unreachable peers, slowing the fuzz
  harness by 100×+ (a single scenario could take 30s
  instead of 1s). The heartbeat loop now observes a
  `StopSignal` and exits cleanly when the node is
  killed. All four call sites updated
  (`src/main.rs`, `tests/integration_2pc.rs`, two in
  `src/raft/sim_harness.rs`).

### Changed (P7 fuzz harness)
- `protocol::LogEntry::term` is now `pub` (was `pub(crate)`).
  Required by the fuzz harness's log-equality cross-check
  which needs to compare `(index, term)` of follower entries
  against the leader's log.
- Reference model cross-check now (a) replays the
  reference model up to the leader's `commit_index` for
  the leader's linearizability oracle, and (b) verifies
  each follower's `applied` prefix is `(index, term)`
  consistent with the leader's log. The previous
  `last_applied <= leader_commit` check was a false
  positive — after a leader change, a follower's
  `last_applied` can transiently exceed the new leader's
  `commit_index` (Raft §5.4.2: a new leader can't commit
  entries from previous terms until it commits at least
  one entry from its own term). What's actually unsafe
  is divergence: a follower applying an entry the leader
  has truncated. The new check catches that without
  flagging transient post-election state.

### Added (P7 reference model + cross-check)
- New `src/raft/reference_model.rs` (170 lines): a sequential
  single-threaded HashMap that applies committed `Set` /
  `Delete` ops in log-index order. Exposes
  `apply(index, &Command)`, `drain_to(&SimCluster,
  commit_index)`, `get(key)`, `snapshot()`, `applied_index()`.
  Treats `Compact` and the 2PC ops as no-ops (those paths are
  covered by other tests; this is the KV linearizability
  oracle).
- 3 new DST cross-check scenarios in `tests/raft_dst.rs`
  (cross-check against the reference model under various
  fault combinations):
  - `dst_reference_model_cross_check_under_faults` —
    3 writes on steady-state leader, partition n2 off,
    write 2 more entries, heal, verify post-heal all 3
    nodes match the reference model.
  - `dst_reference_model_cross_check_after_leader_failover` —
    2 writes, kill n0, drive n1 to leader, verify new
    leader's reads match reference model, then submit
    new write under new leader and cross-check.
  - `dst_reference_model_cross_check_with_delete` —
    submit a `Set`, then a `Delete`, verify every node
    observes the deletion and matches the reference
    model.

### Fixed
- `RaftNode::get_log_entry(index)` had an off-by-one bug:
  it was doing `self.log.get(index as usize)`, but the log
  array is 0-indexed in storage (`log[0]` = Raft-log
  index 1). The bug was latent because the only existing
  caller (`coordinator.rs`) always passed `index = log.len()`
  (one past the last entry), which is out-of-bounds so the
  function returned `None` and the caller fell back to
  `current_term()`. With this fix, callers can now actually
  read the entry at a given log index. This is the
  correctness-required for the reference model's
  `drain_to` path. Verified: `tests/integration_2pc.rs`
  (3 tests) still pass with the fix.

### Added (P7 fault coverage: delay / reorder / duplicate / restart)
- Real `Delay(d)` outcome in `SimTransport::send_raft` and
  `send_vote`: the transport now `tokio::time::sleep`s for `d`
  before pushing the message, instead of collapsing `Delay` to
  `Deliver`. If the sender's `rpc_timeout` fires first, the
  message still arrives at the receiver later — modelling the
  classic "slow link" scenario in Raft testing. (Virtual-clock
  alignment of the delay is deferred; today the delay is
  wall-clock so a Delay > rpc_timeout matches the documented
  semantics.)
- New `RandomDelay<R>` scheduler: each outbound message is
  delayed by `delay` with probability `p_delay`, otherwise
  delivered immediately. `R` is a `FnMut() -> f64` so tests
  can use a seeded RNG for deterministic replay.
- New `ScheduleOutcome::Duplicate(Duration)` outcome: transport
  pushes the message body once immediately, then again after
  the configured spacing (on a separate spawned task so the
  sender's RPC isn't held open). Models packet duplication on
  a lossy link.
- New `DuplicateAll { delay }` scheduler: every message gets
  `ScheduleOutcome::Duplicate(delay)`. Useful for verifying
  that idempotent RPC handlers handle duplicates correctly
  (Raft consensus is by construction idempotent for
  AppendEntries / RequestVote; this is the test surface for
  asserting that).
- `InboundMessageBody` now derives `Clone` (needed for the
  duplicate-path spawn).
- `SimCluster::restart_node(node_idx)` (&mut self): re-spawns
  the killed node's serve + heartbeat loops with a fresh
  `StopSignal`. The RaftNode itself is reused (in-memory state
  preserved) — this is a deliberate simplification of "real
  restart" which would discard and reload from disk; the DST
  doesn't need to verify the reload path (covered by
  `tests/integration_2pc.rs`). Re-registers a fresh inbound
  channel via `Network::re_register` so the sender side has a
  live receiver after the old one was dropped.
- `Network::re_register(node_id)` replaces the (possibly dead)
  inbound sender with a fresh one and returns the matching
  receiver. Used by `restart_node`.
- `SimTransport::replace_inbound(receiver)` swaps in a fresh
  receiver. Panics if a receiver is still installed (i.e. the
  previous serve loop is still running). Callers must
  `stop.stop()` + sleep before calling.
- 6 new unit tests: `random_delay_with_p1_always_delays`,
  `random_delay_with_p0_always_delivers`,
  `random_delay_p_clamps_to_unit_interval`,
  `duplicate_all_returns_duplicate_outcome`,
  `duplicate_all_default_spacing_is_50ms`,
  `sim_harness_kill_then_restart_node_catches_up`.

### Added (P7 invariant checker)
- `src/raft/invariants.rs` (≈480 lines): four safety invariant
  checks that DST scenarios call at teardown to catch any
  latent safety bug, not just the specific behavior under
  test:
  - `check_election_safety` — at most one Leader per term
    (Raft §5.2).
  - `check_log_matching_property` — entries at the same log
    index have the same term across nodes (Raft §5.3).
  - `check_state_machine_safety` — applied entries at the
    same index have the same command across nodes (Raft
    §5.4.2).
  - `check_committed_entry_durability` — at every committed
    index, every node that has applied up through that index
    has the same command (catches log truncation, snapshot
    mis-application).
  - `check_2pc_atomicity` — every DecideTx tx has the same
    decision (all-Commit or all-Abort) across all nodes that
    have it in their log.
- `SimCluster::submit_command(leader_idx, command)` —
  generalization of `submit_set` for `BeginTx` / `DecideTx` /
  `Delete` / `Compact` (DST scenarios need this for tx tests
  without going through the coordinator fast-path).
- All existing DST scenarios (raft_dst + raft_dst_log_conflict)
  now call `assert_invariants(&cluster).expect(...)` at
  teardown. A safety violation surfaces with a precise
  location (which invariant, which node pair, which index or
  tx), not a generic "test X failed somewhere."

### Changed (P7 invariant checker)
- `protocol::LogEntry::command` was `pub(crate)` — invariant
  checker reads `node.log[k].command` directly, so visibility
  is unchanged (still pub(crate) — checker is in-crate).

### Added (P7 DST scenarios: log conflict / divergent log)
- `tests/raft_dst_log_conflict.rs` (5 new integration tests):
  - `dst_split_brain_old_leader_truncates_divergent_log`
    (§5.3 + §5.4): old leader appends uncommitted entries
    during a partition, the other partition elects a new
    leader, after heal the old leader truncates its
    divergent log.
  - `dst_divergent_log_higher_term_wins` (§5.4): two
    partitions each elect a leader; the higher-term leader's
    log survives the heal.
  - `dst_stale_leader_steps_down_and_does_not_apply_uncommitted`
    (§5.2): the old leader steps down on the new leader's
    heartbeat and never applies its uncommitted log entries
    (Raft's commit barrier, not log truncation, is the safety
    mechanism).
  - `dst_minority_isolated_node_catches_up_on_heal` (§5.3):
    a single isolated node cannot make progress; on heal it
    catches up to the majority's committed entries.
  - `dst_no_partition_baseline_converges`: control test —
    no-fault cluster always converges.
- All 5 scenarios pass 20 consecutive runs with timing
  variance < 2% (1.35-1.37s wall-clock).

### Changed
- `protocol::LogEntry::command` changed from `pub(crate)` to
  `pub` so integration tests can assert on log contents
  (e.g. "no node has 'b' after heal"). No internal code
  change — the field was already accessed from many
  in-crate modules; this just relaxes the visibility.

### Added (P7 DST scenarios: leader failover + partition heal)
- `tests/raft_dst.rs` (5 new integration tests):
  - `dst_leader_failover_preserves_committed_log` (§5.2 +
    §5.4.1): leader crashes, surviving follower wins election,
    committed log entries survive in the new leader's log.
  - `dst_leader_failover_then_new_leader_accepts_writes`:
    after failover the new leader accepts fresh writes and
    replicates to the surviving followers.
  - `dst_election_restriction_stale_candidate_loses` (§5.4.1):
    a candidate whose log is stale (last_log_index < peer's)
    cannot win an election.
  - `dst_partition_isolates_leader_minority_wins_then_heal`
    (§5.3): asymmetric partition isolates the old leader,
    the minority partition elects a new leader, after heal
    the old leader catches up via AppendEntries.
  - `dst_leader_failover_repeated_5x_no_state_leak`: 5
    consecutive failover cycles in one process; passes iff
    every iteration ends with the same cluster invariants
    (catches state leaks between iterations).
- All 5 scenarios pass 20 consecutive runs with timing
  variance < 1% (5.36-5.44s wall-clock) — the in-process
  SimTransport + SimClock combination is deterministic in
  practice, not just in theory.

### Added (P7 SimHarness helpers used by the DST tests)
- `SimCluster::kill_node(idx)`: stop one node's serve loop
  and heartbeat loop, force its state to Follower (simulates
  "node crashed and the cluster noticed").
- `SimCluster::current_term(idx)`: read a node's
  `current_term` for assertions about term advancement.
- `SimCluster::try_drive_election(idx, timeout)`: like
  `drive_election` but returns `bool` instead of panicking
  when the candidate loses — needed for
  "candidate _should_ lose" tests.
- `SimCluster::wait_for_replication_except(target, &excluded,
  timeout)`: like `wait_for_replication` but skips a list of
  node indices — needed for "killed node never advances
  last_applied" scenarios.
- `SimCluster::wait_for_replication` is now a thin shim over
  `wait_for_replication_except(target, &[], timeout)`.

### Added (P7 foundation: SimHarness)
- `crate::raft::sim_harness` module (~480 lines, 6 tests):
  - `SimCluster` — a 3-node (or N-node) in-process cluster wired
    against `SimTransport` + `SimClock`. Each node owns a
    `tempfile::tempdir` (WAL/meta/snapshot), a `StopSignal`, and
    a spawned heartbeat loop. No real sockets, no real OS
    scheduling, no real disk I/O.
  - `SimCluster::new_3_nodes(scheduler)` / `new_n_nodes(n, scheduler)`
    — construct the cluster with a shared `FaultScheduler`.
  - `drive_election(candidate_idx)` — force a candidate via
    `become_candidate` (skips the 5-10s randomized timer) and
    poll until the candidate wins.
  - `submit_set(leader_idx, key, value)` — propose a `Set`
    command on the leader (returns the log index).
  - `wait_for_replication(target_index, timeout)` — poll until
    every node has applied `target_index`.
  - `read(node_idx, key)` — read from a node's local state
    machine.
  - `shutdown()` — stop all serve loops and heartbeat loops.
- `SimTransport` now implements `Clone` (via
  `Arc<Mutex<Option<Receiver>>>` on the inbound channel).
- `tempfile` moved from `[dev-dependencies]` to `[dependencies]`
  so the harness can live in the library (reusable from any
  `tests/*.rs`).

### Fixed
- Single-node cluster election: `become_candidate` on a node
  with zero peers never transitioned to Leader because the
  majority check only ran inside the per-peer RPC callback.
  Added an early return in `request_votes` when
  `total_nodes == 1` (self-vote is already a majority).
  Discovered by `sim_harness_1_node_cluster_is_immediately_leader`.

### Added (P7 foundation: FaultScheduler)
- `crate::raft::fault_scheduler` module (~530 lines, including
  7 new tests):
  - `FaultScheduler` trait (object-safe, `Send + Sync + 'static`)
    with a `before_send(link, body) -> ScheduleOutcome` async
    hook. Consulted by `SimTransport::send_raft` /
    `send_vote` on every outbound message.
  - `ScheduleOutcome::{Deliver, Drop, Delay(Duration)}` —
    collapse-able to a single async future via
    `Pin<Box<dyn Future + Send>>`. `Delay` is accepted by the
    trait but the current `SimTransport` collapses it to
    `Deliver` (proper delay handling is the next PR's job —
    integrating it requires replacing `tokio::time::timeout`
    on the sender side with `Clock::sleep`).
  - `LinkId { from, to }` — a directed link identifier.
    Symmetric failures (n1->n2 dropped, n2->n1 delivered) are
    a first-class concept because real network partitions are
    asymmetric.
  - `AlwaysDeliver` — passthrough (zero-config default).
  - `DropLink { from, to }` — drop a single directed link.
  - `RandomDrop<R: FnMut() -> f64>` — drop a fraction of
    messages per a seeded RNG closure. Deterministic given a
    seeded RNG. The harness chooses the threshold by biasing
    the RNG's output.
  - `PartitionedNetwork` — a set of partitioned directed
    links, mutable via `partition(...)` / `heal()`. The
    classic "minority can't elect" Raft correctness test
    pattern: `partition(n1->n2)` + `partition(n1->n3)` leaves
    n1 isolated, so n1's election can't reach quorum.
  - `DropUnless<F: Fn(&LinkId, &InboundMessageBody) -> bool>` —
    drop messages matching a predicate. Useful for "drop all
    heartbeats, deliver everything else".
  - `Network::with_scheduler(rpc_timeout, scheduler)` — build
    a network that consults a custom scheduler on every
    outbound message. The default `Network::new()` /
    `with_rpc_timeout` still use `AlwaysDeliver`.

### Test count
- 166 -> 173 passing (7 new fault_scheduler tests). Zero
  regressions.
- `cargo clippy --release -- -D warnings`: 25 errors before,
  25 errors after. All in pre-existing master files. Zero new
  lint debt.
- 8 sim_transport tests (PR #20) and 3 clock tests (PR #19)
  continue to pass unchanged.

### Out of scope (next PR)
- `ScheduleOutcome::Delay(d)` is currently collapsed to
  `Deliver` by `SimTransport`. Proper delay handling requires
  integrating `Clock::sleep` into the sender-side wait so the
  delay runs on virtual time under `start_paused`. The public
  surface (the trait, the enum variant) is already in place;
  the next PR just plumbs the wait.

### Added (P7 foundation: SimTransport)
- `crate::raft::sim_transport::SimTransport` + `Network` — an
  in-memory `Transport` impl that routes `RaftMessage`s through
  `tokio::sync::mpsc` channels keyed by node id, replacing the
  real TCP path. Future DST (deterministic simulation testing)
  harnesses will drive consensus against this without needing
  real sockets.
- `Network::new()` / `with_rpc_timeout()` — cluster topology
  registry. `Network::register(node_id)` allocates an inbound
  channel; `SimTransport::send_raft` / `send_vote` look up the
  target's inbound channel via the shared `Network`.
- `InboundMessageBody` enum with three variants
  (`Raft(RaftMessage)`, `Vote(VoteRequest)`, `VoteReply(VoteResponse)`).
  This separates the 2PC vote dispatch path from the Raft
  consensus path so a vote request correctly reaches
  `RaftNode::handle_tx_vote_request` (not
  `handle_request_vote`, which has superficially similar fields
  but different semantics — election restriction vs
  pending_txs membership).
- `pub(crate) fn dispatch_raft_message(...)` — extracted
  request -> reply dispatch for the three Raft RPC handlers
  (RequestVote / AppendEntries / InstallSnapshot). Sync (the
  underlying RaftNode handlers are sync). The future fault
  scheduler hook will wrap this with delay / drop / partition.
- 8 new unit tests:
    - 2 error-classification tests (unknown peer => Unreachable,
      inbound reply variant => Protocol).
    - 3 dispatch-routing tests (RequestVote / AppendEntries /
      InstallSnapshot each dispatch to the correct handler and
      return the correct reply variant).
    - 2 end-to-end round-trip tests (RequestVote / AppendEntries
      via `send_raft` + `serve`).
    - 1 end-to-end vote test (VoteRequest via `send_vote` +
      `serve` returns VoteResponse from `handle_tx_vote_request`).

### Added (P7 foundation: SimClock)
- `crate::raft::clock::SimClock` — a virtual clock that returns
  `epoch + virtual_offset` from `now()`, so future
  deterministic-simulation tests can drive the consensus hot path
  under `tokio::time::pause()` + controlled advance without
  burning real wall-clock seconds.
- `SimClock::with_epoch(Instant)` lets multiple clocks share a
  common epoch so their `now()` sequences are directly
  comparable; `SimClock::new()` is the default (captures
  `Instant::now()` at construction).
- `SimClock::virtual_offset()` exposes the cumulative virtual
  duration for test assertions.
- Dev-dependency: `tokio = { ..., features = ["full", "test-util"] }`
  so tests can use `#[tokio::test(start_paused = true)]`. Prod
  build is unaffected.
- 3 new unit tests:
    - `sim_clock_is_deterministic_under_same_seed`: two clocks with
      a shared epoch see identical `now()` for the same advance
      schedule.
    - `sim_clock_advance_moves_virtual_time_without_wall_clock`:
      under `start_paused`, real wall clock doesn't move but the
      virtual offset does.
    - `sim_clock_supports_elapsed_style_deadline_checks`: stamp +
      advance + `duration_since` works (mirrors how
      `last_quorum_heartbeat_at` will be tested in DST).

### Added (P7 foundation: transport abstraction)
- New `crate::raft::net::Transport` trait + `TcpTransport` real impl
  (message-level, not byte-level).
- New `StopSignal` (`Arc<tokio::sync::Notify>`) for clean listener
  shutdown; `main.rs` wires `ctrl_c` into it.
- `RaftNode::new_with_clock_and_transport(...)` constructor for
  injecting both abstractions; `new` / `new_with_storage` /
  `new_with_clock` default to `system_transport()` (send-only TCP).
- 4 production RPC paths now route through the trait:
  - `sync_logs` → `AppendEntries` per peer
  - `request_votes` → `RequestVote` per peer
  - `coordinate_tx` → 2PC `VoteRequest` fan-out (timeout still
    applied at the call site via `TX_VOTE_TIMEOUT_MS`)
  - `main.rs` listener loop → `serve(...)` with `StopSignal`
- `RpcClient::call` is now `pub(crate)` so `TcpTransport::send_raft`
  can route every Raft variant through one helper. The per-variant
  `send_request_vote_rpc` / `send_append_entries_rpc` helpers are
  removed (callers now pattern-match on the `RaftMessage` reply at
  the call site).
- 4 new transport unit tests: error classification (`Unreachable` /
  `Timeout` / `Protocol`), `StopSignal` roundtrip,
  `serve_without_listener` fails fast.

### Added (P7 foundation: clock abstraction)
- New `crate::raft::clock::Clock` trait + `SystemClock` production impl.
- `RaftNode::new_with_clock(...)` constructor for injecting a custom
  clock; production `new` / `new_with_storage` default to
  `SystemClock`.
- All `last_heartbeat` / `last_quorum_heartbeat_at` /
  `ReadIndex::issued_at` stamps (7 production sites in
  `RaftNode`) now route through `self.clock.now()` instead of
  `Instant::now()`.
- Election timer (`raft::timer::run_election_timer`) and heartbeat
  loop (`RaftNode::run_heartbeat_loop`) now pull the clock from the
  node and use `clock.sleep(d)` instead of `tokio::time::sleep` /
  `tokio::time::interval`, so a future `SimClock` can drive virtual
  time without touching the consensus code.
- 3 new unit tests for `SystemClock` (monotonicity, Arc object
  safety, real-sleep smoke test).

### Changed (roadmap)
- P6 (multi-node 2PC coordinator RPC) marked complete (PR #11–#14 merged).
- P7 pinned as the active phase: **Deterministic simulation testing (DST)**
  — a reproducible fault-injection harness that proves election safety,
  state-machine safety, committed-entry durability, and 2PC atomicity,
  rather than relying on happy-path tests alone. Docs-only this PR.

### Added (project branding)
- Project logo under `assets/` (`logo-light.svg` for light themes,
  `logo-dark.svg` for dark themes)
- README hero: theme-aware logo via `<picture>` (dark/light SVG variants),
  centered title, tagline, and badge row

### Added
- Apache-2.0 LICENSE file
- This CHANGELOG file
- Dev-dependency: `tempfile` for test isolation
- **Snapshot-based log compaction**: `Snapshot` struct, JSON snapshot serialization,
  atomic save/load, WAL rewrite after snapshot (bounded disk usage)
- **InstallSnapshot RPC**: leader can ship a snapshot to lagging followers who are
  too far behind to catch up via AppendEntries; recipient wipes its state machine,
  truncates its log, and resets commit/applied indices
- Leader-side `maybe_snapshot(threshold)` trigger: when the log exceeds the
  threshold and the node is the leader, take a snapshot at the current
  `commit_index` and rewrite the WAL
- State machine helpers `snapshot_data()` and `clear_for_snapshot()` to support
  snapshot serialization and install
- **ReadIndex linearizable reads** (Raft thesis §6.4): `Get` no longer risks
  stale data from a partitioned leader
  - `RaftNode::begin_read(node_arc) -> Option<ReadIndex>` records the
    leader's current `commit_index` and an `issued_at` timestamp, and
    forces a heartbeat round to refresh the leader's quorum proof.
  - `RaftNode::confirm_read(ri) -> bool` enforces three safety conditions:
    still leader, state machine caught up, quorum proof recent and after
    `issued_at`.
  - `last_quorum_heartbeat_at: Option<Instant>` on `RaftNode`, refreshed in
    `sync_logs` on every successful `AppendEntries` reply.
  - Client `Get` handler (`src/client.rs::linearizable_get`) polls
    `confirm_read` for up to 2s before reading the state machine.
- `ReadIndex` value type in `protocol.rs` (in-process token; not serialized).
- **Binary RPC protocol (Protocol Buffers)** for inter-node Raft traffic:
  replaces the previous JSON-over-TCP encoding with a length-prefixed protobuf
  framing on the wire.
  - `proto/raft.proto` schema covers the full Raft RPC surface
    (RequestVote, AppendEntries, InstallSnapshot, plus their replies,
    plus `Command` / `LogEntry` / `Snapshot` payloads).
  - `build.rs` invokes `prost-build` at compile time to generate Rust types
    in `OUT_DIR`; the new `raft/proto` module re-exports them and provides
    `From`/`Into` conversions to/from the in-process domain types in
    `protocol.rs` and `raft/rpc.rs`.
  - `RpcClient::call` and `RpcServer::handle_rpc_logic` now use the
    `read_framed` / `write_framed` helpers to send and receive
    4-byte big-endian length prefixes followed by the protobuf payload.
  - Storage formats (WAL, snapshot file, meta) and the external client
    command protocol keep their existing formats — only the bytes on the
    Raft RPC socket changed.

### Changed
- **Storage engine rewritten as a Log-Structured Merge (LSM) Tree**:
  in-memory memtable (BTreeMap<String, MemEntry>) backed by an append-only
  WAL (`memtable.wal`, JSON lines, fsync per write); on-disk SSTables in
  `data_dir/sst/NNNNNN.sst` (JSON array of sorted entries + sidecar
  `.meta`); automatic flush when memtable bytes exceed threshold;
  manual `compact()` does size-tiered merge; read path traverses
  memtable → SSTables newest to oldest.
- **State machine API changes** (`src/state_machine.rs`):
  - `open()` now takes `StateMachineConfig { data_dir, memtable_size_threshold }`.
  - `get` returns owned `Option<String>` (data may come from disk).
  - `snapshot_data()` returns `io::Result<HashMap<...>>`.
  - `clear_for_snapshot()` returns `io::Result<()>`.
  - `memtable_len()` and `sstable_count()` introspection helpers added.
- Callers updated: `src/main.rs`, `src/raft/node.rs`, all state-machine tests.
- Cleaned up 18 compiler warnings (unused imports, unused variable, unnecessary
  parens, io::Result handling) — now 0 warnings
- `RaftStorage::new_with_paths` now takes a third `snapshot_path` argument
- `RaftNode::new` now delegates to `new_with_storage` for shared construction logic
- Client `Get` handler now goes through the linearizable read path
  (backward-compatible — server-side behavior unchanged for non-leaders)
- `protocol.rs` and `rpc.rs` types gained `Clone` and `PartialEq` derives
  (needed for protobuf round-trip assertions)
- `Cargo.toml`: added `prost = "0.13"` (runtime) and `prost-build = "0.13"`
  (build-time)

### Test suite (93 tests, all passing)
- `state_machine`: set/get/delete/replace lifecycle + auto-flush
  threshold + tombstone survival + manual flush + newest-wins
  + range filtering + compaction + snapshot flatten + clear wipes
  + WAL replay on reopen + SSTable discovery on reopen + empty
  value + unicode round-trip
- `raft::storage`: WAL round-trip, meta round-trip, atomic rename,
  cross-instance durability, snapshot round-trip, snapshot atomic rename,
  WAL truncation semantics
- `raft::node`: §5.4.1 vote safety + election restriction, AppendEntries
  consistency, §5.4.2 commit safety, quorum calculation, propose rules,
  replay/apply, leader init idempotency, install snapshot, leader-side
  snapshot trigger, ReadIndex begin/confirm with safety guards
- `raft::proto`: protobuf encode/decode round-trip for every
  `RaftMessage` variant, payload size sanity check vs JSON
- `raft::rpc`: length-prefixed framing round-trip (small + large +
  EOF + end-to-end client/server on a tokio duplex)

### Notes
- `RaftStorage` and `RaftNode` were refactored to accept explicit paths so
  tests can isolate each scenario in a temp dir without touching global `Config`.
- The ReadIndex implementation tracks *any* successful peer reply as proof
  of liveness. A leader silent longer than `max_election_timeout_ms` will
  fail `confirm_read`, partitioning reads away from a partitioned leader.
  Per-read ack tracking is a future refinement.
- The protobuf RPC cutover is a hard breaking change on the wire format.
  All nodes must upgrade together. Storage formats (WAL / snapshot /
  meta) and the external client JSON API are unchanged.
- LSM deferred: bloom filters, block cache, background compaction thread,
  leveled compaction. Current shape keeps the engine simple and in one file.

### Added (two-phase commit)
- **Two-phase commit (2PC) lifecycle** for atomic multi-key writes:
  - New `Command` variants: `BeginTx { tx_id, ops }`, `Vote { tx_id, voter, vote }`,
    `DecideTx { tx_id, decision }`
  - New types: `TxOp` (Put/Delete), `Vote` (Yes/No(reason)),
    `TxDecision` (Commit/Abort)
- **State machine 2PC support** (`src/state_machine.rs`):
  - `pending_txs: BTreeMap<tx_id, PendingTx>` tracks in-flight transactions
    (ops + per-participant votes + decision)
  - `begin_tx`, `record_vote`, `decide_tx` apply log entries
  - `pending_tx_count()` and `pending_tx(tx_id) -> PendingTxView` for
    introspection (tests + future admin endpoints)
  - `get` continues to return only committed values — pending ops are
    isolated until `DecideTx(Commit)` applies them
  - `clear_for_snapshot` also wipes in-flight transactions (consistent
    with "snapshot installed -> start fresh")
- **Coordinator fast path** (`src/client.rs::begin_tx`):
  - Single-node cluster auto-appends `DecideTx(Commit)` right after
    `BeginTx`, so the client sees atomic tx with no extra round-trip
  - Multi-node cluster would instead solicit votes via `Vote` entries
    and append `DecideTx` once all votes are in (RPC plumbing
    deferred — the commands are in the wire schema and ready)
- **RaftNode**:
  - `apply_logs` and `replay_logs` handle all three new commands
  - `propose_batch(Vec<Command>)` appends multiple log entries
    contiguously so BeginTx + DecideTx ride the same proposal
- **Wire schema** (`proto/raft.proto` + `src/raft/proto.rs`):
  - New protobuf messages: `BeginTx`, `TxVote`, `DecideTx`, `TxOp`
  - Full round-trip conversion to/from domain types

### Test suite (105 tests, all passing)
- 93 → 105 (+12 new):
  - `state_machine`: begin_tx stages pending without applying,
    decide_tx commit applies all ops atomically, decide_tx abort
    discards all ops, record_vote updates view, vote for unknown
    tx is noop, decide_tx for unknown tx is noop, multiple
    concurrent transactions isolate reads, begin_tx redefine
    overwrites pending state
  - `raft::node`: replay_logs applies committed tx, replay_logs
    aborted tx has no side effects, propose_batch appends contiguous
    entries, vote recorded for pending tx then commit applies ops

### Notes / Caveats
- Single-node cluster: `BeginTx` from a client is automatically paired
  with a `DecideTx(Commit)` log entry on the same Raft proposal.
- Multi-node cluster: the commands (`Vote`, `DecideTx`) are in the wire
  schema and accepted by the state machine, but the coordinator logic
  to gather votes via RPC is intentionally deferred. The state machine
  itself is fully tested.
- Read isolation is best-effort: while a tx is pending, `get` returns
  only the previously-committed value. We do not implement
  two-phase locking, so concurrent transactions on the same key can
  race; last writer wins on commit.
- No deadlock detection / timeout-based abort. A coordinator crash
  after `BeginTx` would leave a pending tx in the log forever;
  follow-up work could add a tx timeout + admin-driven abort.
- Chunked InstallSnapshot transfer (offset/done fields) is still deferred.

### Fixed (election timer & heartbeat tuning)
- **Election timer brain-split**: `run_election_timer` previously compared
  `last_heartbeat.elapsed()` against the *sleep duration* of the loop
  iteration, which was always true after waking up. The result was an
  election triggered on every loop tick, term inflation, and constant
  leader churn in multi-node clusters. The timer now compares against
  a fixed `min_election_timeout_ms` threshold via the new pure helper
  `should_start_election(state, last_heartbeat, now, threshold)`, and
  re-checks state after a small post-threshold jitter to avoid split
  votes between followers who expired in the same tick.
- **Heartbeat-to-election ratio**: bumped `heartbeat_interval_ms` from
  `1000` to `250`, and widened `min_election_timeout_ms` /
  `max_election_timeout_ms` to `5000` / `10000`. The new ratio
  (~1:20–40) tolerates transient RPC jitter without spurious elections
  and brings the spread (max − min) well under 50% of min so two nodes
  rarely draw overlapping timeout windows.
- New unit tests for `raft::timer`:
  leader never elects even after long silence; follower within threshold
  does not elect; follower at/past threshold elects; candidate uses the
  same threshold; the heartbeat:election ratio constraint holds.

### Fixed (single-node read fallback)
- **`Get` on a single-node cluster no longer times out.** Previously, a
  leader with no peers would call `begin_read` (which triggers a
  heartbeat), then `confirm_read` would loop forever waiting for a
  quorum heartbeat ack that could never arrive. The 2 s timeout then
  fired and every `Get` returned `"read confirmation timeout"`. The
  client path now detects `peers.is_empty()` and reads directly from
  the state machine after `apply_logs()`, preserving the linearizable
  guarantee (the node is Leader, commit_index is up-to-date, last_applied
  catches up before the read).
- **Single-node leader now actually advances `commit_index`.** The
  previous `sync_logs` only ran `maybe_commit` from inside the
  per-peer AppendEntries success handler. With zero peers the handler
  never ran, so the leader's `commit_index` stayed at 0 forever and
  mutations were never visible to subsequent reads. `sync_logs` now
  short-circuits on `peers.is_empty()` and runs `maybe_commit` directly
  before returning.
- New `RaftNode::is_single_node()` accessor for tests / callers that
  need the same predicate without exposing the private `peers` field.
- `RaftNode::sync_logs` is now safe to call on a node with an empty
  peer list (was a no-op before for commit advancement).
- New unit tests for the client fast path (`src/client.rs::tests`):
  `is_single_node_true_when_peers_empty`,
  `is_single_node_false_when_peers_present`,
  `linearizable_get_returns_value_on_single_node_without_timeout`,
  `linearizable_get_returns_not_found_for_missing_key_on_single_node`,
  `linearizable_get_rejects_non_leader_on_single_node`.
- `client::dispatch_command` and `client::linearizable_get` are now
  `pub` (was private) to support unit testing from the same crate
  without an integration harness. No external API change.

### Added (P6 — 2PC coordinator wire schema, PR #11)
- New proto file `proto/coordination.proto` (package `oxide_kv.coordination`)
  defining the 2PC coordinator side-channel RPC:
  - `VoteRequest { term, tx_id, last_log_index, last_log_term }` — leader →
    peer "please vote on this tx".
  - `VoteResponse { term, vote_granted, reason }` — peer → leader "here is my
    vote", with a short diagnostic on rejection.
- `build.rs` now compiles both `proto/raft.proto` and `proto/coordination.proto`.
- New module `src/coordination.rs` with generated `pb` types, plain
  domain types (`VoteRequest`, `VoteResponse`), and `From`/`Into`
  conversions in both directions.
- 9 new unit tests covering: typed round-trip ×2, length-delimited
  wire-format round-trip ×2, boundary values (term=0 / empty tx_id,
  `last_log_index = u64::MAX`, empty reason) ×3, and forward-compat
  (reader ignores unknown field tag 99 with multi-byte varint encoding) ×2.

### Changed / Breaking (P6 — PR #11)
- **Removed `Command::Vote` variant.** With the P6 coordinator design
  (side-channel RPC for votes, log carries only `BeginTx` + `DecideTx`),
  the log-side `Vote` entry has no purpose and is deleted from
  `protocol.rs`. The in-memory `Vote` enum (`Yes` / `No(reason)`) is
  retained as the internal representation inside
  `StateMachine::pending_txs`.
- **Removed `pb::TxVote` message and `Command::Body::TxVote = 6` body
  variant** from `proto/raft.proto`. Tag 6 inside the `Command.oneof`
  is left empty (proto3 does not allow `reserved` inside a `oneof`,
  so we rely on omission). The inverse conversion in
  `src/raft/proto.rs` silently treats a stale `TxVote` body as
  `Command::Compact` for forward-compat with any in-flight peer
  running the pre-P6 build.
- **Removed `raft::node::replay_logs` arm for `Command::Vote`** and
  the `vote_recorded_for_pending_tx_then_commit_applies_ops` test
  (the BeginTx + DecideTx replay path is still covered by
  `replay_logs_applies_committed_tx`).
- **Removed `client::dispatch_command` arm for `Command::Vote`.**
  External clients can no longer inject raw `Vote` JSON commands;
  vote flow is now internal-only via the new RPC.

### Test suite (124 tests, all passing)
- 9 new in `coordination::tests`; 1 removed in `raft::node::tests`
  (replaced by `replay_logs_applies_committed_tx` +
  `state_machine::tests::record_vote_updates_pending_tx_view`). Net
  +8 tests vs. PR #10 baseline (116 → 124).

### Added (P6 — 2PC coordinator RPC transport, PR #12)
- **Multiplexed inter-node transport** (`src/raft/transport.rs`):
  the existing Raft TCP listener now carries two RPC surfaces,
  discriminated by a 1-byte prefix:
  - `0x01` — Raft consensus RPCs (RequestVote, AppendEntries,
    InstallSnapshot and their replies) — unchanged semantically.
  - `0x02` — 2PC coordinator vote RPC (`VoteRequest` /
    `VoteResponse`), the side-channel introduced in P6.
  - Wire format: `[kind:u8][length:u32 BE][protobuf payload]`.
    The discriminator keeps the single-port topology the project
    has shipped with since P3 (no second port for vote RPCs);
    rationale recorded in `ROADMAP.md` P6 section.
  - 16 MiB per-frame cap (`MAX_FRAME_BYTES`) so a hostile or
    buggy peer cannot force unbounded memory allocation.
  - `DispatchKind::from_byte` rejects unknown discriminators so
    frames never get silently misrouted.
- **`RpcClient::send_tx_vote_rpc`** (`src/raft/rpc.rs`): client-side
  half of the vote RPC. Connects to a peer, writes a `Vote`
  envelope, and decodes the `VoteResponse`. Carries the
  P6-configurable timeout so the future coordinator (PR #13) can
  drive vote collection without blocking on a slow peer.
- **`RpcServer::dispatch` + `dispatch_on`** (`src/raft/rpc.rs`):
  reads the discriminator and routes each connection to the
  matching handler. The existing `handle_raft_rpc` name is
  preserved as an alias so older call sites and any external
  tooling that imported the symbol keep working.
- **`RaftNode::handle_tx_vote_request`** (`src/raft/node.rs`):
  receiver-side decision for the vote RPC. Implements the all-yes
  2PC policy locked at PR #11: rejects stale terms, adopts newer
  terms but defers the vote to the elected leader of the new
  term, rejects when the `BeginTx` log entry is not yet pending
  locally, mirrors Raft's election-restriction log-up-to-date
  check, and on a grant records a `Vote::Yes` on the local
  state machine so a future `DecideTx(Commit)` can apply the
  operations atomically.

### Changed / Breaking (P6 — PR #12)
- **Wire format on the inter-node listener is now prefixed with a
  1-byte discriminator.** Pre-P6 peers sending `[length:4][payload]`
  frames will be rejected as `unknown protocol discriminator`.
  This is acceptable because no inter-node deployment exists
  yet (P6 is the first phase to drive multi-node behavior) and
  the cutover was performed as a single coordinated change in
  this PR.

### Test suite (140 tests, all passing)
- 9 new in `transport::tests` (envelope round-trip, large payload,
  EOF before byte, unknown discriminator, oversized length,
  zero-length discriminator, `from_byte` known/unknown values).
- 5 new in `raft::node::tests` (`handle_tx_vote_request` coverage:
  stale term, term advance, tx not pending, leader log stale,
  grant + state machine `record_vote`).
- 2 new in `raft::rpc::tests` (`vote_rpc_dispatch_roundtrip_on_duplex`,
  `dispatch_on_rejects_unknown_discriminator`).
- Net +16 tests vs. PR #11 baseline (124 → 140). All previous
  124 tests still pass; no regressions.


## Unreleased — PR #13 (2PC coordinator orchestration)

### Added (P6 — PR #13)
- **Leader-side 2PC coordinator** (`src/raft/coordinator.rs`,
  `pub(crate) mod coordinator` registered in `src/raft.rs`):
  - `coordinate_tx(node_arc, tx_id, ops) -> TxOutcome` is the single
    entry point called by `client.rs::begin_tx`. Detects single-node
    vs multi-node membership and drives the appropriate path.
  - `TxOutcome` enum: `Committed { begin_index, decide_index, tx_id }`
    / `Aborted { tx_id, reason }` / `NotLeader { tx_id }`.
  - **Single-node fast path**: propose `BeginTx` + `DecideTx(Commit)`
    as one contiguous batch via the existing `propose_batch`.
  - **Multi-node path** (textbook 2PC, all-yes quorum):
    1. Propose `BeginTx` only, wait until `last_applied >= begin_index`
       on the leader.
    2. Snapshot the leader's `(current_term, peers, node_id,
       begin_log_term)`.
    3. Record the leader's implicit Yes on `pending_txs[tx_id].votes`.
    4. Fan-out `VoteRequest` to every peer concurrently via
       `tokio::spawn` + `RpcClient::send_tx_vote_rpc` with a per-peer
       timeout (2s).
    5. Tally votes — any No / timeout / error / higher-term reply
       flips the decision to `Abort` and may step the leader down to
       Follower if a peer returned a higher term.
    6. Propose `DecideTx(Commit | Abort)`, wait for commit + apply.
  - Wall-clock bound: a single round is capped at 10s. A round that
    exceeds the bound returns `Aborted` with a clear reason.
- **Read-only accessors on `RaftNode`** so the coordinator can read
  membership and identity without taking a mutable lock:
  `node_id()`, `peers()`, `current_term()`, `get_log_entry(index)`.

### Fixed (P6 — PR #13)
- **`apply_logs` now handles `BeginTx` / `DecideTx` / `Get` /
  `Compact`.** Pre-PR-#13, `apply_logs` only matched `Set` and
  `Delete`, so any `BeginTx` / `DecideTx` entry committed in the
  steady state (not via `replay_logs` at startup) was a no-op for
  the state machine. This meant `pending_txs` was never populated
  on a running leader, which would have made the multi-node
  coordinator hang on the first vote (peers reply `tx not pending`).
  The PR also adds explicit apply-side logs for each variant so
  debugging is easier.

### Changed (P6 — PR #13)
- **`client.rs::begin_tx` is now a thin wrapper** that delegates to
  `coordinator::coordinate_tx` and translates `TxOutcome` into JSON.
  The pre-PR-#13 single-node `propose_batch([BeginTx, DecideTx])`
  inline logic moves into the coordinator's `single_node_fast_path`,
  preserving behavior. The JSON response gains `decision`,
  `begin_index`, and `decide_index` fields on commit, and
  `reason` on abort — wire-compatible with clients that already
  parse `status: ok` (new fields are additive).

### Out of scope (deferred)
- 3-node integration test (PR #14).
- Participant-side recovery on coordinator crash.
- Auto-elevation of no-peers node to Leader is unchanged.

### Test suite (144 tests, all passing)
- 4 new tests in `raft::coordinator::tests`:
  - `coordinate_tx_single_node_commits_atomically` — end-to-end
    single-node path through the new coordinator.
  - `apply_logs_applies_begin_tx_and_decide_tx_in_steady_state` —
    regression test for the apply_logs fix.
  - `apply_logs_abort_decision_does_not_apply_ops` — counterpart
    test for the Abort path through `apply_logs`.
  - `tx_outcome_equality_and_debug_smoke` — public enum sanity.
- Net +4 tests vs. PR #12 baseline (140 → 144). All previous
  140 tests still pass; no regressions.

### Pre-existing clippy debt
- `cargo clippy --release -- -D warnings` reports 25 errors, all
  pre-existing on `master`. The PR introduces 0 new clippy warnings
  in any file it modifies (`src/raft/coordinator.rs`,
  `src/raft/node.rs`, `src/raft.rs`, `src/client.rs`).


## Unreleased — PR #14 (3-node 2PC integration tests)

### Added (P6 — PR #14)
- **`tests/integration_2pc.rs`** (NEW file, 3 tests, 5.3 s total):
  - `happy_path_3_nodes_commits_via_quorum` — 3-node cluster, manual
    `become_candidate` leader election, full `BeginTx` round-trip.
    Asserts: client response is `committed`, all three nodes have
    the ops applied, `pending_tx_count == 0` on every node, and the
    committed value is visible on a follower's state machine.
  - `no_vote_from_one_peer_aborts_tx_and_isolates_ops` — same
    cluster, but a phantom log entry is injected on node 2 to
    force the leader-log-stale check in `handle_tx_vote_request`
    step 3 to return No. Asserts: client response is `aborted`,
    the ops are NOT applied on any node, `pending_tx_count == 0`
    on every node after the abort.
  - `one_unreachable_peer_times_out_and_aborts` — 2 nodes + one
    blackhole address (`127.0.0.1:1`). Asserts: client response
    is `aborted`, the round takes at least 1s wall-clock
    (proving the coordinator respects the vote timeout), and the
    ops are NOT applied on the live peer.

### Fixed (P6 — PR #14)
- **Coordinator bug: BeginTx applied on peers AFTER vote RPC fires.**
  The pre-PR-#14 `coordinate_tx` waited for the leader's
  `last_applied >= begin_index` but not for the BeginTx entry to
  be replicated to peers. Worse, even after
  `wait_for_replication(match_index >= begin_index)` was added,
  the peer had the entry in its log but `commit_index` was
  still behind (AppendEntries only advances `commit_index` when
  `args.leader_commit > self.commit_index`, and the first
  AppendEntries carrying the entry has `leader_commit = 0`).
  The vote RPC fired immediately, the peer replied "tx not
  pending" because `apply_logs` was a no-op, and the coordinator
  aborted a transaction that would otherwise have committed.
  - **Fix (peer-side)**: `handle_tx_vote_request` now fast-forwards
    its `commit_index` to `req.last_log_index` (when the entry is
    in the log) and runs `apply_logs` before the `pending_txs`
    lookup. This is safe because the leader has already committed
    the entry (proven by the leader's `match_index >= begin_index`)
    and the entry is in our log (proven by the leader-log-up-to-date
    check above).
- **Test harness: heartbeat loop not running.** The integration
  test bootstrap starts the RPC listener but not the heartbeat
  loop, so peers never receive subsequent AppendEntries with the
  new `commit_index` and never apply `DecideTx`. Fix: spawn
  `RaftNode::run_heartbeat_loop` in `spawn_node`.

### Changed (P6 — PR #14)
- **`RaftNode` gains two small public helpers used by the
  integration test**:
  - `set_peers(peers: Vec<String>)` — mutates the peer list
    after construction so the test can wire membership in two
    phases (allocate listener ports, then connect).
  - `push_log_entry_for_test(index, command)` — pushes a phantom
    log entry to simulate a peer whose log is ahead of the
    leader's. Only used by the no-vote-abort test for fault
    injection. The doc comment explicitly marks it as
    test-only with "production code never calls this".
- **`raft::storage` is now `pub`** (was `pub(crate)`). The
  integration test legitimately needs `RaftStorage::new_with_paths`
  and `StateMachine::open` to wire up a node without depending on
  the global `Config`, which is `OnceLock`-initialized once per
  process.

### Test suite (147 tests, all passing)
- 3 new integration tests in `tests/integration_2pc.rs`:
  - happy path (0.27s)
  - no-vote abort (0.31s)
  - timeout abort (5.04s, dominated by the 5s
    `wait_for_replication` bound)
- Net +3 tests vs. PR #13 baseline (144 → 147). All previous
  144 tests still pass; no regressions.
- Total time for the integration test suite: ~5.3s.

### Pre-existing clippy debt
- `cargo clippy --release -- -D warnings` reports 25 errors, all
  pre-existing on `master`. The PR introduces 0 new clippy
  warnings in any file it modifies (`src/raft/coordinator.rs`,
  `src/raft/node.rs`, `src/raft.rs`, `tests/integration_2pc.rs`).

### Out of scope (deferred)
- Leader-step-down mid-round fault injection — covered by the
  coordinator's `NotLeader` return path unit test, but not by
  an integration test.
- 5-node cluster test — not needed yet; the coordinator's
  all-yes quorum logic is the same as 3-node.
- `BeginTx` after a leader change (new leader sees the
  orphaned entry in `pending_txs`) — explicitly out of scope
  for P6 (see ROADMAP.md, P6 "Out of scope").

## [0.1.0] - 2026-02-24

### Added
- Initial release
- Raft consensus: leader election with randomized timeouts
- Election restriction (§5.4.1 of the Raft paper)
- AppendEntries RPC with heartbeats
- WAL-based persistence (bincode)
- Meta persistence for term and vote (JSON, atomic rename)
- Commit index + last applied + state machine replay
- Client TCP JSON protocol (Set / Get / Delete)
- Graceful shutdown (Ctrl-C)
- Single-node auto-elevation to Leader