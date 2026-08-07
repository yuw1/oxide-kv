<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="Oxide-KV logo" width="200">
  </picture>
</p>

<h1 align="center">Oxide-KV</h1>

<p align="center">
  A distributed key-value store in Rust implementing the <b>Raft Consensus<br>
  Algorithm</b>, with a Log-Structured Merge-Tree storage engine, Protobuf<br>
  inter-node RPC, and a Two-Phase Commit lifecycle for atomic multi-key<br>
  writes.
</p>

<p align="center"><i>Lightweight by design — every line is meant to be read.</i></p>

<p align="center">
  <a href="https://github.com/yuw1/oxide-kv/actions"><img src="https://github.com/yuw1/oxide-kv/actions/workflows/rust.yml/badge.svg" alt="CI"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License: Apache-2.0"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-edition_2024-orange.svg" alt="Rust"></a>
  <a href="#development"><img src="https://img.shields.io/badge/tests-271_passing-brightgreen.svg" alt="Tests"></a>
</p>

---

## Features

### Consensus (Raft core, all of §5 + §6 + §7 + §9.6)

- **Leader election** with randomized timeouts (5-10s) and term-based
  step-down; 250ms heartbeats.
- **Pre-vote** (§9.6) — a partitioned follower that recovers probes
  for quorum before bumping its term, so partition recovery can't
  churn the live leader ("disruptive server" problem closed).
- **Election restriction** (§5.4.1) — votes only for candidates with a
  log at least as up-to-date as the voter's.
- **AppendEntries** RPC with heartbeats; **commit safety** (§5.4.2) — a
  leader never commits entries from previous terms by replica count
  alone.
- **Snapshot + InstallSnapshot RPC** (§7) — bounded WAL growth and
  catch-up for lagging followers; auto-triggered by
  `OXIDE_SNAPSHOT_THRESHOLD_BYTES` (default 64 MiB).
- **Joint consensus membership change** (§6) — `AddNode` / `RemoveNode`
  client commands go through a Joint→Simple two-phase configuration
  transition without restarting the cluster; brand-new servers catch
  up via the `JoinCluster` RPC.

### Correctness hardening

- **Linearizable reads via ReadIndex** (§6.4) — `Get` no longer risks
  stale data from a partitioned leader. The leader proves it still
  holds quorum before serving the value.
- **Crash recovery** — on restart the WAL replays into the in-memory
  table; SSTables are discovered from disk and the sparse key range
  is used to skip irrelevant ones on read.

### Storage

- **Log-Structured Merge Tree** state machine: in-memory memtable
  (BTreeMap) backed by an append-only WAL, with sorted on-disk
  SSTables and size-tiered compaction.
- **Atomic single-key writes** via the Raft log; **atomic multi-key
  writes** via the 2PC lifecycle below.

### Coordination

- **Two-Phase Commit lifecycle** — `BeginTx` stages ops in the Raft
  log, the leader (acting as 2PC coordinator) collects votes from all
  participants over a side-channel RPC, and `DecideTx` commits or
  aborts atomically. Pending ops are isolated from reads until commit.
  Single-node coordinator uses a fast path that appends BeginTx +
  DecideTx as one Raft proposal. Stuck transactions are cleaned up by
  a coordinator timeout sweep (`OXIDE_TX_TIMEOUT_MS`, default 30s)
  and an admin `AbortTx` RPC.

### Observability

- **Prometheus `/metrics` endpoint** (`--metrics-addr`, default
  `127.0.0.1:9100`, `disabled` to skip) exposing raft term / role /
  commit index, per-peer replication progress, and 2PC counters, plus
  a `/health` endpoint.

### Plumbing

- **Protocol Buffers** over length-prefixed TCP for inter-node Raft
  RPC. Replaces the original JSON encoding; ~30% smaller payloads,
  faster encode/decode. Wire schema lives in `proto/raft.proto`
  (consensus) and `proto/coordination.proto` (2PC vote RPC); frames
  are a 1-byte protocol discriminator + 4-byte big-endian length +
  protobuf payload.
- **Async Tokio** runtime for RPC handling and concurrent client
  connections.
- **Apache-2.0** license. Zero compiler warnings. 271 lib unit tests
  plus cross-process / integration / simulation suites.

---

## Architecture

```
            ┌────────────────────────────────────────────────┐
            │                  RaftNode                       │
            │                                                │
   Client → │   ┌──────┐  log entries  ┌──────────────┐      │
   (JSON)   │   │ Get  │──────────────▶│ RaftStorage  │      │
            │   │ Set  │  AppendEntries │  - meta.json │      │
            │   │BeginTx│              │  - NNN.sst   │      │
            │   │Decide│              │  - NNN.meta  │      │
            │   └──────┘              │  - WAL       │      │
            │       │                 └──────────────┘      │
            │       ▼                       │                │
            │   ┌──────────┐         ┌─────────────┐          │
            │   │ LSM tree │         │  Raft log   │          │
            │   │ StateMch │         │  (bincode)  │          │
            │   └──────────┘         └─────────────┘          │
            │       │                                        │
            │       ▼                                        │
            │   pending_txs: BTreeMap<TxId, PendingTx>        │
            │   (isolated from reads until DecideTx)         │
            └────────────────────────────────────────────────┘
                              │ TCP + length-prefixed protobuf
                              ▼
                    peer RaftNodes (same code)
```

### Node lifecycle

- **Follower** — passive, responds to RPCs.
- **Candidate** — active during election.
- **Leader** — handles client requests and replicates the log.

### Inter-node RPC

| RPC | Purpose |
|---|---|
| `RequestPreVote` / `PreVoteResponse` | Pre-election probe (§9.6) |
| `RequestVote` / `VoteResponse` | Election |
| `AppendEntries` / `AppendReply` | Log replication + heartbeat |
| `InstallSnapshot` / `InstallSnapshotReply` | Send a snapshot to a lagging follower |
| `JoinCluster` / `JoinClusterResponse` | Cold-new-server membership discovery |

Vote collection for 2PC travels on the same TCP listener under a
separate protocol discriminator (`proto/coordination.proto`).

Wire schemas: [`proto/raft.proto`](./proto/raft.proto) and
[`proto/coordination.proto`](./proto/coordination.proto). Frames are a
1-byte protocol discriminator (0x01 = Raft, 0x02 = 2PC votes) +
4-byte big-endian length prefix + protobuf payload.

### Storage engine

- **Raft log** (`wal`): bincode-serialized log entries, replayed on
  restart.
- **State machine WAL** (`memtable.wal`): JSON-line op log, fsync per
  write, replayed on restart.
- **Memtable** (`BTreeMap<String, MemEntry>`): in-memory sorted
  buffer, flushed when it crosses the size threshold.
- **SSTables** (`sst/NNNNNN.sst` + `.meta`): sorted JSON entries on
  disk; sparse key range used to skip irrelevant tables.
- **Compaction** (size-tiered): merges all SSTables into one, drops
  tombstones with no possible resurrection.

### Two-phase commit

- **BeginTx** (log entry): stages ops in `pending_txs`, NOT in
  memtable. Reads cannot see them.
- **Vote collection** (side-channel RPC, not in the log): the leader,
  acting as 2PC coordinator, asks every participant for a Yes/No vote
  (`proto/coordination.proto`). All-yes required; any No / timeout /
  unreachable peer aborts the transaction.
- **DecideTx** (log entry): `Commit` applies all pending ops
  atomically; `Abort` discards them. A coordinator timeout sweep
  (`OXIDE_TX_TIMEOUT_MS`) and the admin `AbortTx` RPC clean up
  transactions whose coordinator crashed.

---

## Quick Start

### Prerequisites

Rust 2024 edition (stable). For Protobuf code generation the build
expects `protoc` to be on `PATH`; CI installs it automatically.

### Run a 3-node cluster

The one-shot helper (same script CI uses):

```bash
cargo build --release
./deploy/scripts/bootstrap-cluster.sh start    # 3 nodes on one host
./deploy/scripts/bootstrap-cluster.sh status
# Logs: /tmp/oxide-kv-node-{1,2,3}.log
```

Or by hand, three terminals (`--peers` is comma-separated):

```bash
# Node 1
cargo run -- \
  --addr 127.0.0.1:9001 --client-addr 127.0.0.1:9101 \
  --peers 127.0.0.1:9002,127.0.0.1:9003

# Node 2
cargo run -- \
  --addr 127.0.0.1:9002 --client-addr 127.0.0.1:9102 \
  --peers 127.0.0.1:9001,127.0.0.1:9003

# Node 3
cargo run -- \
  --addr 127.0.0.1:9003 --client-addr 127.0.0.1:9103 \
  --peers 127.0.0.1:9001,127.0.0.1:9002
```

### Single-key ops (JSON over TCP to leader's client port)

```bash
echo '{"Set":{"key":"hello","value":"world"}}' | nc 127.0.0.1 9101
echo '{"Get":{"key":"hello"}}' | nc 127.0.0.1 9101
echo '{"Delete":{"key":"hello"}}' | nc 127.0.0.1 9101
```

`Get` uses the ReadIndex path, so a partitioned leader will refuse to
serve stale data instead of returning it. (For scripted use prefer
the Python SDK or the Rust CLI — some `nc` variants close the socket
on stdin EOF before a ReadIndex reply arrives; see the deployment
section below.)

### Multi-key atomic op (2PC)

```bash
echo '{"BeginTx":{"tx_id":"t1","ops":[
  {"Put":{"key":"a","value":"1"}},
  {"Put":{"key":"b","value":"2"}}
]}}' | nc 127.0.0.1 9101
```

In a single-node cluster the leader auto-pairs this with
`DecideTx(Commit)` so the transaction applies atomically. In a
multi-node cluster the leader coordinates the full round: BeginTx
replicates, votes are collected over the side-channel RPC, and
DecideTx(Commit/Abort) replicates the outcome.

### Membership change (no restart required)

```bash
# Add a node (run against the leader):
echo '{"AddNode":{"server":{"node_id":"127.0.0.1:9004","addr":"127.0.0.1:9004"}}}' | nc 127.0.0.1 9101
# Remove a node:
echo '{"RemoveNode":{"node_id":"127.0.0.1:9004"}}' | nc 127.0.0.1 9101
```

Both go through the joint-consensus two-phase transition (Raft §6).
A brand-new server can also bootstrap itself with the `JoinCluster`
RPC given the address of any current member.

### Force-abort a stuck transaction

```bash
echo '{"AbortTx":{"tx_id":"t1"}}' | nc 127.0.0.1 9101
```

---

## Safety & Consistency

The following properties are verified by the test suite.

- **§5.4.1 vote safety** — `(last_log_term > my_term) || (last_log_term == my_term && last_log_index >= my_index)`.
- **§5.4.2 commit safety** — a leader only directly commits entries
  from its own current term; previous-term entries are committed
  implicitly when a current-term entry from the same index replicates.
- **Linearizable reads** — `confirm_read` enforces three guards:
  still leader, `last_applied >= ri.index`, and a fresh quorum proof
  (`last_quorum_heartbeat_at`) at or after the read's `issued_at`.
- **Snapshot install safety** — `handle_install_snapshot` rejects
  stale terms, truncates the log to `> last_included_index`, resets
  `commit_index` / `last_applied`, and persists before replying.
- **LSM durability** — WAL is fsync'd per write; SSTables are written
  via atomic rename. `restore_wal_log` is the single recovery path.
- **2PC read isolation** — pending ops in `pending_txs` are not in
  the memtable; `get` never returns a value that hasn't been
  committed via `DecideTx(Commit)`.

---

## Development

```bash
cargo build --all-targets     # 0 warnings
cargo test                    # 271 passed (lib) + integration suites
```

Test layout: in-source `#[cfg(test)] mod tests` plus integration
tests in `rust/oxide-kv/tests/` (joint consensus, JoinCluster, 2PC,
pre-vote recovery, metrics endpoint, cross-process 3-node smoke,
deterministic-simulation fuzz). Each unit test runs against an
isolated tempdir; the cross-process smoke suite (feature-gated
behind `cross-process-smoke`) needs the release binary.

### Project layout

```
rust/oxide-kv/
├── src/
│   ├── client.rs          Client JSON protocol handler
│   ├── config.rs          Global config (OnceLock)
│   ├── lib.rs
│   ├── main.rs            CLI entry point
│   ├── protocol.rs        Command / LogEntry / Snapshot / 2PC types
│   ├── state_machine.rs   LSM tree (memtable + WAL + SSTable)
│   ├── coordination.rs    2PC coordinator RPC types
│   ├── observability/     Prometheus /metrics + OpenTelemetry no-op
│   └── raft/
│       ├── node.rs        RaftNode (election, log, snapshot, 2PC, read-index)
│       ├── coordinator.rs 2PC coordinator + membership coordinator
│       ├── proto.rs       Protobuf <-> domain conversions
│       ├── rpc.rs         TCP+protobuf client/server
│       ├── storage.rs     WAL + meta + snapshot on disk
│       ├── timer.rs       Election timer
│       ├── clock.rs       Clock trait (wall clock / simulation)
│       ├── net.rs         Transport trait + TcpTransport
│       ├── transport.rs   Transport trait definition
│       ├── sim_transport.rs / sim_harness.rs / fault_scheduler.rs
│       │                  Deterministic-simulation infrastructure (P7)
│       ├── invariants.rs / reference_model.rs  Safety checkers (P7)
rust/oxide-kv-client/    Async TCP client (JSON line protocol)
proto/
├── raft.proto             Raft wire schema
└── coordination.proto     2PC coordinator wire schema
python/                   Pure-Python SDK (TCP + JSON line protocol)
├── oxide_kv/              Client + Transaction + errors
├── examples/              Runnable demos (oxide_kv_demo.py)
└── tests/                 pytest suite
deploy/
├── scripts/bootstrap-cluster.sh   One-host 3-node dev cluster
└── systemd/                       Production unit + env template
```

---

## Deployment (internal network)

This section is the "make it run on three Linux boxes and keep it
up" guide. It assumes an **internal-network** deployment — Oxide-KV
ships without TLS, auth, or rate limiting, so it's not safe to
expose to the public internet. Front it with a reverse proxy if
you need internet access.

### Hardware & OS

| Requirement | Why |
|---|---|
| **3 nodes** (VM / container / bare metal) | Raft requires a majority; 2 nodes means zero fault tolerance, 4+ is supported but adds no quorum benefit for 3-of-5. |
| **Internal network connectivity** between nodes | Raft heartbeats are RPC; partition = leader election churn. |
| **Clock sync** (chrony / ntp), drift < 100ms | Election timer windows are sensitive to skewed clocks; large drift → spurious elections. |
| **OS user** `oxide-kv` (non-login, no home) | Service should not run as root; systemd `User=` field needs this user. |
| **Data directory** `/var/lib/oxide-kv/<node-id>`, mode 0750 | One per node; do NOT share across nodes. |
| **Log directory** `/var/log/oxide-kv` (journald) | Captures startup / election / shutdown events. |

### Ports

| Port (convention) | Protocol | Purpose | Bound to |
|---|---|---|---|
| **9001** (this node) | Protobuf over TCP | Raft inter-node RPC (heartbeats, AppendEntries, RequestPreVote/RequestVote, InstallSnapshot, JoinCluster) | Internal network only |
| **9101** (this node) | JSON over TCP | Client API (Set/Get/Delete/2PC/AddNode/RemoveNode/AbortTx) | Internal network + reverse proxy |
| **9100** (this node) | HTTP | Prometheus `/metrics` + `/health` | Loopback by default; bind to a routable address for remote scraping |

Port numbers are configurable; the production systemd env template
uses `OXIDE_KV_ADDR=<ip>:9001` for every node, and the dev
`bootstrap-cluster.sh` script staggers each node by `+100` (node-1 =
9001, node-2 = 9002, node-3 = 9003, same for 9101/9102/9103 and
9100/9200/9300).

**Do not expose 9001/9002/9003 to the internet.** Raft has no auth;
any random client could trigger spurious elections.

### Firewall rules (example: iptables / nftables)

```
# On each node, allow Raft RPC from the other two:
-A INPUT -p tcp -s 10.0.0.2 --dport 9001 -j ACCEPT   # allow node-2 -> us
-A INPUT -p tcp -s 10.0.0.3 --dport 9001 -j ACCEPT   # allow node-3 -> us
-A INPUT -p tcp --dport 9001 -j DROP                 # deny everyone else

# Allow client API from app hosts / reverse proxy:
-A INPUT -p tcp -s 10.0.1.0/24 --dport 9101 -j ACCEPT
-A INPUT -p tcp --dport 9101 -j DROP
```

### Bootstrap order

The cluster needs a **majority** to elect a leader. With `--peers`
configured for 3 nodes, any two of the three together form a
majority and can elect; the third catches up when it starts.
(Starting all three at once is fine too — that's what
`bootstrap-cluster.sh` does.) A node started with **no** `--peers`
runs in standalone single-node leader mode.

### systemd unit (production)

Reference unit + env-file template live under
[`deploy/systemd/`](deploy/systemd/):

- [`deploy/systemd/oxide-kv@.service`](deploy/systemd/oxide-kv@.service) —
  template unit; instance name (after `@`) is the node-id
  (`node-1`, `node-2`, `node-3`).
- [`deploy/systemd/oxide-kv.env.example`](deploy/systemd/oxide-kv.env.example) —
  per-instance env file (`OXIDE_KV_ADDR`, `OXIDE_KV_PEERS`,
  `OXIDE_KV_DATA_DIR`, optional `OXIDE_SNAPSHOT_THRESHOLD_BYTES`).

Install steps (run on each node):

```bash
# 1. Create the user + dirs (one-time per host)
useradd --system --no-create-home --shell /usr/sbin/nologin oxide-kv
mkdir -p /var/lib/oxide-kv/node-X /var/log/oxide-kv /etc/oxide-kv
chown -R oxide-kv:oxide-kv /var/lib/oxide-kv /var/log/oxide-kv
chmod 0750 /var/lib/oxide-kv /var/log/oxide-kv

# 2. Install the binary (build on one host, scp the binary)
install -m 0755 target/release/oxide-kv /usr/local/bin/oxide-kv

# 3. Install the unit + env file
install -m 0644 deploy/systemd/oxide-kv@.service /etc/systemd/system/
cp deploy/systemd/oxide-kv.env.example /etc/oxide-kv/node-1.env
# Edit /etc/oxide-kv/node-1.env to set this host's IP / peers
chmod 0640 /etc/oxide-kv/node-1.env

# 4. Enable + start
systemctl daemon-reload
systemctl enable --now oxide-kv@node-1.service

# 5. Verify
systemctl status oxide-kv@node-1.service
journalctl -u oxide-kv@node-1 -f
```

The unit applies a tight sandbox (`NoNewPrivileges`,
`ProtectSystem=strict`, `ReadWritePaths=` whitelisted to
`/var/lib/oxide-kv` + `/var/log/oxide-kv`, etc.) so a
vulnerability in the binary can't trivially escalate.

### Local development / quick-start

For a single-host 3-node cluster (laptop, CI smoke test),
use [`deploy/scripts/bootstrap-cluster.sh`](deploy/scripts/bootstrap-cluster.sh):

```bash
cargo build --release
./deploy/scripts/bootstrap-cluster.sh start
# Logs: /tmp/oxide-kv-node-{1,2,3}.log
./deploy/scripts/bootstrap-cluster.sh status

# Talk to the cluster:
python3 python/examples/oxide_kv_demo.py
# Or via the Rust CLI:
cargo run --release --example oxide_kv_cli -- set hello world

./deploy/scripts/bootstrap-cluster.sh stop     # shut down
./deploy/scripts/bootstrap-cluster.sh clean    # nuke data dirs
```

### Pre-flight checklist

Before going to production, verify each box:

```
[ ] 3 hosts with internal network connectivity between them
[ ] OS user `oxide-kv` created, no shell, no home
[ ] /var/lib/oxide-kv/<node-id> created, owned oxide-kv:oxide-kv, mode 0750
[ ] /var/log/oxide-kv exists, owned oxide-kv:oxide-kv
[ ] /etc/oxide-kv/<node-id>.env exists, mode 0640, with correct IP / peers
[ ] Firewall: 9001/9101 reachable from cluster + app hosts, dropped elsewhere
[ ] chrony / ntpd running, drift < 100ms across all 3 hosts
[ ] systemd unit installed, enabled, started
[ ] Bootstrap order: first 2 nodes started before the 3rd
[ ] Verification: 30s after startup, one node is leader, term stable, no election churn
```

### Monitoring

Every node serves Prometheus metrics on `--metrics-addr` (default
`127.0.0.1:9100`; the bootstrap script staggers node-N to
`127.0.0.1:(9000 + N*100)`), plus a `GET /health` liveness probe.

Registered metrics:

| Metric | Meaning |
|---|---|
| `oxide_raft_term` | Current Raft term |
| `oxide_raft_role` | 0=follower, 1=candidate, 2=leader |
| `oxide_raft_commit_index` | Highest committed log index |
| `oxide_raft_last_applied` | Highest entry applied to the state machine |
| `oxide_raft_log_length` | Current Raft log length |
| `oxide_raft_snapshot_age_seconds` | Age of the latest snapshot |
| `oxide_raft_snapshot_bytes` | Size of the latest snapshot |
| `oxide_peer_match_index{peer=...}` | Per-peer replicated index (leader only) |
| `oxide_peer_next_index{peer=...}` | Per-peer next index (leader only) |
| `oxide_tx_pending_count` | In-flight 2PC transactions |
| `oxide_tx_timeout_aborted_total` | Tx aborted by the timeout sweep |
| `oxide_tx_admin_aborted_total` | Tx aborted via the `AbortTx` RPC |

Suggested alerts:

- `oxide_raft_role == 2` on more than one node at once → split brain.
- No node with `oxide_raft_role == 2` for > 2 × election timeout
  (~20s) → no leader.
- Leader's `oxide_peer_match_index` lagging `oxide_raft_commit_index`
  by > 100 → stuck / slow peer.
- `oxide_raft_snapshot_age_seconds` growing without bound while
  `oxide_raft_log_length` also grows → auto-compaction not firing;
  check `OXIDE_SNAPSHOT_THRESHOLD_BYTES`.

### Backup & restore

The on-disk format is:

```
/var/lib/oxide-kv/<node-id>/
├── node_<raft-addr>.wal            # Raft log (bincode), truncated on snapshot
├── node_<raft-addr>_meta.json      # term + voted-for; atomic rename
├── node_<raft-addr>_snapshot.json  # latest snapshot (if any)
├── memtable.wal                    # state-machine op log (JSON lines)
└── sst/                            # LSM SSTables
    ├── 000000.sst + 000000.sst.meta
    └── ...
```

To restore onto a fresh node: copy the **entire** data dir from
any peer that has been continuously in the cluster. The new node
will load the latest snapshot (if any), replay the remaining
`node_<addr>.wal` + `memtable.wal`, and discover SSTables from
`sst/` on startup (auto log compaction makes this
O(snapshot_interval) not O(total_writes)).

### Upgrade procedure

Membership changes are available via joint consensus (`AddNode` /
`RemoveNode`), so a replacement node can join before the old one
leaves. For a plain binary upgrade in place:

1. Stop one follower (`systemctl stop oxide-kv@node-X.service`).
2. Replace its binary at `/usr/local/bin/oxide-kv`.
3. Start it (`systemctl start oxide-kv@node-X.service`).
4. Wait ~5s for it to catch up (check `oxide_peer_match_index` on
   the leader).
5. Repeat for the second follower, then **finally** the leader
   (the leader is the last to be replaced; stopping it triggers
   an election on the other two, the new leader is chosen, and
   the upgraded binary joins as a follower).

Verify after each step: term is stable across the cluster for
30s with no spurious elections in the journal.

### Startup behaviour note

On a fresh bootstrap the cluster elects a leader within a few
seconds. Early P8 builds could churn through several terms while
three followers woke up in overlapping election windows; that was
closed by pre-vote (Raft §9.6) plus the same-term demote fix, and
the cross-process smoke test (`single_leader_converges_within_15_seconds`)
now guards it in CI. Reads / writes issued before a leader exists
are rejected with `"Not a leader"` — clients should retry (the
Python SDK's `Client.discover` and the demo script handle this).

---

---

## Running the simulation (P7 DST)

The fuzz harness in `tests/raft_fuzz.rs` runs the real RaftNode code
under deterministic fault injection (kill / restart / partition /
heal / election / yield) and cross-checks post-run state against the
safety invariant checker and a sequential reference model.

### Default CI

PR / push runs the 5 default sweeps (~601 scenarios, ~8 min):

```text
fuzz_default_seeds_0_to_200       200 scenarios × 25 actions
fuzz_default_seeds_1000_to_1200   200 scenarios × 25 actions
fuzz_long_seeds_2000_to_2100      100 scenarios × 50 actions
fuzz_short_seeds_3000_to_3100     100 scenarios × 5 actions
fuzz_smoke_single_seed            1 trivial scenario
```

### Nightly sweep

A separate `#[ignore]` entry runs a fresh 1000-scenario sweep,
driven by `.github/workflows/nightly.yml` on a daily cron
(02:00 UTC). Locally:

```bash
cargo test --release --test raft_fuzz -- \
  --ignored fuzz_nightly_seeds_10000_to_11000 --nocapture
```

### Reproducing a failing seed

When a fuzz test panics, the message includes the failing seed and
the full action sequence. To turn that into a minimal repro:

```bash
OXIDE_FUZZ_SEED=<seed> OXIDE_FUZZ_LEN=<len> \
  cargo test --release --test raft_fuzz shrink_repro -- \
  --ignored --nocapture
```

The shrinker (`PR #30`) applies delta debugging — chunk removal
followed by single-element removal — and emits a copy-pasteable
`Vec<Action>` literal you can drop into a regression test.

### Action vocabulary

The fuzzer draws actions from this set. Each action is parameterized
with a random sample from the seeded RNG so the same seed always
produces the same sequence.

| Action | What it does |
|---|---|
| `SubmitSet { key, value }` | propose a `Set` op on the current leader |
| `SubmitDelete { key }` | propose a `Delete` op on the current leader |
| `SubmitTx { tx_id, ops }` | drive a real 2PC round (`BeginTx` → vote fan-out → `DecideTx`) on the leader via the production coordinator |
| `DriveElection { candidate_idx }` | force node `candidate_idx` to start a new election |
| `KillNode { idx }` | crash node `idx` (its heartbeat / serve loops stop, on-disk WAL/meta preserved) |
| `RestartNode { idx }` | bring a killed node back; reloads WAL, restarts loops |
| `PartitionLink { from, to }` | drop messages on the directed link `from -> to` |
| `HealPartitions` | restore every dropped link |
| `Yield` | let the runtime advance a few ticks (so heartbeats / replication can propagate) |

Default distribution: 30% plain ops, 12% 2PC tx, 18% kill/restart,
18% partition/heal, 12% election, 10% yield. After every op
action the harness gives the cluster a beat to replicate before
cross-checking. The 2PC round is bounded to 1s; a timeout cancels
the round (worst case a `BeginTx` is committed with no
`DecideTx`, which both the 2PC-atomicity invariant and the
reference model tolerate).

### From panic to regression test

When a fuzz scenario fails, the panic message contains:

1. **The failure mode** — `invariant violation: ...`, `reference model mismatch on leader nN ...`, or `follower nN log diverges from leader ...`.
2. **The seed and action length** — pick these out of the panic header and run the shrinker.
3. **The full action sequence** — preserved verbatim for diff against the shrunk sequence.

To convert a failing seed into a locked-in regression test:

```bash
# Step 1: shrink. Prints a minimal Vec<Action> literal.
OXIDE_FUZZ_SEED=<seed> OXIDE_FUZZ_LEN=<len> \
  cargo test --release --test raft_fuzz shrink_repro -- \
  --ignored --nocapture
```

The shrinker output ends with a block like:

```rust
let actions = vec![
    Action::SubmitSet { key: "k2".into(), value: "v1".into() },
    Action::PartitionLink { from: 0, to: 1 },
    Action::RestartNode { idx: 1 },
];
run_actions(&actions).await.unwrap();
```

Paste it into a new `#[tokio::test]` in `tests/raft_fuzz.rs` (or a
new `tests/regressions_p7.rs` if you prefer to keep fuzz tests
separate). The minimal sequence still reproduces the failure
under `cargo test`, so a follow-up fix PR can verify the
regression is gone with a single `cargo test --test raft_fuzz`.

If a real bug is surfaced, file the fix in its own
`fix/...` branch — the P7 fuzz harness *tests* the existing
code; fixes don't belong inside the harness PRs.

---

## Roadmap

The active roadmap lives in [ROADMAP.md](./ROADMAP.md). That file is
the single source of truth for phase status, in-progress work, and
acceptance criteria.

### Phase summary

| Phase | Capability | Status | PR |
|---|---|---|---|
| P0 | LICENSE, CHANGELOG, unit tests, warning cleanup | ✅ | [#1](https://github.com/yuw1/oxide-kv/pull/1) |
| P1 | Snapshot + InstallSnapshot RPC + log compaction | ✅ | [#2](https://github.com/yuw1/oxide-kv/pull/2) |
| P2 | Linearizable reads via ReadIndex | ✅ | [#3](https://github.com/yuw1/oxide-kv/pull/3) |
| P3 | Protobuf binary RPC | ✅ | [#4](https://github.com/yuw1/oxide-kv/pull/4) |
| P4 | LSM-Tree state machine | ✅ | [#5](https://github.com/yuw1/oxide-kv/pull/5) |
| P5 | 2PC lifecycle | ✅ | [#6](https://github.com/yuw1/oxide-kv/pull/6) |
| Bug | Election timer brain-split + heartbeat:election ratio | ✅ | [#8](https://github.com/yuw1/oxide-kv/pull/8) |
| Bug | Single-node read fallback + commit advancement | ✅ | [#9](https://github.com/yuw1/oxide-kv/pull/9) |
| P6 | Multi-node 2PC coordinator RPC | ✅ | [#11](https://github.com/yuw1/oxide-kv/pull/11)–[#14](https://github.com/yuw1/oxide-kv/pull/14) |
| P7 | Deterministic simulation testing (DST) | ✅ | [#28](https://github.com/yuw1/oxide-kv/pull/28)–[#32](https://github.com/yuw1/oxide-kv/pull/32) |
| P8 | Pre-vote, joint consensus, tx timeout, metrics, prod cluster test | ✅ | [#33](https://github.com/yuw1/oxide-kv/pull/33)–[#43](https://github.com/yuw1/oxide-kv/pull/43) (+ [#50](https://github.com/yuw1/oxide-kv/pull/50)/[#51](https://github.com/yuw1/oxide-kv/pull/51)) |

Candidate future directions (see ROADMAP.md "Future directions"):
sharded multi-Raft, gRPC transport, TLS, LSM polish (bloom filters,
block cache, background compaction), client-side leader re-discovery,
benchmark suite.

---

## License

Apache-2.0. See [LICENSE](./LICENSE).