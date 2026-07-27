# Changelog

All notable changes to Oxide-KV will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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