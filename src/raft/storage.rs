use crate::config::Config;
use crate::protocol::LogEntry;

pub struct RaftStorage {
    wal_path: String,
    meta_path: String,
}

impl RaftStorage {
    pub fn new() -> Self {
        Self {
            wal_path: Config::global().wal_path(),
            meta_path: Config::global().meta_path(),
        }
    }

    pub fn load_initial_state(&self) -> (u64, Option<String>, Vec<LogEntry>) {
        let (term, vote) = self.read_meta();
        println!("📖 Meta Restored: Term={}, VotedFor={:?}", term, vote);

        let logs = self.restore_wal_log();
        (term, vote, logs)
    }

    pub fn append_wal_log(&self, entry: &LogEntry) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.wal_path.clone())?;

        let bytes = bincode::serialize(entry).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e)
        })?;

        file.write_all(&bytes)?;
        file.sync_all()?; // Ensure durability
        Ok(())
    }

    pub fn restore_wal_log(&self) -> Vec<LogEntry> {
        let mut log = Vec::new();

        if let Ok(file) = std::fs::File::open(&self.wal_path) {
            let mut reader = std::io::BufReader::new(file);

            // Attempt to continuously deserialize from the stream until the end of the file
            while let Ok(entry) = bincode::deserialize_from(&mut reader) {
                log.push(entry);
            }
        }
        log
    }

    pub fn save_meta(&self, term: u64, vote: Option<String>) -> anyhow::Result<()> {
        let meta = serde_json::json!({
            "current_term": term,
            "vote_for": vote,
        });
        let path = self.meta_path.clone();
        let temp_path = format!("{}.tmp", path);
        std::fs::write(&temp_path, meta.to_string())?;
        std::fs::rename(temp_path, path)?;
        Ok(())
    }

    pub fn read_meta(&self) -> (u64, Option<String>) {
        if let Ok(content) = std::fs::read_to_string(self.meta_path.clone()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let term = v["current_term"].as_u64().unwrap_or(0);
                let vote = v["vote_for"].as_str().map(|s| s.to_string());
                return (term, vote);
            }
        }
        (0, None)
    }
}