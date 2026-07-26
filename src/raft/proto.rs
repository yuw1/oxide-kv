//! Generated protobuf types for the Raft wire protocol, plus conversions to/from
//! the in-process domain types defined in `crate::protocol` and `crate::raft::rpc`.
//!
//! Domain types (e.g. `Command`, `LogEntry`, `Snapshot`) keep their serde-based
//! in-memory and on-disk formats (used by `raft/storage`, the WAL, the snapshot
//! file, and the external client JSON protocol). Only the bytes that travel on
//! the inter-node TCP socket use this protobuf encoding.

use crate::protocol::{
    Command as DomainCommand, LogEntry as DomainLogEntry, Snapshot as DomainSnapshot,
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
        };
        pb::Command { body: Some(body) }
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
            Some(pb::command::Body::Set(s)) => s.into(),
            Some(pb::command::Body::Get(g)) => g.into(),
            Some(pb::command::Body::Delete(d)) => d.into(),
            Some(pb::command::Body::Compact(e)) => e.into(),
            None => DomainCommand::Compact, // No payload → treat as no-op
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