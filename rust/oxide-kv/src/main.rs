use clap::Parser;
use oxide_kv::client::ClientHandler;
use oxide_kv::config::Config;
use oxide_kv::observability::{Metrics, run_metrics_server};
use oxide_kv::raft::coordinator;
use oxide_kv::raft::net::{StopSignal, TcpTransport, Transport};
use oxide_kv::raft::node::{NodeState, RaftNode};
use oxide_kv::raft::timer::run_election_timer;
use oxide_kv::state_machine::{StateMachine, StateMachineConfig};
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Initialise the global `tracing` subscriber.
///
/// Behaviour:
/// - **No `RUST_LOG` set** → default level `info` for the
///   `oxide_kv` crate (heartbeat / AE receive stay at `debug`,
///   so the default log is the clean state-change summary).
/// - **`RUST_LOG=oxide_kv::raft=debug`** → full protocol trace
///   (every heartbeat, every AE, every commit advance).
/// - **`RUST_LOG=oxide_kv=trace`** → everything including
///   `tracing::trace!` calls if any module adds them later.
///
/// Idempotent: if a subscriber is already installed (e.g. a
/// test main installs its own), `try_init` is a no-op so we
/// don't pull the floor out.
fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("oxide_kv=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

#[derive(Parser, Debug)]
pub struct Args {
    #[arg(short, long)]
    pub addr: String,
    #[arg(short, long)]
    pub client_addr: String,
    #[arg(short, long, value_delimiter = ',')]
    pub peers: Vec<String>,
    #[arg(long)]
    pub data_dir: Option<String>,
    /// Address for the Prometheus `/metrics` HTTP endpoint.
    /// Default `127.0.0.1:9100` (private loopback; bind to a
    /// routable address if scraping from another host). Set to
    /// `disabled` to skip starting the server.
    #[arg(long, default_value = "127.0.0.1:9100")]
    pub metrics_addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

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
    let state_machine = Arc::new(RwLock::new(
        StateMachine::open(sm_config).expect("Failed to open StateMachine"),
    ));

    // 3. Create Raft instance and inject restored state
    let mut raft_node_inner = RaftNode::new(
        Config::global().listen_addr.clone(),
        Config::global().peers.clone(),
        state_machine.clone(),
    );

    // 4. Handle single-node startup: If no peers, become Leader automatically
    if args.peers.is_empty() {
        info!("no peers detected; entering standalone Leader mode");
        raft_node_inner.state = NodeState::Leader;
    }

    let raft_node = Arc::new(RwLock::new(raft_node_inner));

    // 5. Restore from snapshot (if any), then replay remaining WAL entries.
    //    A snapshot covers everything up to its last_included_index; the
    //    WAL still contains any entries appended after the snapshot, so
    //    replay applies just the delta. A node with no prior snapshot
    //    file simply replays from index 1.
    {
        let mut node = raft_node.write().unwrap();
        if let Some((idx, term)) = node.restore_from_snapshot() {
            info!(idx, term, "restored snapshot; replaying WAL delta");
        }
        node.replay_logs();
    }

    // 5b. Wire Prometheus metrics (P8 PR 8). The handle is shared
    //     with the metrics HTTP server spawned in step 8c below.
    //     Pre-register the configured peer list so per-peer
    //     gauges (`peer_match_index` / `peer_next_index`) appear
    //     in the first scrape rather than only after the first
    //     AppendEntries reply.
    let metrics = Metrics::with_peers(&args.peers).expect("Failed to construct metrics registry");
    {
        let mut node = raft_node.write().unwrap();
        node.set_metrics(metrics.clone());
        // Seed the initial gauges so the first scrape returns
        // real values, not pre-seed defaults. This is a single
        //     `refresh_metrics()` call (covers term /
        //     commit_index / last_applied / log_length / role).
        node.refresh_metrics();
    }
    if args.metrics_addr != "disabled" {
        info!(endpoint = %args.metrics_addr, "metrics endpoint will be served");
    }

    // 6. Start Raft RPC listener (Handles internal voting and heartbeats).
    //    Route through the abstracted `Transport` trait so the future
    //    P7 simulation harness can replace this with an in-memory
    //    listener. The `StopSignal` is wired to `ctrl_c` below so a
    //    graceful shutdown can drain the accept loop.
    let r_node = raft_node.clone();
    let raft_listener = TcpListener::bind(&Config::global().listen_addr).await?;
    info!(addr = %Config::global().listen_addr, "Raft RPC service started");
    let raft_transport: Arc<dyn Transport> = Arc::new(TcpTransport::with_listener(raft_listener));
    let raft_stop = StopSignal::new();
    {
        let r_node = r_node.clone();
        let raft_stop = raft_stop.clone();
        tokio::spawn(async move {
            if let Err(e) = raft_transport.serve(r_node, raft_stop).await {
                error!(error = %e, "raft transport serve stopped");
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
    let heartbeat_stop = raft_stop.clone();
    tokio::spawn(async move {
        RaftNode::run_heartbeat_loop(heartbeat_node, heartbeat_stop).await;
    });

    // 8b. Start Tx Timeout Sweep Loop (P8 PR 7)
    // Periodically scans `pending_txs` on the leader and
    // force-aborts transactions older than `OXIDE_TX_TIMEOUT_MS`.
    // Closes the coordinator-crash hole: a leader that dies mid-2PC
    // leaves a BeginTx entry in every follower's `pending_txs`
    // table forever; this loop is the new leader's recovery path.
    let sweep_node = raft_node.clone();
    let sweep_stop = raft_stop.clone();
    tokio::spawn(async move {
        coordinator::run_tx_timeout_loop(sweep_node, sweep_stop).await;
    });

    // 8c. Start Prometheus `/metrics` HTTP server (P8 PR 8).
    //     Hand-rolled minimal HTTP/1.1 reader; serves the typed
    //     `Metrics` registry. Disabled by passing
    //     `--metrics-addr disabled` (used by tests that don't
    //     want to bind a port).
    if args.metrics_addr != "disabled" {
        let metrics_for_server = metrics.clone();
        let metrics_addr = args.metrics_addr.clone();
        let metrics_stop = raft_stop.clone();
        tokio::spawn(async move {
            if let Err(e) = run_metrics_server(metrics_for_server, metrics_addr, metrics_stop).await
            {
                error!(error = %e, "metrics server stopped");
            }
        });
    }

    // 9. Graceful shutdown handling
    // TODO: Persist final state and cleanly drain in-flight RPCs before exit.
    let _shutdown_node = raft_node.clone();
    let raft_stop_for_ctrl_c = raft_stop.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("shutdown signal received; draining");
        // Tell the Raft listener to stop accepting new connections;
        // in-flight RPCs drain naturally. Persistence/cleanup will
        // land in a later PR (see CHANGELOG).
        raft_stop_for_ctrl_c.stop();
        std::process::exit(0);
    });

    // 10. Start External Client API listener (Handles SET/GET commands)
    let c_node = raft_node.clone();
    let client_listener = TcpListener::bind(&Config::global().client_addr).await?;
    info!(addr = %Config::global().client_addr, "client API service started");

    info!("system ready; waiting for client connections");

    // 11. Main client processing loop
    while let Ok((stream, _)) = client_listener.accept().await {
        let n = c_node.clone();
        tokio::spawn(async move {
            ClientHandler::handle_client_request(stream, n).await;
        });
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: prior to PR #36, `Args::data_dir` had no `#[arg(long)]`
    /// annotation, so clap parsed it as a positional argument and rejected
    /// `--data-dir /tmp/x`. This test pins the documented `--data-dir`
    /// CLI form that the deployment guide (`README.md`) and
    /// `deploy/systemd/oxide-kv@.service` rely on.
    #[test]
    fn args_parses_data_dir_flag() {
        let args = <Args as clap::Parser>::try_parse_from([
            "oxide-kv",
            "--addr",
            "127.0.0.1:9001",
            "--client-addr",
            "127.0.0.1:9101",
            "--peers",
            "127.0.0.1:9002,127.0.0.1:9003",
            "--data-dir",
            "/var/lib/oxide-kv/node-1",
        ])
        .expect("--data-dir should be a valid clap flag");
        assert_eq!(args.addr, "127.0.0.1:9001");
        assert_eq!(args.client_addr, "127.0.0.1:9101");
        assert_eq!(args.peers, vec!["127.0.0.1:9002", "127.0.0.1:9003"]);
        assert_eq!(args.data_dir.as_deref(), Some("/var/lib/oxide-kv/node-1"));
    }

    /// Without `--data-dir`, the flag stays `None` and the binary falls
    /// back to its default data dir (whatever the storage layer picks
    /// when the field is `None`).
    #[test]
    fn args_data_dir_is_optional() {
        let args = <Args as clap::Parser>::try_parse_from([
            "oxide-kv",
            "--addr",
            "127.0.0.1:9001",
            "--client-addr",
            "127.0.0.1:9101",
        ])
        .expect("data_dir should be optional");
        assert!(args.data_dir.is_none());
    }

    /// P8 PR 8: `--metrics-addr` defaults to `127.0.0.1:9100` and
    /// accepts the sentinel `disabled` to skip the metrics
    /// server. Pinning the default + sentinel in tests prevents
    /// accidental breaking changes to the documented ops surface.
    #[test]
    fn args_metrics_addr_defaults_to_loopback_9100() {
        let args = <Args as clap::Parser>::try_parse_from([
            "oxide-kv",
            "--addr",
            "127.0.0.1:9001",
            "--client-addr",
            "127.0.0.1:9101",
        ])
        .expect("metrics_addr should have a default");
        assert_eq!(args.metrics_addr, "127.0.0.1:9100");
    }

    #[test]
    fn args_metrics_addr_disabled_sentinel() {
        let args = <Args as clap::Parser>::try_parse_from([
            "oxide-kv",
            "--addr",
            "127.0.0.1:9001",
            "--client-addr",
            "127.0.0.1:9101",
            "--metrics-addr",
            "disabled",
        ])
        .expect("disabled sentinel should parse");
        assert_eq!(args.metrics_addr, "disabled");
    }
}
