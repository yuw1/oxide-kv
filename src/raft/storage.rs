use crate::config::Config;
use crate::protocol::{LogEntry, Snapshot};

pub struct RaftStorage {
    wal_path: String,
    meta_path: String,
    snapshot_path: String,
}

impl RaftStorage {
    pub fn new() -> Self {
        Self {
            wal_path: Config::global().wal_path(),
            meta_path: Config::global().meta_path(),
            snapshot_path: Config::global().snapshot_path(),
        }
    }

    /// Construct a `RaftStorage` with explicit on-disk paths.
    /// Primarily used by tests to isolate each scenario in its own temp dir
    /// without depending on the global `Config`.
    pub fn new_with_paths(wal_path: String, meta_path: String, snapshot_path: String) -> Self {
        Self { wal_path, meta_path, snapshot_path }
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

    /// Persist a snapshot to disk atomically (write to .tmp, rename).
    pub fn save_snapshot(&self, snapshot: &Snapshot) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(snapshot).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e)
        })?;
        let path = self.snapshot_path.clone();
        let temp_path = format!("{}.tmp", path);
        std::fs::write(&temp_path, json)?;
        std::fs::rename(temp_path, path)?;
        Ok(())
    }

    /// Load the most recent snapshot from disk, or `None` if no snapshot exists.
    pub fn load_snapshot(&self) -> Option<Snapshot> {
        let content = std::fs::read_to_string(&self.snapshot_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Discard WAL entries whose index is `<= snapshot_index` by rewriting the
    /// WAL file. Returns the number of entries retained.
    ///
    /// The rewrite preserves the same frame format as `append_wal_log`
    /// (one bincode entry per write), so `restore_wal_log` keeps working
    /// unchanged. The rewrite is atomic (write to .tmp, rename).
    pub fn rewrite_wal_after_snapshot(&self, snapshot_index: u64) -> std::io::Result<usize> {
        use std::io::Write;
        let entries: Vec<LogEntry> = self
            .restore_wal_log()
            .into_iter()
            .filter(|e| e.index as u64 > snapshot_index)
            .collect();

        let temp_path = format!("{}.tmp", self.wal_path);
        let mut file = std::fs::File::create(&temp_path)?;
        for entry in &entries {
            let bytes = bincode::serialize(entry).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e)
            })?;
            file.write_all(&bytes)?;
        }
        file.sync_all()?;
        std::fs::rename(temp_path, &self.wal_path)?;
        Ok(entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Command, LogEntry, Snapshot};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn temp_storage() -> (TempDir, RaftStorage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wal = dir.path().join("test.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("test_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("test_snapshot.json").to_str().unwrap().to_string();
        let storage = RaftStorage::new_with_paths(wal, meta, snap);
        (dir, storage)
    }

    fn entry(term: u64, index: usize, key: &str, value: &str) -> LogEntry {
        LogEntry {
            term,
            index,
            command: Command::Set {
                key: key.to_string(),
                value: value.to_string(),
            },
        }
    }

    #[test]
    fn restore_wal_log_returns_empty_when_file_missing() {
        let (_dir, storage) = temp_storage();
        let logs = storage.restore_wal_log();
        assert!(logs.is_empty());
    }

    #[test]
    fn wal_roundtrip_preserves_entries_in_order() {
        let (_dir, storage) = temp_storage();
        let entries = vec![
            entry(1, 1, "a", "1"),
            entry(1, 2, "b", "2"),
            entry(2, 3, "c", "3"),
        ];

        for e in &entries {
            storage.append_wal_log(e).expect("append");
        }

        let restored = storage.restore_wal_log();
        assert_eq!(restored.len(), entries.len());
        for (got, want) in restored.iter().zip(entries.iter()) {
            assert_eq!(got.term, want.term);
            assert_eq!(got.index, want.index);
        }
    }

    #[test]
    fn wal_roundtrip_handles_delete_command() {
        let (_dir, storage) = temp_storage();
        let e = LogEntry {
            term: 5,
            index: 1,
            command: Command::Delete { key: "gone".to_string() },
        };
        storage.append_wal_log(&e).expect("append");

        let restored = storage.restore_wal_log();
        assert_eq!(restored.len(), 1);
        match &restored[0].command {
            Command::Delete { key } => assert_eq!(key, "gone"),
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn wal_roundtrip_is_durable_across_storage_instances() {
        // Simulate a restart: write with one storage, read with a fresh one pointing
        // at the same files.
        let (dir, storage) = temp_storage();
        let entries = vec![entry(1, 1, "k", "v"), entry(1, 2, "k2", "v2")];
        for e in &entries {
            storage.append_wal_log(e).unwrap();
        }

        let wal = dir.path().join("test.wal").to_str().unwrap().to_string();
        let meta = dir.path().join("test_meta.json").to_str().unwrap().to_string();
        let snap = dir.path().join("test_snapshot.json").to_str().unwrap().to_string();
        let storage2 = RaftStorage::new_with_paths(wal, meta, snap);

        let restored = storage2.restore_wal_log();
        assert_eq!(restored.len(), 2);
    }

    #[test]
    fn read_meta_returns_zero_when_file_missing() {
        let (_dir, storage) = temp_storage();
        assert_eq!(storage.read_meta(), (0, None));
    }

    #[test]
    fn save_meta_then_read_meta_roundtrips() {
        let (_dir, storage) = temp_storage();
        storage.save_meta(7, Some("node-2".to_string())).expect("save");
        let (term, vote) = storage.read_meta();
        assert_eq!(term, 7);
        assert_eq!(vote.as_deref(), Some("node-2"));
    }

    #[test]
    fn save_meta_supports_no_vote() {
        let (_dir, storage) = temp_storage();
        storage.save_meta(3, None).expect("save");
        let (term, vote) = storage.read_meta();
        assert_eq!(term, 3);
        assert!(vote.is_none());
    }

    #[test]
    fn save_meta_uses_atomic_rename_no_temp_leftover() {
        let (dir, storage) = temp_storage();
        storage.save_meta(1, Some("x".into())).unwrap();
        let temp_path = dir.path().join("test_meta.json.tmp");
        assert!(
            !temp_path.exists(),
            "atomic rename should leave no .tmp file behind"
        );
    }

    // ---------- snapshot tests ----------

    fn sample_snapshot(index: u64, term: u64) -> Snapshot {
        let mut data = HashMap::new();
        data.insert("alpha".into(), "1".into());
        data.insert("beta".into(), "2".into());
        Snapshot { last_included_index: index, last_included_term: term, data }
    }

    #[test]
    fn load_snapshot_returns_none_when_missing() {
        let (_dir, storage) = temp_storage();
        assert!(storage.load_snapshot().is_none());
    }

    #[test]
    fn save_snapshot_then_load_roundtrips() {
        let (_dir, storage) = temp_storage();
        let snap = sample_snapshot(42, 7);
        storage.save_snapshot(&snap).expect("save");

        let loaded = storage.load_snapshot().expect("load");
        assert_eq!(loaded.last_included_index, 42);
        assert_eq!(loaded.last_included_term, 7);
        assert_eq!(loaded.data.get("alpha").map(String::as_str), Some("1"));
        assert_eq!(loaded.data.get("beta").map(String::as_str), Some("2"));
    }

    #[test]
    fn save_snapshot_uses_atomic_rename() {
        let (dir, storage) = temp_storage();
        storage.save_snapshot(&sample_snapshot(1, 1)).unwrap();
        let temp_path = dir.path().join("test_snapshot.json.tmp");
        assert!(!temp_path.exists(), "atomic rename should leave no .tmp file behind");
    }

    #[test]
    fn save_snapshot_overwrites_previous_snapshot() {
        let (_dir, storage) = temp_storage();
        storage.save_snapshot(&sample_snapshot(1, 1)).unwrap();
        storage.save_snapshot(&sample_snapshot(100, 5)).unwrap();
        let loaded = storage.load_snapshot().unwrap();
        assert_eq!(loaded.last_included_index, 100);
        assert_eq!(loaded.last_included_term, 5);
    }

    #[test]
    fn rewrite_wal_keeps_entries_after_snapshot_index() {
        let (_dir, storage) = temp_storage();
        // Seed WAL with 5 entries spanning indices 1..=5.
        for i in 1..=5 {
            storage.append_wal_log(&entry(1, i, "k", "v")).unwrap();
        }

        // Snapshot at index 3 — entries 4 and 5 must survive.
        let kept = storage.rewrite_wal_after_snapshot(3).unwrap();
        assert_eq!(kept, 2);

        let remaining = storage.restore_wal_log();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].index, 4);
        assert_eq!(remaining[1].index, 5);
    }

    #[test]
    fn rewrite_wal_at_end_keeps_nothing() {
        let (_dir, storage) = temp_storage();
        for i in 1..=3 {
            storage.append_wal_log(&entry(1, i, "k", "v")).unwrap();
        }

        let kept = storage.rewrite_wal_after_snapshot(3).unwrap();
        assert_eq!(kept, 0);
        assert!(storage.restore_wal_log().is_empty());
    }

    #[test]
    fn rewrite_wal_before_any_entry_keeps_everything() {
        let (_dir, storage) = temp_storage();
        for i in 1..=3 {
            storage.append_wal_log(&entry(1, i, "k", "v")).unwrap();
        }

        // snapshot_index=0 means "no snapshot taken yet" — keep everything.
        let kept = storage.rewrite_wal_after_snapshot(0).unwrap();
        assert_eq!(kept, 3);
    }
}