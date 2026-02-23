use crate::config::Config;
use crate::protocol::LogEntry;
pub use crate::raft::node::{NodeState, RaftNode};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[derive(Serialize, Deserialize, Debug)]
pub enum RaftMessage {
    RequestVote(RequestVoteArgs),
    VoteResponse(VoteResponseArgs),
    AppendEntries(AppendEntriesArgs),
    AppendReply(AppendReplyArgs),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestVoteArgs {
    pub term: u64,
    pub candidate_id: String,
    pub last_log_index: u64,
    pub last_log_term: u64
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct VoteResponseArgs {
    pub term: u64,
    pub vote_granted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AppendEntriesArgs {
    pub term: u64,              // Leader's current term
    pub leader_id: String,      // Leader's ID so follower can redirect clients
    pub prev_log_index: u64,    // Index of log entry immediately preceding new ones
    pub prev_log_term: u64,     // Term of prev_log_index entry
    pub entries: Vec<LogEntry>, // Log entries to store (empty for heartbeat)
    pub leader_commit: u64,     // Leader's commit_index
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AppendReplyArgs {
    pub term: u64,    // Current term of follower, for leader to update itself
    pub success: bool, // True if follower contained entry matching prev_log_index and prev_log_term
}

pub struct RpcClient;

impl RpcClient {
    /// Internal helper to handle the common TCP transport logic
    async fn call(addr: &str, msg: RaftMessage, timeout_duration: Duration) -> anyhow::Result<RaftMessage> {
        // 1. Establish connection with timeout
        let stream = timeout(timeout_duration, TcpStream::connect(addr)).await??;
        let (reader, mut writer) = tokio::io::split(stream);

        // 2. Serialize and send (with newline as delimiter)
        let mut msg_json = serde_json::to_string(&msg)?;
        msg_json.push('\n');
        writer.write_all(msg_json.as_bytes()).await?;
        writer.flush().await?;

        // 3. Read response line
        let mut line = String::new();
        let mut buf_reader = BufReader::new(reader);
        buf_reader.read_line(&mut line).await?;

        // 4. Deserialize and return
        let resp = serde_json::from_str::<RaftMessage>(&line)?;
        Ok(resp)
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
        let (reader, mut writer) = stream.split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        // 1. Read the JSON line from the stream
        buf_reader.read_line(&mut line).await?;
        if line.is_empty() { return Ok(()); }

        // 2. Parse the common RaftMessage enum
        let msg: RaftMessage = serde_json::from_str(&line)?;

        // 3. Dispatch to the specific handler in RaftNode
        match msg {
            RaftMessage::RequestVote(args) => {
                let reply = {
                    let mut node = raft_node.write().unwrap();
                    node.handle_request_vote(&args)
                };
                let resp = serde_json::to_string(&RaftMessage::VoteResponse(reply))? + "\n";
                writer.write_all(resp.as_bytes()).await?;
                println!("✅ Responded to vote request from node {}", args.candidate_id);
            }
            RaftMessage::AppendEntries(args) => {
                let reply = {
                    let mut node = raft_node.write().unwrap();
                    node.handle_append_entries(&args)
                };
                let resp = serde_json::to_string(&RaftMessage::AppendReply(reply))? + "\n";
                writer.write_all(resp.as_bytes()).await?;
                println!("✅ Responded to heartbeat from Leader {} (Term {})", args.leader_id, args.term);
            }
            _ => {
                println!("⚠️ Received unexpected RPC message type");
            }
        }
        Ok(())
    }
}