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
- **Test suite (61 tests, all passing)**:
  - `state_machine`: set/get/delete/replace lifecycle
  - `raft::storage`: WAL round-trip, meta round-trip, atomic rename,
    cross-instance durability, snapshot round-trip, snapshot atomic rename,
    WAL truncation semantics
  - `raft::node`: §5.4.1 vote safety + election restriction, AppendEntries
    consistency, §5.4.2 commit safety, quorum calculation, propose rules,
    replay/apply, leader init idempotency, install snapshot (state machine
    replacement, log truncation, commit advance, term handling, disk
    persistence, WAL rewrite), leader-side snapshot trigger

### Changed
- Cleaned up 18 compiler warnings (unused imports, unused variable, unnecessary
  parens, io::Result handling) — now 0 warnings
- `RaftStorage::new_with_paths` now takes a third `snapshot_path` argument
- `RaftNode::new` now delegates to `new_with_storage` for shared construction logic

### Notes
- `RaftStorage` and `RaftNode` were refactored to accept explicit paths so
  tests can isolate each scenario in a temp dir without touching global `Config`.
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