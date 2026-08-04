//! Generated protobuf types for the Raft wire protocol, plus conversions to/from
//! the in-process domain types defined in `crate::protocol` and `crate::raft::rpc`.
//!
//! Domain types (e.g. `Command`, `LogEntry`, `Snapshot`) keep their serde-based
//! in-memory and on-disk formats (used by `raft/storage`, the WAL, the snapshot
//! file, and the external client JSON protocol). Only the bytes that travel on
//! the inter-node TCP socket use this protobuf encoding.

use crate::protocol::{
    Command as DomainCommand, Configuration as DomainConfiguration, LogEntry as DomainLogEntry,
    ServerId as DomainServerId, Snapshot as DomainSnapshot,
    TxDecision as DomainTxDecision, TxOp as DomainTxOp,
};
use crate::raft::rpc::{
    AppendEntriesArgs as DomainAppendArgs, AppendReplyArgs as DomainAppendReply,
    InstallSnapshotArgs as DomainInstallArgs, InstallSnapshotReplyArgs as DomainInstallReply,
    RaftMessage as DomainRaftMessage, RequestVoteArgs as DomainRequestVote,
    VoteResponseArgs as DomainVote,
};

/// Pull in the prost-generated module. The file path is fixed by
/// `prost-build` based on the `package` declaration in `proto/raft.proto`.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/oxide_kv.raft.rs"));
}

// =========================================================================
// Domain → proto
// =========================================================================

impl From<&DomainCommand> for pb::Command {
    fn from(c: &DomainCommand) -> Self {
        let body = match c {
            DomainCommand::Set { key, value } => pb::command::Body::Set(pb::Set {
                key: key.clone(),
                value: value.clone(),
            }),
            DomainCommand::Get { key } => pb::command::Body::Get(pb::Get { key: key.clone() }),
            DomainCommand::Delete { key } => {
                pb::command::Body::Delete(pb::Delete { key: key.clone() })
            }
            DomainCommand::Compact => pb::command::Body::Compact(pb::Empty {}),
            DomainCommand::BeginTx { tx_id, ops } => {
                pb::command::Body::BeginTx(pb::BeginTx {
                    tx_id: tx_id.clone(),
                    ops: ops.iter().map(|o| o.into()).collect(),
                })
            }
            // `Command::Vote` was removed in P6 (see `proto/coordination.proto`).
            // Votes now travel on the side-channel RPC; the Raft log carries
            // only `BeginTx` and `DecideTx`.
            DomainCommand::DecideTx { tx_id, decision } => {
                pb::command::Body::DecideTx(pb::DecideTx {
                    tx_id: tx_id.clone(),
                    commit: matches!(decision, DomainTxDecision::Commit),
                })
            }
            // P8 PR 6: client-facing membership commands.
            // The leader's MembershipCoordinator intercepts these and
            // replaces them with `InstallConfiguration` log entries
            // before replication. In normal operation these never
            // appear on the wire, but the encoding exists so the
            // type is closed and the proto schema matches the
            // Rust enum.
            DomainCommand::AddNode { server } => {
                pb::command::Body::AddNode(server.into())
            }
            DomainCommand::RemoveNode { node_id } => {
                pb::command::Body::RemoveNode(node_id.clone())
            }
            DomainCommand::InstallConfiguration { config } => {
                pb::command::Body::InstallConfiguration(config.into())
            }
        };
        pb::Command { body: Some(body) }
    }
}

impl From<&DomainServerId> for pb::ServerId {
    fn from(s: &DomainServerId) -> Self {
        pb::ServerId {
            node_id: s.node_id.clone(),
            addr: s.addr.clone(),
        }
    }
}

impl From<pb::ServerId> for DomainServerId {
    fn from(s: pb::ServerId) -> Self {
        DomainServerId {
            node_id: s.node_id,
            addr: s.addr,
        }
    }
}

impl From<&DomainConfiguration> for pb::ConfigurationEntry {
    fn from(c: &DomainConfiguration) -> Self {
        let (kind, old_servers, new_servers) = match c {
            DomainConfiguration::Simple(servers) => {
                (pb::configuration_entry::Kind::Simple, vec![], servers.iter().map(|s| s.into()).collect())
            }
            DomainConfiguration::Joint { old, new } => (
                pb::configuration_entry::Kind::Joint,
                old.iter().map(|s| s.into()).collect(),
                new.iter().map(|s| s.into()).collect(),
            ),
        };
        pb::ConfigurationEntry {
            kind: kind as i32,
            old_servers,
            new_servers,
        }
    }
}

impl From<pb::ConfigurationEntry> for DomainConfiguration {
    fn from(c: pb::ConfigurationEntry) -> Self {
        // Read `kind` first, then consume the server lists. Reading
        // `c.kind()` after `into_iter` on the server fields would be
        // a partial-move error.
        let kind = c.kind();
        let old: Vec<DomainServerId> = c.old_servers.into_iter().map(|s| s.into()).collect();
        let new: Vec<DomainServerId> = c.new_servers.into_iter().map(|s| s.into()).collect();
        match kind {
            pb::configuration_entry::Kind::Simple => {
                // Defensive: a Simple entry with no servers is degenerate
                // (no quorum possible). Treat as empty Simple so the
                // cluster can't make progress rather than panicking.
                DomainConfiguration::Simple(new)
            }
            pb::configuration_entry::Kind::Joint => DomainConfiguration::Joint { old, new },
            pb::configuration_entry::Kind::Unspecified => {
                // Forward-compat: unknown kind defaults to Simple(new),
                // same as a Simple entry.
                DomainConfiguration::Simple(new)
            }
        }
    }
}

impl From<&DomainTxOp> for pb::TxOp {
    fn from(op: &DomainTxOp) -> Self {
        let body = match op {
            DomainTxOp::Put { key, value } => pb::tx_op::Body::Put(pb::Set {
                key: key.clone(),
                value: value.clone(),
            }),
            DomainTxOp::Delete { key } => pb::tx_op::Body::Delete(pb::Delete { key: key.clone() }),
        };
        pb::TxOp { body: Some(body) }
    }
}

impl From<&DomainLogEntry> for pb::LogEntry {
    fn from(e: &DomainLogEntry) -> Self {
        pb::LogEntry {
            term: e.term,
            index: e.index as u64,
            command: Some((&e.command).into()),
        }
    }
}

impl From<&DomainRequestVote> for pb::RequestVoteArgs {
    fn from(a: &DomainRequestVote) -> Self {
        pb::RequestVoteArgs {
            term: a.term,
            candidate_id: a.candidate_id.clone(),
            last_log_index: a.last_log_index,
            last_log_term: a.last_log_term,
        }
    }
}

impl From<&DomainVote> for pb::VoteResponseArgs {
    fn from(a: &DomainVote) -> Self {
        pb::VoteResponseArgs {
            term: a.term,
            vote_granted: a.vote_granted,
        }
    }
}

impl From<&DomainAppendArgs> for pb::AppendEntriesArgs {
    fn from(a: &DomainAppendArgs) -> Self {
        pb::AppendEntriesArgs {
            term: a.term,
            leader_id: a.leader_id.clone(),
            prev_log_index: a.prev_log_index,
            prev_log_term: a.prev_log_term,
            entries: a.entries.iter().map(|e| e.into()).collect(),
            leader_commit: a.leader_commit,
        }
    }
}

impl From<&DomainAppendReply> for pb::AppendReplyArgs {
    fn from(a: &DomainAppendReply) -> Self {
        pb::AppendReplyArgs {
            term: a.term,
            success: a.success,
        }
    }
}

impl From<&DomainSnapshot> for pb::Snapshot {
    fn from(s: &DomainSnapshot) -> Self {
        pb::Snapshot {
            last_included_index: s.last_included_index,
            last_included_term: s.last_included_term,
            data: s.data.clone(),
        }
    }
}

impl From<&DomainInstallArgs> for pb::InstallSnapshotArgs {
    fn from(a: &DomainInstallArgs) -> Self {
        pb::InstallSnapshotArgs {
            term: a.term,
            leader_id: a.leader_id.clone(),
            last_included_index: a.last_included_index,
            last_included_term: a.last_included_term,
            snapshot: Some((&a.snapshot).into()),
        }
    }
}

impl From<&DomainInstallReply> for pb::InstallSnapshotReplyArgs {
    fn from(a: &DomainInstallReply) -> Self {
        pb::InstallSnapshotReplyArgs { term: a.term }
    }
}

/// Top-level envelope: wrap a domain `RaftMessage` variant in its proto
/// counterpart for transport.
pub fn encode_domain(domain: &DomainRaftMessage) -> pb::RaftMessage {
    use DomainRaftMessage as DM;
    let body = match domain {
        DM::RequestVote(a) => pb::raft_message::Body::RequestVote(a.into()),
        DM::VoteResponse(a) => pb::raft_message::Body::VoteResponse(a.into()),
        DM::AppendEntries(a) => pb::raft_message::Body::AppendEntries(a.into()),
        DM::AppendReply(a) => pb::raft_message::Body::AppendReply(a.into()),
        DM::InstallSnapshot(a) => pb::raft_message::Body::InstallSnapshot(a.into()),
        DM::InstallSnapshotReply(a) => pb::raft_message::Body::InstallSnapshotReply(a.into()),
        // Pre-vote (P8 PR 5, Raft §9.6): reuse the RequestVoteArgs /
        // VoteResponseArgs wire types; the receiver distinguishes
        // them by the RaftMessage.body tag (7 / 8).
        DM::RequestPreVote(a) => pb::raft_message::Body::RequestPreVote(a.into()),
        DM::PreVoteResponse(a) => pb::raft_message::Body::PreVoteResponse(a.into()),
    };
    pb::RaftMessage { body: Some(body) }
}

// =========================================================================
// Proto → domain
// =========================================================================

impl From<pb::Set> for DomainCommand {
    fn from(p: pb::Set) -> Self {
        DomainCommand::Set { key: p.key, value: p.value }
    }
}

impl From<pb::Get> for DomainCommand {
    fn from(p: pb::Get) -> Self {
        DomainCommand::Get { key: p.key }
    }
}

impl From<pb::Delete> for DomainCommand {
    fn from(p: pb::Delete) -> Self {
        DomainCommand::Delete { key: p.key }
    }
}

impl From<pb::Empty> for DomainCommand {
    fn from(_: pb::Empty) -> Self {
        DomainCommand::Compact
    }
}

impl From<pb::Command> for DomainCommand {
    fn from(p: pb::Command) -> Self {
        match p.body {
            Some(pb::command::Body::Set(s)) => DomainCommand::Set { key: s.key, value: s.value },
            Some(pb::command::Body::Get(g)) => DomainCommand::Get { key: g.key },
            Some(pb::command::Body::Delete(d)) => DomainCommand::Delete { key: d.key },
            Some(pb::command::Body::Compact(_)) => DomainCommand::Compact,
            Some(pb::command::Body::BeginTx(b)) => DomainCommand::BeginTx {
                tx_id: b.tx_id,
                ops: b.ops.into_iter().map(|o| o.into()).collect(),
            },
            // `pb::command::Body::TxVote` was removed in P6 alongside
            // `Command::Vote` (see `proto/coordination.proto`). If a stale
            // peer ever sends one, treat it as a no-op rather than panicking;
            // the side-channel RPC is the only legal vote path going forward.
            Some(pb::command::Body::DecideTx(d)) => {
                let decision = if d.commit {
                    DomainTxDecision::Commit
                } else {
                    DomainTxDecision::Abort
                };
                DomainCommand::DecideTx { tx_id: d.tx_id, decision }
            }
            // P8 PR 6 inverse. AddNode/RemoveNode on the wire
            // only happens when a client tries to send one to a
            // Follower (the leader intercepts AddNode/RemoveNode
            // before they ever leave the source process in normal
            // operation). Treat as Compact (no-op) to keep the
            // cluster safe; the leader returns a "not the leader"
            // response at the JSON layer.
            Some(pb::command::Body::AddNode(_)) => DomainCommand::Compact,
            Some(pb::command::Body::RemoveNode(_)) => DomainCommand::Compact,
            Some(pb::command::Body::InstallConfiguration(c)) => {
                DomainCommand::InstallConfiguration { config: c.into() }
            }
            None => DomainCommand::Compact, // No payload → treat as no-op
        }
    }
}

impl From<pb::TxOp> for DomainTxOp {
    fn from(p: pb::TxOp) -> Self {
        match p.body {
            Some(pb::tx_op::Body::Put(s)) => DomainTxOp::Put { key: s.key, value: s.value },
            Some(pb::tx_op::Body::Delete(d)) => DomainTxOp::Delete { key: d.key },
            None => DomainTxOp::Delete { key: String::new() }, // best-effort no-op
        }
    }
}

impl From<pb::LogEntry> for DomainLogEntry {
    fn from(p: pb::LogEntry) -> Self {
        DomainLogEntry {
            term: p.term,
            index: p.index as usize,
            command: p.command.map(|c| c.into()).unwrap_or(DomainCommand::Compact),
        }
    }
}

impl From<pb::RequestVoteArgs> for DomainRequestVote {
    fn from(p: pb::RequestVoteArgs) -> Self {
        DomainRequestVote {
            term: p.term,
            candidate_id: p.candidate_id,
            last_log_index: p.last_log_index,
            last_log_term: p.last_log_term,
        }
    }
}

impl From<pb::VoteResponseArgs> for DomainVote {
    fn from(p: pb::VoteResponseArgs) -> Self {
        DomainVote {
            term: p.term,
            vote_granted: p.vote_granted,
        }
    }
}

impl From<pb::AppendEntriesArgs> for DomainAppendArgs {
    fn from(p: pb::AppendEntriesArgs) -> Self {
        DomainAppendArgs {
            term: p.term,
            leader_id: p.leader_id,
            prev_log_index: p.prev_log_index,
            prev_log_term: p.prev_log_term,
            entries: p.entries.into_iter().map(|e| e.into()).collect(),
            leader_commit: p.leader_commit,
        }
    }
}

impl From<pb::AppendReplyArgs> for DomainAppendReply {
    fn from(p: pb::AppendReplyArgs) -> Self {
        DomainAppendReply {
            term: p.term,
            success: p.success,
        }
    }
}

impl From<pb::Snapshot> for DomainSnapshot {
    fn from(p: pb::Snapshot) -> Self {
        DomainSnapshot {
            last_included_index: p.last_included_index,
            last_included_term: p.last_included_term,
            data: p.data,
        }
    }
}

impl From<pb::InstallSnapshotArgs> for DomainInstallArgs {
    fn from(p: pb::InstallSnapshotArgs) -> Self {
        let snapshot = p.snapshot.map(|s| s.into()).unwrap_or(DomainSnapshot {
            last_included_index: p.last_included_index,
            last_included_term: p.last_included_term,
            data: Default::default(),
        });
        DomainInstallArgs {
            term: p.term,
            leader_id: p.leader_id,
            last_included_index: p.last_included_index,
            last_included_term: p.last_included_term,
            snapshot,
        }
    }
}

impl From<pb::InstallSnapshotReplyArgs> for DomainInstallReply {
    fn from(p: pb::InstallSnapshotReplyArgs) -> Self {
        DomainInstallReply { term: p.term }
    }
}

/// Decode a proto envelope back to the domain `RaftMessage` enum.
pub fn decode_domain(p: pb::RaftMessage) -> DomainRaftMessage {
    use DomainRaftMessage as DM;
    match p.body {
        Some(pb::raft_message::Body::RequestVote(a)) => DM::RequestVote(a.into()),
        Some(pb::raft_message::Body::VoteResponse(a)) => DM::VoteResponse(a.into()),
        Some(pb::raft_message::Body::AppendEntries(a)) => DM::AppendEntries(a.into()),
        Some(pb::raft_message::Body::AppendReply(a)) => DM::AppendReply(a.into()),
        Some(pb::raft_message::Body::InstallSnapshot(a)) => DM::InstallSnapshot(a.into()),
        Some(pb::raft_message::Body::InstallSnapshotReply(a)) => DM::InstallSnapshotReply(a.into()),
        // Pre-vote decode (P8 PR 5).
        Some(pb::raft_message::Body::RequestPreVote(a)) => DM::RequestPreVote(a.into()),
        Some(pb::raft_message::Body::PreVoteResponse(a)) => DM::PreVoteResponse(a.into()),
        None => {
            // A top-level RaftMessage with no payload should never arrive on the
            // wire. Treat as a vote response with term=0 to fail closed.
            DM::VoteResponse(DomainVote { term: 0, vote_granted: false })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::rpc::RaftMessage as DM;
    use prost::Message;

    #[test]
    fn envelope_roundtrips_through_proto() {
        let original = DM::AppendEntries(DomainAppendArgs {
            term: 7,
            leader_id: "n1".into(),
            prev_log_index: 4,
            prev_log_term: 6,
            entries: vec![
                DomainLogEntry {
                    term: 7,
                    index: 5,
                    command: DomainCommand::Set {
                        key: "k".into(),
                        value: "v".into(),
                    },
                },
                DomainLogEntry {
                    term: 7,
                    index: 6,
                    command: DomainCommand::Delete { key: "gone".into() },
                },
            ],
            leader_commit: 5,
        });

        let encoded = encode_domain(&original);
        let bytes = encoded.encode_to_vec();
        let decoded_pb = pb::RaftMessage::decode(&bytes[..]).expect("decode");
        let decoded = decode_domain(decoded_pb);

        assert_eq!(decoded, original);
    }

    #[test]
    fn all_message_variants_roundtrip() {
        let cases = vec![
            DM::RequestVote(DomainRequestVote {
                term: 1,
                candidate_id: "n2".into(),
                last_log_index: 10,
                last_log_term: 2,
            }),
            DM::VoteResponse(DomainVote { term: 3, vote_granted: true }),
            DM::AppendReply(DomainAppendReply { term: 4, success: false }),
            DM::InstallSnapshot(DomainInstallArgs {
                term: 5,
                leader_id: "n1".into(),
                last_included_index: 100,
                last_included_term: 5,
                snapshot: DomainSnapshot {
                    last_included_index: 100,
                    last_included_term: 5,
                    data: [("alpha".to_string(), "1".to_string())]
                        .into_iter()
                        .collect(),
                },
            }),
            DM::InstallSnapshotReply(DomainInstallReply { term: 5 }),
        ];

        for original in cases {
            let encoded = encode_domain(&original);
            let bytes = encoded.encode_to_vec();
            let decoded = pb::RaftMessage::decode(&bytes[..]).expect("decode");
            assert_eq!(decode_domain(decoded), original);
        }
    }

    #[test]
    fn command_variants_roundtrip() {
        let commands = vec![
            DomainCommand::Set { key: "k".into(), value: "v".into() },
            DomainCommand::Get { key: "k".into() },
            DomainCommand::Delete { key: "k".into() },
            DomainCommand::Compact,
        ];
        for c in commands {
            let encoded = pb::Command::from(&c);
            let bytes = encoded.encode_to_vec();
            let decoded = pb::Command::decode(&bytes[..]).expect("decode");
            assert_eq!(DomainCommand::from(decoded), c);
        }
    }

    #[test]
    fn protobuf_is_smaller_than_equivalent_json() {
        // Soft check: protobuf should beat JSON for a moderately-sized message.
        let payload = DM::AppendEntries(DomainAppendArgs {
            term: 9,
            leader_id: "node-1".into(),
            prev_log_index: 99,
            prev_log_term: 9,
            entries: (1..=20)
                .map(|i| DomainLogEntry {
                    term: 9,
                    index: 100 + i as usize,
                    command: DomainCommand::Set {
                        key: format!("key-{i:03}"),
                        value: format!("value-{i:03}-with-some-padding-to-make-it-realistic"),
                    },
                })
                .collect(),
            leader_commit: 120,
        });
        let proto_bytes = encode_domain(&payload).encode_to_vec();
        let json_bytes = serde_json::to_vec(&payload).unwrap();

        assert!(
            proto_bytes.len() < json_bytes.len(),
            "expected protobuf ({} B) < json ({} B)",
            proto_bytes.len(),
            json_bytes.len(),
        );
    }
}