# Oxide-KV: A Distributed Key-Value Store in Rust

A lightweight, reliable distributed key-value store built from scratch in Rust, implementing the **Raft Consensus Algorithm**. This project focuses on high availability, consistency, and crash recovery.

## 🚀 Key Features

* **Leader Election**: Robust election mechanism with randomized timeouts and term-based logic.
* **Election Restriction**: Implements the safety property ensuring only nodes with up-to-date logs can be elected as Leader.
* **Persistent Storage**: Automatic recovery of Metadata (Term, VoteFor) and Write-Ahead Logs (WAL) from disk upon restart.
* **State Machine Replication**: Synchronizes commands across the cluster via heartbeats and log entries.
* **Asynchronous Architecture**: Powered by `Tokio` for high-performance RPC handling and concurrent client requests.

## 🏗️ System Architecture

### Node States

* **Follower**: Passive state; responds to RPCs from Leaders and Candidates.
* **Candidate**: Active state used to elect a new Leader.
* **Leader**: Handles all client requests and manages log replication.

### RPC Protocols

The system communicates using two primary Raft RPCs:

1. **RequestVote**: Initiated by Candidates during elections.
2. **AppendEntries**: Initiated by Leaders to replicate log entries and serve as heartbeats.

### Storage Engine

* **WAL (Write-Ahead Log)**: Every command is appended to a disk-based log before being applied to the state machine.
* **Snapshot (JSON)**: Periodically saves the current state of the KV store to disk for fast recovery.

## 🛠️ Quick Start

### 1. Prerequisites

Ensure you have the Rust toolchain installed (Edition 2021+).

### 2. Run the Cluster

Open three terminals to start a local 3-node cluster:

**Node 1 (8001):**

```bash
cargo run -- --addr 127.0.0.1:8001 --client-addr 127.0.0.1:9001 --peers 127.0.0.1:8002 127.0.0.1:8003

```

**Node 2 (8002):**

```bash
cargo run -- --addr 127.0.0.1:8002 --client-addr 127.0.0.1:9002 --peers 127.0.0.1:8001 127.0.0.1:8003

```

**Node 3 (8003):**

```bash
cargo run -- --addr 127.0.0.1:8003 --client-addr 127.0.0.1:9003 --peers 127.0.0.1:8001 127.0.0.1:8002

```

### 3. Client Interaction

Connect to the **Leader** node's client address (e.g., 9001) using `nc` or any TCP client:

```bash
# Set a value
echo '{"Set":{"key":"hello","value":"world"}}' | nc 127.0.0.1 9001

# Get a value
echo '{"Get":{"key":"hello"}}' | nc 127.0.0.1 9001

```

## 🔍 Safety & Consistency

The core voting logic (`handle_request_vote`) strictly follows **Section 5.4.1** of the Raft paper to ensure the **Leader Completeness Property**:

* **Term Check**: Candidates with an outdated `current_term` are immediately rejected.
* **Log Matching**: A node will only vote for a candidate if the candidate's log is at least as up-to-date as its own.
* Comparison: `(last_log_term > my_term) || (last_log_term == my_term && last_log_index >= my_index)`



## 📝 Roadmap / TODO

* [ ] **Linearizable Reads**: Implement `ReadIndex` or `Leasing` mechanisms to ensure GET requests always return the most recent committed data, preventing "stale reads" during network partitions without the overhead of disk I/O.
* [ ] **Log Compaction**: Develop a snapshotting system to serialize the current StateMachine state and truncate the WAL (Write-Ahead Log), reclaiming disk space and significantly reducing node recovery time.
* [ ] **Binary Protocol (Protobuf/gRPC)**: Migrate from JSON to a high-performance binary serialization layer using Protobuf and gRPC to minimize network latency and CPU overhead during cross-node communication.
* [ ] **LSM-Tree Storage Engine**: Replace the in-memory HashMap with a Log-Structured Merge-Tree (LSM) to handle high-write throughput via MemTables, SSTables, and background compaction processes.
* [ ] **Distributed Transactions (2PC)**: Implement a Two-Phase Commit protocol to ensure atomic operations across multiple shards or keys, maintaining ACID guarantees in a truly distributed environment.