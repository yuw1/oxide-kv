use crate::config::Config;
use crate::coordination::{VoteRequest, VoteResponse};
use crate::protocol::{LogEntry, Snapshot};
pub use crate::raft::node::{NodeState, RaftNode};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::raft::transport::{
    read_envelope, read_envelope_discriminator, read_envelope_payload, write_envelope, DispatchKind,
};

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
    /// Wire format (P6 PR #12 onward): 1-byte protocol discriminator
    /// followed by a 4-byte big-endian length prefix and a protobuf
    /// payload. The discriminator selects between Raft consensus RPCs
    /// (`0x01`) and the 2PC coordinator vote RPC (`0x02`); both share
    /// the same TCP listener bound to `Config::listen_addr`.
    ///
    /// `pub(crate)` so `crate::raft::net::TcpTransport` (the P7
    /// Transport-trait real impl) can route every Raft variant through
    /// one helper instead of needing per-variant dispatcher code.
    pub(crate) async fn call(addr: &str, msg: RaftMessage, timeout_duration: Duration) -> anyhow::Result<RaftMessage> {
        let stream = timeout(timeout_duration, TcpStream::connect(addr)).await??;
        let (mut reader, mut writer) = stream.into_split();

        let payload = crate::raft::proto::encode_domain(&msg).encode_to_vec();
        write_envelope(&mut writer, DispatchKind::Raft, &payload).await?;
        let (_, resp_buf) = match read_envelope(&mut reader).await? {
            Some(env) => env,
            None => return Err(anyhow::anyhow!("RPC peer closed connection before reply")),
        };
        let resp_pb = crate::raft::proto::pb::RaftMessage::decode(&resp_buf[..])?;
        Ok(crate::raft::proto::decode_domain(resp_pb))
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

    /// Send a 2PC coordinator `VoteRequest` to a peer over the
    /// multiplexed transport (`DispatchKind::Vote`).
    ///
    /// This is the P6 PR #12 client-side half of the side-channel RPC
    /// that lets the leader collect per-peer votes between BeginTx
    /// and DecideTx without inflating the Raft log (see
    /// `ROADMAP.md` P6 and `proto/coordination.proto`).
    #[allow(dead_code)] // Wired up by PR #13 coordinator orchestration.
    pub(crate) async fn send_tx_vote_rpc(
        addr: &str,
        req: VoteRequest,
        timeout_duration: Duration,
    ) -> anyhow::Result<VoteResponse> {
        let stream = timeout(timeout_duration, TcpStream::connect(addr)).await??;
        let (mut reader, mut writer) = stream.into_split();

        let pb = crate::coordination::pb::VoteRequest::from(&req).encode_to_vec();
        write_envelope(&mut writer, DispatchKind::Vote, &pb).await?;
        let (kind, resp_buf) = match read_envelope(&mut reader).await? {
            Some(env) => env,
            None => return Err(anyhow::anyhow!("Vote RPC peer closed connection before reply")),
        };
        if kind != DispatchKind::Vote {
            return Err(anyhow::anyhow!(
                "Vote RPC peer replied with wrong discriminator {:?}",
                kind
            ));
        }
        let resp_pb = crate::coordination::pb::VoteResponse::decode(&resp_buf[..])?;
        Ok(VoteResponse::from(resp_pb))
    }
}

pub struct RpcServer;

impl RpcServer {
    /// Dispatch a single inbound TCP connection from the multiplexed
    /// inter-node listener.
    ///
    /// P6 PR #12: the shared listener now demuxes on the first byte
    /// (the protocol discriminator from `crate::raft::transport`).
    /// Raft consensus RPCs continue to flow through `handle_raft_rpc`
    /// unchanged in semantics; the new `handle_vote_rpc` handles the
    /// 2PC coordinator vote surface added in P6.
    pub async fn dispatch(mut stream: TcpStream, raft_node: Arc<RwLock<RaftNode>>) {
        if let Err(e) = Self::dispatch_logic(&mut stream, raft_node).await {
            eprintln!(
                "[Thread {:?}] RPC dispatch error: {}",
                std::thread::current().id(),
                e
            );
        }
    }

    /// Internal entry point. Reads the discriminator byte and routes
    /// the rest of the connection to the matching handler. Each
    /// handler owns its own read/write loop, so a slow or hung handler
    /// does not block traffic for the other RPC surface on a separate
    /// connection — multiplexing here is per-connection, not per-byte.
    async fn dispatch_logic(
        stream: &mut TcpStream,
        raft_node: Arc<RwLock<RaftNode>>,
    ) -> anyhow::Result<(), anyhow::Error> {
        // Read just the discriminator byte; each handler below calls
        // `read_envelope_payload` for the length-prefixed body so the
        // frame is consumed exactly once.
        let (mut reader, writer) = stream.split();
        let kind = match read_envelope_discriminator(&mut reader).await? {
            Some(k) => k,
            None => return Ok(()), // clean EOF before any byte
        };

        match kind {
            DispatchKind::Raft => {
                Self::handle_raft_rpc_inner(reader, writer, raft_node).await
            }
            DispatchKind::Vote => {
                Self::handle_vote_rpc_inner(reader, writer, raft_node).await
            }
        }
    }

    /// Generic dispatch helper used by tests. Identical semantics to
    /// the TCP-bound `dispatch_logic` but parameterised over any
    /// `AsyncRead + AsyncWrite` so an in-memory duplex can exercise
    /// the routing logic without binding a real socket.
    #[cfg(test)]
    pub(crate) async fn dispatch_on<S>(
        stream: S,
        raft_node: Arc<RwLock<RaftNode>>,
    ) -> anyhow::Result<(), anyhow::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, writer) = tokio::io::split(stream);
        let kind = match read_envelope_discriminator(&mut reader).await? {
            Some(k) => k,
            None => return Ok(()),
        };
        match kind {
            DispatchKind::Raft => {
                Self::handle_raft_rpc_inner(reader, writer, raft_node).await
            }
            DispatchKind::Vote => {
                Self::handle_vote_rpc_inner(reader, writer, raft_node).await
            }
        }
    }

    /// Backwards-compatible alias used by older call sites (e.g. the
    /// `RpcServer::handle_raft_rpc` name in tests) so the public API
    /// for the Raft RPC path stays the same as before the multiplexer
    /// was introduced. Calls into `dispatch` so the dispatch logic
    /// stays in one place.
    pub async fn handle_raft_rpc(stream: TcpStream, raft_node: Arc<RwLock<RaftNode>>) {
        Self::dispatch(stream, raft_node).await;
    }

    async fn handle_raft_rpc_inner<R, W>(
        mut reader: R,
        mut writer: W,
        raft_node: Arc<RwLock<RaftNode>>,
    ) -> anyhow::Result<(), anyhow::Error>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        // 1. Read the length-prefixed payload (discriminator was
        //    already consumed by `dispatch_logic`).
        let req_buf = match read_envelope_payload(&mut reader).await? {
            Some(buf) => buf,
            None => return Ok(()),
        };

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
        write_envelope(&mut writer, DispatchKind::Raft, &resp_payload).await?;
        Ok(())
    }

    /// Server-side handler for the 2PC coordinator vote RPC.
    ///
    /// Decodes the inbound `VoteRequest`, delegates to
    /// `RaftNode::handle_tx_vote_request` for the safety decision,
    /// and writes back a `VoteResponse` envelope on the same
    /// connection. The handler is intentionally short — all policy
    /// lives in `RaftNode::handle_tx_vote_request` so unit tests can
    /// exercise the decision in isolation without TCP plumbing.
    async fn handle_vote_rpc_inner<R, W>(
        mut reader: R,
        mut writer: W,
        raft_node: Arc<RwLock<RaftNode>>,
    ) -> anyhow::Result<(), anyhow::Error>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        // Discriminator already consumed by `dispatch_logic`; this
        // call only reads the length-prefixed payload portion.
        let req_buf = match read_envelope_payload(&mut reader).await? {
            Some(buf) => buf,
            None => return Ok(()),
        };

        let req_pb = crate::coordination::pb::VoteRequest::decode(&req_buf[..])?;
        let req: VoteRequest = req_pb.into();

        let reply = {
            let mut node = raft_node.write().unwrap();
            node.handle_tx_vote_request(&req)
        };

        let resp_pb: crate::coordination::pb::VoteResponse = (&reply).into();
        let resp_bytes = resp_pb.encode_to_vec();
        write_envelope(&mut writer, DispatchKind::Vote, &resp_bytes).await?;
        Ok(())
    }
}

/// Write a single length-prefixed protobuf frame.
///
/// Deprecated as of P6 PR #12: the wire format is now multiplexed, so
/// callers must use `crate::raft::transport::write_envelope` with an
/// explicit `DispatchKind`. This shim is kept for `tests` below that
/// exercise the in-memory duplex path without going through the
/// dispatch envelope.
#[cfg(test)]
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
#[cfg(test)]
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

    /// End-to-end test: a `VoteRequest` envelope written by the
    /// client is consumed by `RpcServer::dispatch_on`, which routes
    /// it to `handle_vote_rpc_inner`, which calls
    /// `RaftNode::handle_tx_vote_request` and writes a `VoteResponse`
    /// back. The test confirms the full wire path works through the
    /// multiplexed envelope without binding a real socket.
    #[tokio::test]
    async fn vote_rpc_dispatch_roundtrip_on_duplex() {
        use crate::coordination::{pb, VoteRequest, VoteResponse};
        use crate::raft::transport::{read_envelope, write_envelope, DispatchKind};
        use crate::raft::node::RaftNode;
        use crate::raft::storage::RaftStorage;
        use crate::protocol::TxOp;
        use crate::state_machine::{StateMachine, StateMachineConfig};
        use prost::Message;
        use std::sync::{Arc, RwLock};

        // Build a node with a pending tx so the vote can be granted.
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("v.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("v_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("v_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join("v_sm");
        let sm = Arc::new(RwLock::new(StateMachine::open(StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        }).unwrap()));
        let mut node = RaftNode::new_with_storage("v".to_string(), vec![], sm, storage);
        node.current_term = 1;
        {
            let mut sm = node.state_machine.write().unwrap();
            sm.begin_tx(
                "tx-vote".to_string(),
                vec![TxOp::Put { key: "k".into(), value: "v".into() }],
            )
            .unwrap();
        }
        let node_arc = Arc::new(RwLock::new(node));

        let (a, b) = duplex(64 * 1024);

        // Server task: drives the full dispatch path.
        let server_node = node_arc.clone();
        let server = tokio::spawn(async move {
            RpcServer::dispatch_on(b, server_node).await
        });

        // Client side: write a VoteRequest envelope and read the
        // response envelope. We use the wire types directly (not
        // the domain `RpcClient::send_tx_vote_rpc`) so the test
        // exercises the dispatcher's envelope handling in isolation.
        let req = VoteRequest {
            term: 1,
            tx_id: "tx-vote".to_string(),
            last_log_index: 0,
            last_log_term: 0,
        };
        let req_pb = pb::VoteRequest::from(&req).encode_to_vec();
        let (mut a_reader, mut a_writer) = tokio::io::split(a);
        write_envelope(&mut a_writer, DispatchKind::Vote, &req_pb)
            .await
            .unwrap();
        let (kind, resp_buf) = read_envelope(&mut a_reader).await.unwrap().unwrap();
        assert_eq!(kind, DispatchKind::Vote);
        let resp_pb = pb::VoteResponse::decode(&resp_buf[..]).unwrap();
        let resp: VoteResponse = resp_pb.into();

        // The vote should be granted because the term matches and
        // the tx is pending locally.
        assert!(resp.vote_granted, "expected Yes, got No: {}", resp.reason);
        assert_eq!(resp.term, 1);
        assert!(resp.reason.is_empty());

        // Server should complete cleanly.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    }

    /// Confirm that an unknown discriminator on the inbound byte
    /// causes `dispatch_on` to return an error rather than silently
    /// misrouting the frame. This is the safety check that
    /// discriminates the multiplexed envelope.
    #[tokio::test]
    async fn dispatch_on_rejects_unknown_discriminator() {
        use crate::raft::node::RaftNode;
        use crate::raft::storage::RaftStorage;
        use crate::state_machine::{StateMachine, StateMachineConfig};
        use std::sync::{Arc, RwLock};

        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("u.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("u_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("u_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        let sm_dir = dir.path().join("u_sm");
        let sm = Arc::new(RwLock::new(StateMachine::open(StateMachineConfig {
            data_dir: sm_dir,
            memtable_size_threshold: 1024 * 1024,
        }).unwrap()));
        let node = RaftNode::new_with_storage("u".to_string(), vec![], sm, storage);
        let node_arc = Arc::new(RwLock::new(node));

        let (mut a, b) = duplex(64);

        // Write an unknown discriminator (0x7F) and a 4-byte length
        // so the dispatcher's envelope-reader can produce the right
        // error message.
        a.write_all(&[0x7Fu8]).await.unwrap();
        a.write_all(&0u32.to_be_bytes()).await.unwrap();
        drop(a);

        let err = RpcServer::dispatch_on(b, node_arc).await.unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("0x7f"), "got: {}", msg);
    }
}