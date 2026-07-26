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
- **Test suite (79 tests, all passing)**:
  - `state_machine`: set/get/delete/replace lifecycle
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

### Changed
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
- Chunked InstallSnapshot transfer (offset/done fields) is a future optimization.

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