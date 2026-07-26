use crate::config::Config;
use crate::protocol::{LogEntry, Snapshot};
pub use crate::raft::node::{NodeState, RaftNode};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum RaftMessage {
    RequestVote(RequestVoteArgs),
    VoteResponse(VoteResponseArgs),
    AppendEntries(AppendEntriesArgs),
    AppendReply(AppendReplyArgs),
    InstallSnapshot(InstallSnapshotArgs),
    InstallSnapshotReply(InstallSnapshotReplyArgs),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RequestVoteArgs {
    pub term: u64,
    pub candidate_id: String,
    pub last_log_index: u64,
    pub last_log_term: u64
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct VoteResponseArgs {
    pub term: u64,
    pub vote_granted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AppendEntriesArgs {
    pub term: u64,              // Leader's current term
    pub leader_id: String,      // Leader's ID so follower can redirect clients
    pub prev_log_index: u64,    // Index of log entry immediately preceding new ones
    pub prev_log_term: u64,     // Term of prev_log_index entry
    pub entries: Vec<LogEntry>, // Log entries to store (empty for heartbeat)
    pub leader_commit: u64,     // Leader's commit_index
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AppendReplyArgs {
    pub term: u64,    // Current term of follower, for leader to update itself
    pub success: bool, // True if follower contained entry matching prev_log_index and prev_log_term
}

/// Sent by the Leader to bring a lagging Follower (or a new Follower) up to
/// date by transmitting a complete state-machine snapshot.
///
/// Note: this implementation transmits the snapshot in a single message. For
/// very large state machines, chunked transfer (offset/done fields) should be
/// layered on top — see Raft thesis §7.2.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InstallSnapshotArgs {
    pub term: u64,
    pub leader_id: String,
    pub last_included_index: u64,
    pub last_included_term: u64,
    pub snapshot: Snapshot,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InstallSnapshotReplyArgs {
    pub term: u64,
}

pub struct RpcClient;

impl RpcClient {
    /// Internal helper to handle the common TCP transport logic.
    ///
    /// Wire format: 4-byte big-endian length prefix followed by a protobuf
    /// payload. Length prefix is mandatory because protobuf is a binary
    /// stream format with no self-delimiting frame boundary.
    async fn call(addr: &str, msg: RaftMessage, timeout_duration: Duration) -> anyhow::Result<RaftMessage> {
        let stream = timeout(timeout_duration, TcpStream::connect(addr)).await??;
        let (mut reader, mut writer) = stream.into_split();

        let payload = crate::raft::proto::encode_domain(&msg).encode_to_vec();
        write_framed(&mut writer, &payload).await?;
        let resp_buf = read_framed(&mut reader).await?;
        let resp_pb = crate::raft::proto::pb::RaftMessage::decode(&resp_buf[..])?;
        Ok(crate::raft::proto::decode_domain(resp_pb))
    }

    /// Send RequestVote RPC to a peer
    pub(crate) async fn send_request_vote_rpc(addr: &str, args: RequestVoteArgs) -> anyhow::Result<VoteResponseArgs> {
        match Self::call(addr, RaftMessage::RequestVote(args), Duration::from_secs(Config::rpc_request_vote_timeout_ms())).await? {
            RaftMessage::VoteResponse(reply) => Ok(reply),
            _ => Err(anyhow::anyhow!("Unexpected RPC response for RequestVote")),
        }
    }

    /// Send AppendEntries RPC to a peer
    pub(crate) async fn send_append_entries_rpc(addr: String, args: AppendEntriesArgs) -> anyhow::Result<AppendReplyArgs> {
        match Self::call(&addr, RaftMessage::AppendEntries(args), Duration::from_secs(Config::rpc_append_entries_timeout_ms())).await? {
            RaftMessage::AppendReply(reply) => Ok(reply),
            _ => Err(anyhow::anyhow!("Unexpected RPC response type for AppendEntries")),
        }
    }

    /// Send InstallSnapshot RPC to a peer
    #[allow(dead_code)] // Wired up by future per-peer InstallSnapshot orchestration.
    pub(crate) async fn send_install_snapshot_rpc(
        addr: String,
        args: InstallSnapshotArgs,
    ) -> anyhow::Result<InstallSnapshotReplyArgs> {
        match Self::call(
            &addr,
            RaftMessage::InstallSnapshot(args),
            Duration::from_secs(Config::rpc_append_entries_timeout_ms()),
        ).await? {
            RaftMessage::InstallSnapshotReply(reply) => Ok(reply),
            _ => Err(anyhow::anyhow!("Unexpected RPC response type for InstallSnapshot")),
        }
    }
}

pub struct RpcServer;

impl RpcServer {
    pub async fn handle_raft_rpc(mut stream: TcpStream, raft_node: Arc<RwLock<RaftNode>>) {
        // Standard error handling pattern for background tasks
        if let Err(e) = Self::handle_rpc_logic(&mut stream, raft_node).await {
            eprintln!("[Thread {:?}] Raft RPC handling error: {}", std::thread::current().id(), e);
        }
    }

    async fn handle_rpc_logic(stream: &mut TcpStream, raft_node: Arc<RwLock<RaftNode>>) -> Result<(), Box<dyn std::error::Error>> {
        let (mut reader, mut writer) = stream.split();

        // 1. Read a length-prefixed protobuf frame from the stream.
        let req_buf = read_framed(&mut reader).await?;
        if req_buf.is_empty() { return Ok(()); }

        // 2. Decode to the domain enum.
        let req_pb = crate::raft::proto::pb::RaftMessage::decode(&req_buf[..])?;
        let msg: RaftMessage = crate::raft::proto::decode_domain(req_pb);

        // 3. Dispatch and reply (also length-prefixed protobuf).
        let response = match msg {
            RaftMessage::RequestVote(args) => {
                let reply = {
                    let mut node = raft_node.write().unwrap();
                    node.handle_request_vote(&args)
                };
                println!("✅ Responded to vote request from node {}", args.candidate_id);
                RaftMessage::VoteResponse(reply)
            }
            RaftMessage::AppendEntries(args) => {
                let reply = {
                    let mut node = raft_node.write().unwrap();
                    node.handle_append_entries(&args)
                };
                println!("✅ Responded to heartbeat from Leader {} (Term {})", args.leader_id, args.term);
                RaftMessage::AppendReply(reply)
            }
            RaftMessage::InstallSnapshot(args) => {
                let reply = {
                    let mut node = raft_node.write().unwrap();
                    node.handle_install_snapshot(&args)
                };
                println!("📦 Responded to InstallSnapshot from Leader {} (Term {})", args.leader_id, args.term);
                RaftMessage::InstallSnapshotReply(reply)
            }
            other => {
                println!("⚠️ Received unexpected RPC message type: {:?}", other);
                return Ok(());
            }
        };

        let resp_payload = crate::raft::proto::encode_domain(&response).encode_to_vec();
        write_framed(&mut writer, &resp_payload).await?;
        Ok(())
    }
}

/// Write a single length-prefixed protobuf frame.
pub(crate) async fn write_framed<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    writer.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single length-prefixed protobuf frame. Returns an empty Vec on EOF.
pub(crate) async fn read_framed<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(0) => return Ok(Vec::new()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Vec::new()),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use tokio::io::duplex;

    #[tokio::test]
    async fn framed_roundtrip_small_payload() {
        let payload = b"hello-protobuf";
        let (mut a, mut b) = duplex(1024);

        write_framed(&mut a, payload).await.unwrap();
        let received = read_framed(&mut b).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn framed_roundtrip_large_payload() {
        // Force the 4-byte length prefix to exceed one byte.
        let payload: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let (mut a, mut b) = duplex(256 * 1024);

        write_framed(&mut a, &payload).await.unwrap();
        let received = read_framed(&mut b).await.unwrap();
        assert_eq!(received.len(), payload.len());
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn framed_read_returns_empty_on_eof() {
        let (a, mut b) = duplex(16);
        drop(a);
        let received = read_framed(&mut b).await.unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn end_to_end_rpc_over_duplex() {
        // Verify the full client/server wire path on an in-memory duplex:
        // domain -> proto -> framed bytes -> proto -> domain.
        use crate::raft::proto::{decode_domain, encode_domain};
        use crate::raft::rpc::{AppendEntriesArgs, VoteResponseArgs};

        let original = RaftMessage::AppendEntries(AppendEntriesArgs {
            term: 42,
            leader_id: "n1".into(),
            prev_log_index: 7,
            prev_log_term: 41,
            entries: vec![],
            leader_commit: 7,
        });

        let (mut client_io, mut server_io) = duplex(64 * 1024);

        // Client writes the request.
        let payload = encode_domain(&original).encode_to_vec();
        write_framed(&mut client_io, &payload).await.unwrap();

        // Server reads and decodes it.
        let buf = read_framed(&mut server_io).await.unwrap();
        let decoded_pb = crate::raft::proto::pb::RaftMessage::decode(&buf[..]).unwrap();
        let decoded = decode_domain(decoded_pb);
        assert_eq!(decoded, original);

        // Server writes a reply.
        let reply = RaftMessage::VoteResponse(VoteResponseArgs { term: 42, vote_granted: true });
        let resp_payload = encode_domain(&reply).encode_to_vec();
        write_framed(&mut server_io, &resp_payload).await.unwrap();

        // Client reads and decodes the reply.
        let resp_buf = read_framed(&mut client_io).await.unwrap();
        let resp_pb = crate::raft::proto::pb::RaftMessage::decode(&resp_buf[..]).unwrap();
        let resp = decode_domain(resp_pb);
        assert_eq!(resp, reply);
    }
}