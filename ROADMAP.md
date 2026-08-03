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
| P6 | Multi-node 2PC coordinator RPC | ✅ Merged | [#11](https://github.com/yuw1/oxide-kv/pull/11)–[#14](https://github.com/yuw1/oxide-kv/pull/14) | Leader-as-coordinator, all-yes quorum, side-channel vote RPC |
| **P7** | **Deterministic simulation testing (DST)** | **🔄 Active** | — | **Correctness harness: fault injection + invariant checks** |

---

## P6 — Multi-node 2PC coordinator RPC

### Problem statement

After P5, the 2PC state machine and wire schema are fully in place.
BeginTx and DecideTx are accepted `Command` variants, the state machine
handles pending transactions in `pending_txs`, and the protobuf wire
schema carries both.

What is **not** in place is the **coordinator orchestration** on the
leader. Today:

- **Single-node cluster**: the client `BeginTx` auto-pairs with a
  `DecideTx(Commit)` in the same proposal. Atomic with one round-trip.
- **Multi-node cluster**: there is no automatic vote collection, no
  quorum check, no abort path.

The README and CHANGELOG both note "RPC plumbing deferred" — this
phase closes that gap.

### Locked architecture decisions (2026-08-02)

| Decision | Choice | Rationale |
|---|---|---|
| Coordinator role | **Leader of the Raft cluster also acts as the 2PC coordinator.** | Reuses Raft's leader election for coordinator liveness; one fewer set of election timers to reason about. BeginTx/DecideTx go through the Raft log as today. |
| Quorum policy | **All-yes required.** Any peer returning No, timing out, or being unreachable aborts the tx. | Textbook 2PC. Majority quorum is intentionally **not** used here — a single slow peer would force Abort under majority, but All-Yes gives the cleanest semantics for the operator and matches classic 2PC intuition. |
| Vote transport | **Side-channel RPC**, separate from the Raft log. Log carries only `BeginTx` + `DecideTx`. | Vote collection is a coordinator concern, not a consensus concern. Putting it in the log inflates log size per tx and confuses log readers. |
| Wire schema for votes | New file `proto/coordination.proto` with `VoteRequest` / `VoteResponse` messages. **Physically isolated** from `proto/raft.proto`. | Two concerns, two files. The two schemas evolve independently and can be reviewed separately. |
| Old `Command::Vote` variant | **Removed** (breaking). The `Vote` enum survives only as the in-memory representation inside `StateMachine::pending_txs`. | With votes out of the log, the log-side variant has no purpose. State machine still tracks Yes/No per peer internally. |
| Failure recovery (priority A vs B) | **A first, B as TODO.** Coordinator-only recovery for now (new leader re-runs BeginTx + vote collection). Per-participant timeout-driven autonomous abort deferred. | Smallest correct thing; B has subtle correctness implications (coordinator / participant disagreement on outcome). |

### Goal

A client on a multi-node cluster can issue **one** BeginTx to the
leader. The leader automatically:

1. Replicates `BeginTx` through the Raft log so every node has the
   pending tx in `pending_txs`.
2. Asks every peer "is this tx safe to commit?" via a side-channel
   `VoteRequest` RPC.
3. Collects the votes. **All peers must vote Yes**, otherwise the tx
   aborts.
4. Proposes `DecideTx(Commit)` or `DecideTx(Abort)` as a second log
   entry based on the vote outcome.

The client sees one round-trip in (BeginTx) and one notification out
(decision). The intermediate coordination is invisible.

### Non-goals (deferred to P7+)

- Cross-shard / multi-Raft 2PC.
- Tx timeout + admin-driven abort (currently the state machine has no
  timeout — a coordinator crash leaves a pending tx in the log).
- Participant-side autonomous abort (priority B above): a follower
  that times out the coordinator would unilaterally abort. Deferred
  to a later phase; current policy is "wait forever, recovery is
  coordinator-only".
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
   - Existing 110+ tests still pass; P6 adds ~25.

5. **No regressions**
   - Single-node fast path (`BeginTx + DecideTx(Commit)` in one proposal) still works.
   - Manual `DecideTx` JSON command still works for tests / admin (the `Command::Vote` arm is gone, but the rest of `Command` is unchanged).

### PR plan (locked — Option A side-RPC)

| PR | Title | Scope | Tests added |
|---|---|---|---|
| #10 | `docs: add ROADMAP.md and pin P6 as multi-node 2PC coordinator RPC` | New `ROADMAP.md`; README points at it. | Docs only. |
| **#11** | `feat(coordination): Protobuf schema for 2PC coordinator RPC, remove old Command::Vote` | New `proto/coordination.proto` with `VoteRequest` / `VoteResponse`. New `src/coordination.rs` with domain types, `From` conversions, and wire round-trip tests (typed round-trip, length-delimited wire round-trip, boundary values, forward-compat with unknown tag 99). `build.rs` updated to compile the new proto. `Command::Vote` variant removed from `protocol.rs`; `Vote` enum retained only as the in-memory state-machine representation. `Command::Body::TxVote` removed from `raft.proto`; tag 6 left empty (cannot be `reserved` inside `oneof`). PR #11 sub-tasks: <br> 1. `proto/coordination.proto` with `VoteRequest { term, tx_id, last_log_index, last_log_term }` and `VoteResponse { term, vote_granted, reason }`.<br> 2. `build.rs` compiles both `proto/raft.proto` and `proto/coordination.proto`.<br> 3. `src/coordination.rs` with domain types + `From` conversions + 9 unit tests (typed round-trip ×2, wire round-trip ×2, boundary ×3, forward-compat ×2).<br> 4. `Command::Vote` removed from `protocol.rs`; `Vote` enum stays for `pending_txs`.<br> 5. `Command::Body::TxVote` removed from `raft.proto`; reverse conversion in `src/raft/proto.rs` silently treats stale `TxVote` body as Compact.<br> 6. ROADMAP.md updated: locked decisions table + PR #11 sub-tasks. | `coordination`: 9 unit tests (see src/coordination.rs). `raft/proto`: regression coverage on the inverse conversion. State-machine tests unchanged. |
| ✅ #12 (merged) | `feat(raft): multiplex 2PC vote RPC onto the Raft port` | Transport: multiplexed on the Raft port (`DispatchKind::Vote = 0x02`); see `src/raft/transport.rs`. Server-side `RpcServer::dispatch` + `handle_vote_rpc_inner` → `RaftNode::handle_tx_vote_request`. Client-side `RpcClient::send_tx_vote_rpc`. | `transport::tests`: 9 envelope/framing tests. `raft::node::tests`: 5 `handle_tx_vote_request` cases. `raft::rpc::tests`: 2 dispatch round-trips. +16 tests (124 → 140). |
| ✅ #13 (merged) | `feat(coordinator): leader-side 2PC coordinator orchestration` | New `src/raft/coordinator.rs` module exposing `coordinate_tx(node_arc, tx_id, ops) -> TxOutcome` and `TxOutcome` enum (Committed / Aborted / NotLeader). Single-node fast path preserved (BeginTx + DecideTx(Commit) batch). Multi-node path: propose BeginTx → wait for apply → record leader's Yes → fan-out VoteRequest via `tokio::spawn` → tally votes (all-yes = Commit, any No/timeout/error/higher-term = Abort) → propose DecideTx. 10s wall-clock bound on the entire round. `RaftNode` gains read-only accessors (`node_id`, `peers`, `current_term`, `get_log_entry`). `client::begin_tx` becomes a thin wrapper that translates `TxOutcome` to JSON. | `coordinator::tests`: 4 tests (single-node end-to-end commit, apply_logs regression for BeginTx/DecideTx in steady state, apply_logs Abort counterpart, `TxOutcome` smoke). +4 tests (140 → 144). |
| 🔵 #14 (this PR, OPEN) | `test(2pc): 3-node in-process integration test` | New `tests/integration_2pc.rs` (3 tests). 3-node in-process cluster on `127.0.0.1:0` ephemeral ports + tempdir data dirs. 3 scenarios: happy path (commit + cluster-wide visibility), no-vote abort (phantom log entry forces `leader-log-stale` No), unreachable peer abort (blackhole at `127.0.0.1:1`). | `tests/integration_2pc.rs`: 3 integration tests. +3 (144 → 147). Total integration test time: ~5.3s. |

PR #10 is the docs PR that introduced this roadmap.

Estimated test growth: +20-25 tests across PRs #11-#14. After PR #14 the count is 147 / 147 passing (144 unit + 3 integration). P6 is complete. Final target after P6: ~150.

### Out of scope for these PRs

- Client SDK in Python/Go.
- Tx timeout for crash recovery (separate concern: how does the state machine decide "tx t1 has been pending too long, abort it?" — needs an admin RPC or a background sweeper).
- Cross-Raft-group transactions.
- Read-your-writes within a tx before commit (not a stated goal; can be layered later).
- Participant-side autonomous abort (priority B from the decision table). Will be a separate phase if/when priority A turns out to leave real recovery holes.

---

## P7 — Deterministic simulation testing (DST)

### Problem statement

P0–P6 delivered a working Raft + 2PC + LSM stack with 147 passing
tests. But those tests are overwhelmingly **happy-path and single-point
race** coverage. There is currently **no test that proves the system
stays safe under partition, crash, and recovery combinations.**

Concretely, we *believe* the following hold but cannot *demonstrate* any
of them:

- A committed entry is never lost, even if the leader crashes right
  after commit and a follower with a stale log wins the next election.
- There is never more than one leader in the same term, across
  arbitrary message loss / reorder / duplication.
- The 2PC all-yes quorum never produces a partial commit when the
  coordinator crashes mid-round, when a partition isolates a peer, or
  when a participant restarts with a pending tx in its log.
- ReadIndex never serves a value that a linearizable client could
  observe as stale.

This is the gap between "wrote a Raft" and "can take responsibility for
its correctness." P7 closes it.

### Goal

A **deterministic, reproducible simulation harness** that runs the real
Raft + 2PC code (not a model) against a controlled, adversarial network
and fault scheduler, then asserts safety invariants over thousands of
randomized scenarios.

Determinism is the core requirement: given a seed, a failing scenario
must replay identically, so a bug found at 3am is debuggable at 9am.

### Approach

1. **Virtual clock + virtual network.** Replace wall-clock timers and
   real TCP with a simulated clock and an in-memory network that the
   scheduler controls. The Raft code under test must not be able to
   tell the difference. This likely means abstracting the transport
   (`src/raft/transport.rs`) and the clock behind traits, with a
   `real` impl (production) and a `sim` impl (tests).
2. **Fault injection.** The scheduler can, at any tick: drop / delay /
   reorder / duplicate a message; crash a node (stop its tasks, keep
   its disk); restart a node (reload from disk); partition the network
   into arbitrary groups; heal a partition.
3. **Invariant checker.** After every scenario (and, where cheap,
   periodically mid-scenario) assert the safety properties:
   - **Election safety:** at most one leader per term.
   - **Leader append-only / state-machine safety:** if two nodes apply
     a log entry at the same index, the entries are identical.
   - **Committed-entry durability:** once the leader reports commit,
     that entry survives any subsequent legal fault sequence.
   - **2PC atomicity:** for each tx, either all nodes apply it or none
     do; never a partial commit.
   - **Linearizability:** every read returns a value consistent with
     some linearization of the committed writes (checked against a
     reference model).
4. **Reference model.** A simple sequential spec (a HashMap with
   atomic multi-key tx) that the simulation compares real behavior
   against. This is what turns "no crash" into "correct."
5. **Seed-driven fuzzing.** A driver that runs N scenarios with random
   seeds, shrinking a failing seed toward a minimal reproduction.

### Non-goals (for P7)

- Liveness proofs / guarantees under sustained partition (we assert
  *safety*; liveness under adversarial scheduling is out of scope).
- Performance benchmarking (that is a separate "benchmark suite" item).
- Jepsen-style black-box testing over a real network (DST is white-box,
  in-process, deterministic; a real-network Jepsen harness may come
  later but is not P7).
- Rewriting Raft / 2PC logic. P7 *tests* the existing code; if it finds
  bugs, those are fixed in follow-up fix PRs, not inside the harness PR.

### Acceptance criteria

P7 ships when **all** of these hold on `master`:

1. **Harness exists and is deterministic.** A simulation runs the real
   node code over a virtual clock + network. Running the same seed
   twice produces the identical event trace.
2. **Fault coverage.** The scheduler can inject at least: message drop,
   delay, reorder, node crash, node restart-from-disk, 2-way and 3-way
   partition, partition heal.
3. **Invariants enforced.** Election safety, state-machine safety,
   committed-entry durability, and 2PC atomicity are all checked and
   will fail the test on violation.
4. **Scale.** The suite runs ≥1000 randomized scenarios (across seeds)
   in CI-bounded time, plus a smaller default `cargo test` set.
5. **Reference model.** Linearizable reads and tx outcomes are checked
   against a sequential spec, not just "did it crash."
6. **No regressions.** Existing 147 tests still pass; P7 adds the
   harness + a seed-driven test entry point.
7. **Documented.** README + CHANGELOG explain how to run the simulation
   and how to reproduce a failing seed.

### Planned work (not tied to specific PR numbers)

The work below is what P7 needs; how it splits into PRs is decided as
the work actually lands (GitHub assigns numbers at creation time, so
pinning them here would be false precision).

**Progress (2026-08-03):** the first three bullets are merged (traits,
sim clock/network/scheduler, invariants + reference model + seed fuzz).
The fuzz harness now also drives real 2PC rounds (`SubmitTx` action →
`SimCluster::run_tx` → the production coordinator), which is what feeds
the 2PC-atomicity invariant and the reference model's transaction
handling. All P7 acceptance items shipped.

- **Abstract transport + clock behind traits.** Introduce `Transport`
  and `Clock` traits; `real` impls wrap the current TCP + `tokio::time`
  with no behavior change and existing tests green. This is the
  foundation the simulation builds on.
- **Deterministic virtual clock + network + fault scheduler.** `sim`
  impls: a virtual clock, an in-memory network supporting
  drop/delay/reorder/duplicate, a fault scheduler, and a seed-driven
  driver. The harness runs a trivial scenario deterministically.
- **Safety invariants + reference model.** An invariant checker
  (election / state-machine / committed-entry durability / 2PC
  atomicity) plus a sequential reference model, wired into scenario
  teardown.
- **Fault scenarios + seed fuzzing + shrinker.** Crash / restart /
  partition / heal scenarios; an N-seed fuzzer; minimal-repro shrinking;
  CI integration; docs.

If the harness surfaces real bugs (expected), each gets its own
`fix/...` PR with a regression scenario added to the suite.

### Open questions (to resolve as work starts)

- How invasive is abstracting the clock? `tokio::time` is used in
  several places (election timer, heartbeat loop, RPC timeouts). May
  need a `Clock` trait threaded through, or `tokio::time::pause()` +
  controlled advance. Resolve in an initial spike.
- Do we simulate at the TCP byte level or at the RPC-message level?
  Message-level is far cheaper to build and sufficient for safety;
  byte-level catches framing bugs but is heavier. Lean message-level.
- CI time budget: how many seeds in the default gate vs a nightly
  large-N run.

---

## Future directions (not promised, not ordered)

The full list lives in the README "Candidate future directions" section.
When one of these becomes an active phase, it gets its own section here.

- Sharded multi-Raft
- Joint consensus for membership change
- Tx timeout + admin-driven abort (close the coordinator-crash hole)
- Participant-side autonomous abort (2PC priority B)
- Client SDKs (Python, Go)
- Benchmark suite
- LSM polish (bloom filters, block cache, background compaction, leveled compaction)
- gRPC transport (replace raw TCP + protobuf)
- TLS for inter-node RPC and the client API
- Per-read ack tracking for full ReadIndex
- Metrics export (Prometheus)