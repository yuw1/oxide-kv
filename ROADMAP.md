# Oxide-KV Roadmap

> Living document for Oxide-KV's evolution. Past phases are summarized;
> the active phase is at the bottom under "Current". Future directions
> are candidate ideas, not commitments.

This file is the single source of truth for roadmap status. The older
"Roadmap" section in `README.md` is intentionally reduced to a
backward-link; if something here disagrees with the README, this file
wins.

---

## Phase status

| Phase | Capability | Status | PR | Notes |
|---|---|---|---|---|
| P0 | LICENSE, CHANGELOG, unit tests, warning cleanup | ✅ Merged | [#1](https://github.com/yuw1/oxide-kv/pull/1) | Baseline |
| P1 | Snapshot + InstallSnapshot RPC + log compaction | ✅ Merged | [#2](https://github.com/yuw1/oxide-kv/pull/2) | Raft §7 |
| P2 | Linearizable reads via ReadIndex | ✅ Merged | [#3](https://github.com/yuw1/oxide-kv/pull/3) | Raft §6.4 |
| P3 | Protobuf binary RPC (length-prefixed framing) | ✅ Merged | [#4](https://github.com/yuw1/oxide-kv/pull/4) | Wire format cutover |
| P4 | LSM-Tree state machine (memtable + WAL + SSTables) | ✅ Merged | [#5](https://github.com/yuw1/oxide-kv/pull/5) | Storage rewrite |
| P5 | Two-phase commit lifecycle (BeginTx / Vote / DecideTx) | ✅ Merged | [#6](https://github.com/yuw1/oxide-kv/pull/6) | State machine + wire schema; single-node fast path |
| Bug | Election timer brain-split + heartbeat:election ratio | ✅ Merged | [#8](https://github.com/yuw1/oxide-kv/pull/8) | Timer/logic fix |
| Bug | Single-node read fallback + commit advancement | ✅ Merged | [#9](https://github.com/yuw1/oxide-kv/pull/9) | Read fast path + sync_logs |
| **P6** | **Multi-node 2PC coordinator RPC** | **🔄 In progress** | [#11](https://github.com/yuw1/oxide-kv/pull/11)+ | **Active** |

---

## P6 — Multi-node 2PC coordinator RPC

### Problem statement

After P5, the 2PC state machine and wire schema are fully in place.
BeginTx and DecideTx are accepted `Command` variants, the state
machine handles pending transactions in `pending_txs`, and the
protobuf wire schema carries both.

What is **not** in place is the **coordinator orchestration** on the
leader. Today:

- **Single-node cluster**: the client `BeginTx` auto-pairs with a
  `DecideTx(Commit)` in the same proposal. Atomic with one round-trip.
- **Multi-node cluster**: there is no automatic vote collection, no
  quorum check, no abort path.

The README and CHANGELOG both note "RPC plumbing deferred" — this
phase closes that gap.

### Goal

A client on a multi-node cluster can issue **one** BeginTx to the
leader. The leader automatically:

1. Replicates `BeginTx` through the Raft log so every node has the
   pending tx in `pending_txs`.
2. Asks every peer "is this tx safe to commit?" via a side RPC.
3. Collects the votes with a quorum / timeout policy.
4. Proposes `DecideTx(Commit)` or `DecideTx(Abort)` as a second log
   entry based on the vote outcome.

The client sees one round-trip in (BeginTx) and one notification out
(decision). The intermediate coordination is invisible.

### Non-goals (deferred to P7+)

- Cross-shard / multi-Raft 2PC.
- Tx timeout + admin-driven abort (currently the state machine has no
  timeout — a coordinator crash leaves a pending tx in the log).
- Optimistic concurrency control / 2PL inside a tx. Current semantics
  are "last writer wins on commit"; we keep that.
- A Python / Go client SDK.
- Benchmark suite for partition / failover / 2PC throughput.

### Acceptance criteria

P6 ships when **all** of these hold on `master`:

1. **Three-node cluster runs a 2PC transaction end-to-end**
   - Start 3 nodes, wait for a stable leader.
   - Send one `BeginTx{tx_id:"t1", ops:[Put("a","1"), Put("b","2")]}` to the leader.
   - Within ~250ms the leader responds with `{"status":"ok","tx_id":"t1","decision":"Commit"}` (or `Abort` with reason).
   - `Get("a")` and `Get("b")` on any node return the new values.

2. **Failure paths covered**
   - At least one peer returns `No`: leader proposes `DecideTx(Abort)`; pending ops never become visible; subsequent `Get` returns the pre-tx value.
   - At least one peer times out / is unreachable: leader aborts (failure-as-No); no partial commit.
   - Concurrent transactions on disjoint keys: both commit; reads stay isolated until commit (existing isolation property holds).

3. **Network resilience**
   - Peer briefly disconnects mid-vote: leader times out the missing peer, aborts the tx; no leader step-down.
   - Leader steps down mid-coordination: the partially-applied `BeginTx` log entry stays in the log but no `DecideTx` ever commits (state machine rejects `Commit` for stale leader log entries, or the new leader takes over and a future admin/abort completes).

4. **Tests**
   - Unit tests cover the coordinator state machine: vote collection, quorum decision, timeout → abort, all-yes → commit, any-no → abort.
   - Integration test spins up 3 in-process nodes on a tempdir, drives a full happy-path 2PC.
   - Integration test for the abort path: kill a peer's vote response.
   - Existing 105+ tests still pass.

5. **No regressions**
   - Single-node fast path (`BeginTx + DecideTx(Commit)` in one proposal) still works.
   - Manual 2PC control via raw `Vote` / `DecideTx` JSON commands still works.

### Architecture decision — **locked 2026-08-02** ✅

| Decision | Choice | Rationale |
|---|---|---|
| Vote transport | **Option A** — side-channel RPC. Log carries only `BeginTx` + `DecideTx`. | Vote collection is a coordinator concern, not a consensus concern. Cleaner separation, fewer log entries per tx (2 vs. N+2), standard textbook 2PC. |
| Coordinator role | **Leader of the Raft cluster also acts as the 2PC coordinator.** | Reuses Raft's leader election for coordinator liveness. BeginTx/DecideTx go through the Raft log as today. |
| Quorum policy | **All-yes required.** Any peer returning No, timing out, or being unreachable aborts the tx. | Textbook 2PC. Majority quorum intentionally **not** used here — All-Yes matches classic 2PC intuition and gives cleanest operator semantics. |
| Old `Command::Vote` variant | **Removed** (breaking change, no back-compat). The `Vote` enum survives only as the in-memory state-machine representation inside `StateMachine::pending_txs`. | With votes out of the log, the log-side variant has no purpose. |
| Wire schema location | New file `proto/coordination.proto`, physically isolated from `proto/raft.proto`. | Two concerns, two files. Each evolves independently. |
| Failure recovery priority | **A first, B deferred.** Coordinator-only recovery (new leader re-runs BeginTx + vote collection) for P6. Participant-side autonomous abort deferred to a later phase. | Smallest correct thing; B has subtle correctness implications (coordinator / participant disagreement on outcome). |

Detailed Option A vs B writeup and rationale preserved below for
historical context.

There are two clean ways to wire the coordinator. They differ on **who
sends `Vote` and what it means**.

#### Option A — Side RPC, log carries only BeginTx + DecideTx

- **Wire**: leader→peer `RequestTxVote { tx_id, ops }` RPC (separate from Raft).
- **Flow**: leader appends `BeginTx` to its log → replicated by Raft as today → leader fans out `RequestTxVote` to each peer → peers inspect their local `pending_txs` and respond Yes/No → leader collects → leader appends `DecideTx(Commit | Abort)`.
- **`Vote` Command becomes redundant.** We can either keep it in the wire schema for back-compat or deprecate it.
- **Pros**: cleanest separation. Vote is a coordinator concern, not a state-machine concern. Fewer log entries per tx (2 instead of N+2). Standard textbook 2PC.
- **Cons**: requires a new RPC type. Peers must inspect `pending_txs` to answer. Slightly more code in the RPC layer.

#### Option B — `Vote` as a Raft log entry, coordinator just counts log entries

- **Wire**: no new RPC. Leader appends one `Vote{tx_id, voter, vote}` per peer as a Raft log entry. State machine's `record_vote` already handles it.
- **Flow**: leader appends `BeginTx` → leader appends `Vote{self,Yes}` → leader appends `Vote{peer1,Yes}` (received via side-channel) → ... → once all `Vote` entries seen in the log, leader appends `DecideTx`.
- **Pros**: reuses existing state machine code. Vote is replicated to all nodes automatically.
- **Cons**: extra log entries per tx (N+2 instead of 2). Vote from a peer is a lie — the peer didn't actually vote, the leader is just recording what the peer said. Reads "Vote" entries are confusing because no quorum machinery exists for them. Coordinator still needs a side-channel to learn peer votes.

#### Recommendation

**Option A.** It matches textbook 2PC, has fewer log entries, and
keeps `Vote` out of the Raft log where it doesn't belong.

#### Decision locked 2026-08-02 ✅

- [x] **Option A** (side-channel RPC, log carries only BeginTx + DecideTx).
- [x] **Remove** the existing `Vote` `Command` variant from the wire schema (breaking; see PR #11).
- [x] **All-yes quorum.** Any No, any timeout, or any unreachable peer aborts the tx.

### PR plan (Option A locked)

| PR | Title | Scope | Tests added |
|---|---|---|---|
| **#11** | `feat(coordination): Protobuf schema for 2PC coordinator RPC, remove old Command::Vote` | New `proto/coordination.proto` with `VoteRequest` / `VoteResponse` (physical isolation from `proto/raft.proto`). New `src/coordination.rs` with domain types + `From` conversions + 9 unit tests (typed round-trip ×2, wire round-trip ×2, boundary ×3, forward-compat ×2). `build.rs` compiles both proto files. `Command::Vote` variant removed from `protocol.rs`; `Vote` enum retained only as the in-memory state-machine representation. `Command::Body::TxVote` removed from `raft.proto` (tag 6 left empty inside the oneof — `reserved` cannot be declared there). `raft::proto` inverse conversion silently treats stale `TxVote` body as Compact. CHANGELOG notes breaking change. ROADMAP decisions + PR plan updated. | `coordination`: 9 unit tests (see `src/coordination.rs`). `raft::proto`: regression coverage on the inverse conversion. State-machine tests unchanged. Total tests: 116 → 124 (+8 net). |
| **#12** | `feat(raft): RequestTxVote RPC handler + RpcClient wrapper` | Server-side dispatch in `RpcServer::handle_rpc_logic` → `RaftNode::handle_tx_vote_request`. Client-side `RpcClient::send_request_tx_vote`. Transport: TBD (separate port vs. multiplexed socket on existing Raft port). | `raft::rpc`: end-to-end framed round-trip with the new RPC type. `raft::node`: tests for `handle_tx_vote_request` (Yes when tx is pending and ops are safe; No when tx is not pending or conflict). |
| **#13** | `feat(coordinator): leader-side BeginTx coordinator with vote collection` | New `Coordinator` struct (or methods on `RaftNode`) holding in-flight tx state: `InflightTx { tx_id, ops, voters: BTreeMap<peer, Vote>, deadline }`. `begin_tx_coordinate(tx_id, ops)` on leader: replicate BeginTx, fan out RequestTxVote with timeout, decide Commit/Abort, propose DecideTx. Apply **all-yes quorum**. | `coordinator`: vote collection logic, all-yes quorum decision, timeout → abort, leader-step-down → recovery. `client::begin_tx` routes to coordinator on multi-node clusters. |
| **#14** | `test(2pc): 3-node in-process integration test for happy path + abort path` | Spin up 3 nodes on a tempdir, drive `BeginTx`, assert Commit + visible reads. Inject peer No-vote, assert Abort + isolation holds. Inject peer timeout, assert Abort. | `tests/integration_2pc.rs` (new integration test file). |

PR #10 is the docs PR that introduced this roadmap.

Estimated test growth: +20-25 tests. Total target after P6: ~130.

### Out of scope for these PRs

- Client SDK in Python/Go.
- Tx timeout for crash recovery (separate concern: how does the state machine decide "tx t1 has been pending too long, abort it?" — needs an admin RPC or a background sweeper).
- Cross-Raft-group transactions.
- Read-your-writes within a tx before commit (not a stated goal; can be layered later).

---

## Future directions (not promised, not ordered)

The full list lives in the README "Candidate future directions" section.
When one of these becomes an active phase, it gets its own section here.

- Sharded multi-Raft
- Joint consensus for membership change
- Tx timeout + admin-driven abort (close the coordinator-crash hole)
- Client SDKs (Python, Go)
- Benchmark suite
- LSM polish (bloom filters, block cache, background compaction, leveled compaction)
- gRPC transport (replace raw TCP + protobuf)
- TLS for inter-node RPC and the client API
- Per-read ack tracking for full ReadIndex
- Metrics export (Prometheus)