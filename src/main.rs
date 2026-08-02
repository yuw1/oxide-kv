use std::sync::{Arc, RwLock};
use clap::Parser;
use tokio::net::TcpListener;
use oxide_kv::client::ClientHandler;
use oxide_kv::config::Config;
use oxide_kv::state_machine::{StateMachine, StateMachineConfig};
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::net::{StopSignal, TcpTransport, Transport};
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
    let sm_config = StateMachineConfig {
        data_dir: Config::global().get_base_path(),
        // 4 MiB memtable threshold: tunable later via Config if needed.
        memtable_size_threshold: 4 * 1024 * 1024,
    };
    let state_machine = Arc::new(RwLock::new(StateMachine::open(sm_config).expect("Failed to open StateMachine")));

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

    // 6. Start Raft RPC listener (Handles internal voting and heartbeats).
    //    Route through the abstracted `Transport` trait so the future
    //    P7 simulation harness can replace this with an in-memory
    //    listener. The `StopSignal` is wired to `ctrl_c` below so a
    //    graceful shutdown can drain the accept loop.
    let r_node = raft_node.clone();
    let raft_listener = TcpListener::bind(&Config::global().listen_addr).await?;
    println!("📡 Raft RPC Service started at: {}", Config::global().listen_addr);
    let raft_transport: Arc<dyn Transport> = Arc::new(TcpTransport::with_listener(raft_listener));
    let raft_stop = StopSignal::new();
    {
        let r_node = r_node.clone();
        let raft_stop = raft_stop.clone();
        tokio::spawn(async move {
            if let Err(e) = raft_transport.serve(r_node, raft_stop).await {
                eprintln!("[raft] transport serve stopped: {}", e);
            }
        });
    }

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
    // TODO: Persist final state and cleanly drain in-flight RPCs before exit.
    let _shutdown_node = raft_node.clone();
    let raft_stop_for_ctrl_c = raft_stop.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        println!("\n🛑 Shutdown signal received. Saving state...");
        // Tell the Raft listener to stop accepting new connections;
        // in-flight RPCs drain naturally. Persistence/cleanup will
        // land in a later PR (see CHANGELOG).
        raft_stop_for_ctrl_c.stop();
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