use std::io::Write;
use std::string::ToString;
use std::sync::{Arc, RwLock};
use clap::Parser;
use serde_json::Deserializer;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use oxide_kv::client::ClientHandler;
use oxide_kv::config::Config;
use oxide_kv::state_machine::StateMachine;
use oxide_kv::protocol::Command;
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::rpc::RpcServer;
use oxide_kv::raft::timer::run_election_timer;

#[derive(Parser, Debug)]
pub struct Args {
    #[arg(short, long)]
    pub addr: String,
    #[arg(short, long)]
    pub client_addr: String,
    #[arg(short, long, value_delimiter = ',')]
    pub peers: Vec<String>,
    pub data_dir: Option<String>
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Parse command line arguments
    let args = <Args as clap::Parser>::parse();
    let config = Config {
        node_id: args.addr.clone(),
        listen_addr: args.addr.clone(),
        client_addr: args.client_addr,
        peers: args.peers.clone(),
        data_dir: args.data_dir,
    };
    Config::init(config);

    // 2. Initialize Key-Value State Machine
    let state_machine = Arc::new(RwLock::new(StateMachine::open().expect("Failed to open StateMachine")));

    // 3. Create Raft instance and inject restored state
    let mut raft_node_inner = RaftNode::new(
        Config::global().listen_addr.clone(),
        Config::global().peers.clone(),
        state_machine.clone(),
    );

    // 4. Handle single-node startup: If no peers, become Leader automatically
    if args.peers.is_empty() {
        println!("🚀 No peers detected. Entering standalone Leader mode.");
        raft_node_inner.state = NodeState::Leader;
    }

    let raft_node = Arc::new(RwLock::new(raft_node_inner));

    // 5. Replay logs: Apply WAL commands to the in-memory state machine
    {
        let mut node = raft_node.write().unwrap();
        node.replay_logs();
    }

    // 6. Start Raft RPC listener (Handles internal voting and heartbeats)
    let r_node = raft_node.clone();
    let raft_listener = TcpListener::bind(&Config::global().listen_addr).await?;
    println!("📡 Raft RPC Service started at: {}", Config::global().listen_addr);

    tokio::spawn(async move {
        while let Ok((stream, _)) = raft_listener.accept().await {
            let n = r_node.clone();
            tokio::spawn(async move {
                RpcServer::handle_raft_rpc(stream, n).await;
            });
        }
    });

    // 7. Start Election Timer
    // Triggers election if no heartbeat is received within the randomized timeout
    let timer_node = raft_node.clone();
    tokio::spawn(async move {
        run_election_timer(timer_node).await;
    });

    // 8. Start Leader Heartbeat Loop
    // Periodically sends heartbeats only if state == Leader
    let heartbeat_node = raft_node.clone();
    tokio::spawn(async move {
        RaftNode::run_heartbeat_loop(heartbeat_node).await;
    });

    // 9. Graceful shutdown handling
    let shutdown_node = raft_node.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        println!("\n🛑 Shutdown signal received. Saving state...");
        // TODO: Add explicit persistence/cleanup logic here
        std::process::exit(0);
    });

    // 10. Start External Client API listener (Handles SET/GET commands)
    let c_node = raft_node.clone();
    let client_listener = TcpListener::bind(&Config::global().client_addr).await?;
    println!("📥 Client API Service started at: {}", &Config::global().client_addr);

    println!("✅ System ready. Waiting for client connections...");

    // 11. Main client processing loop
    while let Ok((stream, _)) = client_listener.accept().await {
        let n = c_node.clone();
        tokio::spawn(async move {
            ClientHandler::handle_client_request(stream, n).await;
        });
    }

    Ok(())
}