use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::fs;
use std::sync::OnceLock;

static GLOBAL_CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    // Base node information
    pub node_id: String,
    pub listen_addr: String,     // Internal Raft communication address
    pub client_addr: String,     // External client request address
    pub peers: Vec<String>,

    // Root directory for storage
    #[serde(default)]
    pub data_dir: Option<String>,
}

impl Config {
    pub fn init(config: Config) {
        GLOBAL_CONFIG.set(config).expect("Config has already been initialized");
    }

    pub fn global() -> &'static Config {
        GLOBAL_CONFIG.get().expect("Config is not initialized! Call Config::init first.")
    }

    // --- Path Management ---

    pub fn get_base_path(&self) -> PathBuf {
        let path = match &self.data_dir {
            Some(d) => PathBuf::from(d),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("data"),
        };
        if !path.exists() {
            fs::create_dir_all(&path).expect("Failed to create data directory");
        }
        path
    }

    pub fn meta_path(&self) -> String {
        self.get_base_path().join(format!("node_{}_meta.json", self.node_id)).display().to_string()
    }

    pub fn wal_path(&self) -> String {
        self.get_base_path().join(format!("node_{}.wal", self.node_id)).display().to_string()
    }

    pub fn snapshot_path(&self) -> String {
        self.get_base_path().join(format!("node_{}_snapshot.json", self.node_id)).display().to_string()
    }

    // --- Time & Timeout Management ---

    /// Lower bound of the randomized election timeout.
    /// Constraint: Must be significantly larger than (heartbeat_interval + rpc_append_entries_timeout)
    pub fn min_election_timeout_ms() -> u64 { 3000 }

    /// Upper bound of the randomized election timeout.
    pub fn max_election_timeout_ms() -> u64 { 5000 }

    /// How often the Leader sends heartbeats to maintain its authority.
    pub fn heartbeat_interval_ms() -> u64 { 1000 }

    /// Max time to wait for a response of RequestVote RPC.
    pub fn rpc_request_vote_timeout_ms() -> u64 { 1000 }

    /// Max time to wait for a response of AppendEntries RPC.
    pub fn rpc_append_entries_timeout_ms() -> u64 { 1500 }
}