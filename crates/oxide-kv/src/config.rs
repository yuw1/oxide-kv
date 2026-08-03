use serde::Deserialize;
use std::path::PathBuf;
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
    /// Constraint: Must be significantly larger than (heartbeat_interval + rpc_append_entries_timeout).
    /// Tuned so the heartbeat-to-election ratio is ≥ 1:10 to tolerate transient RPC
    /// jitter without spurious elections (industry standard is 1:10–1:20).
    pub fn min_election_timeout_ms() -> u64 { 5000 }

    /// Upper bound of the randomized election timeout.
    /// Range (max - min) must be < 50% of min so two nodes rarely draw overlapping
    /// windows and split their votes (industry standard).
    pub fn max_election_timeout_ms() -> u64 { 10000 }

    /// How often the Leader sends heartbeats to maintain its authority.
    /// Must be small enough that one tick fits well inside the election timeout
    /// (see min/max_election_timeout_ms doc).
    pub fn heartbeat_interval_ms() -> u64 { 250 }

    /// Max time to wait for a response of RequestVote RPC.
    pub fn rpc_request_vote_timeout_ms() -> u64 { 1000 }

    /// Max time to wait for a response of AppendEntries RPC.
    pub fn rpc_append_entries_timeout_ms() -> u64 { 1500 }

    /// When the on-disk WAL grows past this many bytes, the leader takes
    /// a snapshot of its current state machine and truncates the WAL.
    /// Defaults to 64 MiB which keeps the WAL small enough to replay in
    /// well under a second on a typical machine while still letting a
    /// busy node accumulate a useful amount of history between snapshots.
    ///
    /// Override via the `OXIDE_SNAPSHOT_THRESHOLD_BYTES` env var (mainly
    /// for tests that need a tiny threshold so a few entries trigger a
    /// snapshot). Followers never snapshot — only the leader does.
    pub fn snapshot_threshold_bytes() -> u64 {
        std::env::var("OXIDE_SNAPSHOT_THRESHOLD_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(64 * 1024 * 1024)
    }
}