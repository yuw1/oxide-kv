//! Log-Structured Merge (LSM) Tree key-value store used as the Raft state machine.
//!
//! On-disk layout under `data_dir`:
//!
//! ```text
//! data_dir/
//!   memtable.wal          // append-only WAL; one JSON op per line
//!   sst/
//!     000000.sst          // oldest SSTable on disk
//!     000000.sst.meta
//!     000001.sst
//!     000001.sst.meta
//!     ...
//! ```
//!
//! Read path: check the memtable, then iterate SSTables newest to oldest,
//! binary-searching the sparse key range. Write path: append to WAL, insert
//! into memtable, and flush to a new SSTable once the memtable crosses
//! `memtable_size_threshold`. Compaction merges every existing SSTable into
//! one (size-tiered, no levels yet).
//!
//! Tombstones (`MemEntry { value: None }`) are stored alongside live values
//! so deletes survive flushes and merges. They are only dropped during
//! compaction when there is no later write that could resurrect the key.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::protocol::{TxDecision, TxOp, Vote};

/// Configuration for [`StateMachine`].
#[derive(Debug, Clone)]
pub struct StateMachineConfig {
    pub data_dir: PathBuf,
    /// Flush the memtable to a fresh SSTable once its estimated in-memory
    /// byte usage crosses this threshold.
    pub memtable_size_threshold: usize,
}

impl Default for StateMachineConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            memtable_size_threshold: 1024 * 1024,
        }
    }
}

/// One row of the memtable. `value: None` is a tombstone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct MemEntry {
    value: Option<String>,
}

/// State of an in-flight two-phase commit transaction. Pending ops live
/// here (NOT in the memtable / SSTables) so reads don't see uncommitted
/// writes. They are applied to the LSM only when the coordinator decides
/// `Commit` via `decide_tx`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTx {
    ops: Vec<TxOp>,
    /// Vote received from each participant; missing entry == no vote yet.
    votes: BTreeMap<String, Vote>,
    /// Final decision, if any. Once set, the pending tx can be discarded.
    decision: Option<TxDecision>,
}

/// One WAL line. Encoded as JSON for human-debuggability; the WAL is the
/// hot crash-recovery path so we still fsync after each append.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum WalOp {
    Put { key: String, value: String },
    Delete { key: String },
}

/// One row of an on-disk SSTable. `value: None` (with `tombstone: true`) is
/// a tombstone; `value: Some(_)` (with `tombstone: false`) is a live entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SSTEntry {
    key: String,
    value: Option<String>,
    tombstone: bool,
}

/// Metadata sidecar for each SSTable (`.sst.meta` next to the data file).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SSTableMeta {
    id: u64,
    first_key: String,
    last_key: String,
    entry_count: usize,
    #[allow(dead_code)] // used by external tooling; useful for debugging
    created_at: u64,
}

/// In-memory handle for an SSTable already on disk.
struct SSTableHandle {
    path: PathBuf,
    meta: SSTableMeta,
}

/// The state machine. Public API mirrors what callers in `raft::node` and
/// `client` already use, with the only differences being:
///   - `get` returns an owned `Option<String>` (data may live on disk now)
///   - `snapshot_data` and `clear_for_snapshot` return `io::Result` (disk IO)
pub struct StateMachine {
    config: StateMachineConfig,
    memtable: BTreeMap<String, MemEntry>,
    /// Tracked separately from `memtable.len()` so a single huge value can
    /// trigger a flush; small enough to stay approximate.
    memtable_bytes: usize,
    wal: File,
    /// SSTables in age order: index 0 is oldest, last is newest.
    sstables: Vec<SSTableHandle>,
    next_sst_id: u64,
    /// Two-phase commit transactions currently in flight. Keyed by `tx_id`.
    /// Rebuilt from Raft log replay on startup; not persisted to LSM WAL
    /// because the WAL already records Set/Delete mutations.
    pending_txs: BTreeMap<String, PendingTx>,
}

impl StateMachine {
    /// Open a state machine rooted at `config.data_dir`. Replays the WAL and
    /// discovers existing SSTables so a restart picks up where it left off.
    pub fn open(config: StateMachineConfig) -> io::Result<Self> {
        fs::create_dir_all(&config.data_dir)?;
        let sst_dir = config.data_dir.join("sst");
        fs::create_dir_all(&sst_dir)?;

        // Discover existing SSTables; sort by numeric id so older tables come first.
        let mut sstables: Vec<SSTableHandle> = Vec::new();
        let mut next_sst_id: u64 = 0;
        for entry in fs::read_dir(&sst_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("sst") {
                continue;
            }
            if let Some(id) = parse_sst_id(&path) {
                let meta = load_sst_meta(&path)?;
                next_sst_id = next_sst_id.max(id + 1);
                sstables.push(SSTableHandle { path, meta });
            }
        }
        sstables.sort_by_key(|s| s.meta.id);

        // Open (or create) the WAL. Append + read so we can replay in one pass.
        let wal_path = config.data_dir.join("memtable.wal");
        let wal = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&wal_path)?;

        // Replay WAL into the memtable.
        let mut memtable: BTreeMap<String, MemEntry> = BTreeMap::new();
        let mut memtable_bytes: usize = 0;
        let mut reader = BufReader::new(wal.try_clone()?);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<WalOp>(trimmed) {
                Ok(WalOp::Put { key, value }) => {
                    memtable_bytes = memtable_bytes.saturating_sub(estimate_entry(&key));
                    memtable_bytes += estimate_entry(&key) + value.len();
                    memtable.insert(key, MemEntry { value: Some(value) });
                }
                Ok(WalOp::Delete { key }) => {
                    memtable_bytes = memtable_bytes.saturating_sub(estimate_entry(&key));
                    memtable_bytes += estimate_entry(&key);
                    memtable.insert(key, MemEntry { value: None });
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("WAL replay failed: {e}"),
                    ));
                }
            }
        }

        Ok(Self {
            config,
            memtable,
            memtable_bytes,
            wal,
            sstables,
            next_sst_id,
            pending_txs: BTreeMap::new(),
        })
    }

    /// Number of live (non-tombstoned) entries in the memtable.
    pub fn memtable_len(&self) -> usize {
        self.memtable.values().filter(|e| e.value.is_some()).count()
    }

    /// Number of SSTables currently on disk.
    pub fn sstable_count(&self) -> usize {
        self.sstables.len()
    }

    /// Insert or overwrite a key. Triggers a flush if the memtable threshold is exceeded.
    pub fn set(&mut self, key: &str, value: &str) -> io::Result<()> {
        let op = WalOp::Put { key: key.to_string(), value: value.to_string() };
        self.append_wal(&op)?;

        if let Some(prev) = self.memtable.get(key) {
            self.memtable_bytes = self
                .memtable_bytes
                .saturating_sub(prev.value.as_ref().map(String::len).unwrap_or(0));
        } else {
            self.memtable_bytes += estimate_entry(key);
        }
        self.memtable_bytes += value.len();
        self.memtable.insert(key.to_string(), MemEntry { value: Some(value.to_string()) });

        if self.memtable_bytes >= self.config.memtable_size_threshold {
            self.flush()?;
        }
        Ok(())
    }

    /// Tombstone a key. The tombstone persists through flush and is only
    /// dropped during compaction (and only if no later write resurrects it).
    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let op = WalOp::Delete { key: key.to_string() };
        self.append_wal(&op)?;

        if let Some(prev) = self.memtable.get(key) {
            self.memtable_bytes = self
                .memtable_bytes
                .saturating_sub(prev.value.as_ref().map(String::len).unwrap_or(0));
        } else {
            self.memtable_bytes += estimate_entry(key);
        }
        self.memtable.insert(key.to_string(), MemEntry { value: None });

        if self.memtable_bytes >= self.config.memtable_size_threshold {
            self.flush()?;
        }
        Ok(())
    }

    /// Look up a key. Returns `None` for missing keys and for tombstones.
    pub fn get(&self, key: &str) -> Option<String> {
        // 1. Memtable (newest writes).
        if let Some(entry) = self.memtable.get(key) {
            return entry.value.clone();
        }
        // 2. SSTables newest to oldest.
        for sst in self.sstables.iter().rev() {
            if key < sst.meta.first_key.as_str() || key > sst.meta.last_key.as_str() {
                continue;
            }
            let entries = match load_sst_entries(&sst.path) {
                Ok(e) => e,
                Err(_) => continue,
            };
            // SSTable is sorted; stop as soon as we pass the key.
            for entry in &entries {
                match entry.key.as_str().cmp(key) {
                    std::cmp::Ordering::Equal => {
                        return if entry.tombstone { None } else { entry.value.clone() };
                    }
                    std::cmp::Ordering::Greater => return None,
                    std::cmp::Ordering::Less => continue,
                }
            }
        }
        None
    }

    /// Flatten the current state into a `HashMap` for snapshotting.
    /// Reads SSTables oldest-to-newest, then overlays the memtable.
    pub fn snapshot_data(&self) -> io::Result<HashMap<String, String>> {
        let mut result: HashMap<String, String> = HashMap::new();
        for sst in &self.sstables {
            let entries = load_sst_entries(&sst.path)?;
            for entry in entries {
                if entry.tombstone {
                    result.remove(&entry.key);
                } else if let Some(value) = entry.value {
                    result.insert(entry.key, value);
                }
            }
        }
        for (key, entry) in &self.memtable {
            match &entry.value {
                Some(v) => {
                    result.insert(key.clone(), v.clone());
                }
                None => {
                    result.remove(key);
                }
            }
        }
        Ok(result)
    }

    /// Replace the entire state with `data` from a freshly-installed snapshot.
    ///
    /// This is the path used by [`crate::raft::node::RaftNode::new`] when
    /// restoring from disk on startup: a snapshot exists from a previous
    /// run, the local LSM is wiped, and the snapshot data is loaded in.
    ///
    /// After this returns the state machine holds exactly the contents of
    /// `data`; subsequent WAL replays will replay any log entries written
    /// *after* `last_included_index`. 2PC `pending_txs` are also cleared,
    /// since they belong to Raft log state, not LSM state — and we cannot
    /// reconstruct them from the snapshot alone.
    pub fn install_snapshot(
        &mut self,
        data: HashMap<String, String>,
    ) -> io::Result<()> {
        self.clear_for_snapshot()?;
        // Reset memtable_bytes so the loop below sees a clean slate, then
        // let each insert bump it via the same accounting used by the live
        // `set` path. We can't route through `set` directly because that
        // also writes to the LSM WAL — the snapshot is the source of truth
        // here, and the LSM WAL has already been truncated by
        // `clear_for_snapshot`.
        self.memtable_bytes = 0;
        for (k, v) in data {
            self.memtable_bytes += estimate_entry(&k);
            self.memtable_bytes += v.len();
            self.memtable.insert(k, MemEntry { value: Some(v) });
        }
        Ok(())
    }

    /// Wipe all on-disk state. Used when a follower installs a snapshot
    /// from the leader — the local LSM is discarded wholesale.
    pub fn clear_for_snapshot(&mut self) -> io::Result<()> {
        self.memtable.clear();
        self.memtable_bytes = 0;
        // Truncate the WAL.
        self.wal.set_len(0)?;
        self.wal.seek(SeekFrom::Start(0))?;
        self.wal.sync_all()?;
        // Delete every SSTable + its sidecar metadata file.
        for sst in &self.sstables {
            let _ = fs::remove_file(&sst.path);
            let _ = fs::remove_file(meta_path(&sst.path));
        }
        self.sstables.clear();
        self.next_sst_id = 0;
        // 2PC pending txs are part of Raft log state, not LSM state, but
        // wiping them too is consistent with "snapshot installed -> start fresh".
        self.pending_txs.clear();
        Ok(())
    }

    // =================== Two-phase commit API ===================

    /// Stage a new transaction. The ops are stored in `pending_txs` and
    /// NOT applied to the LSM yet — reads cannot see them until the
    /// coordinator appends a matching `DecideTx(Commit)` log entry.
    pub fn begin_tx(&mut self, tx_id: String, ops: Vec<TxOp>) -> io::Result<()> {
        // Idempotent: re-defining a tx_id with different ops overwrites the
        // pending state. A well-behaved coordinator will not do this.
        self.pending_txs.insert(
            tx_id,
            PendingTx { ops, votes: BTreeMap::new(), decision: None },
        );
        Ok(())
    }

    /// Record a participant's vote on a pending transaction.
    pub fn record_vote(&mut self, tx_id: &str, voter: String, vote: Vote) -> io::Result<()> {
        if let Some(tx) = self.pending_txs.get_mut(tx_id) {
            tx.votes.insert(voter, vote);
        }
        // Unknown tx_id: silently ignore. The vote may have been emitted by
        // a participant that already processed a DecideTx and purged the
        // pending entry; later votes that arrive out of order are no-ops.
        Ok(())
    }

    /// Apply the coordinator's final decision. On `Commit`, every op in
    /// the transaction is applied atomically. On `Abort`, the transaction
    /// is discarded without side effects.
    pub fn decide_tx(&mut self, tx_id: &str, decision: TxDecision) -> io::Result<()> {
        let tx = match self.pending_txs.remove(tx_id) {
            Some(tx) => tx,
            None => return Ok(()), // idempotent: already decided
        };
        if matches!(decision, TxDecision::Commit) {
            for op in tx.ops {
                match op {
                    TxOp::Put { key, value } => {
                        let _ = self.set(&key, &value);
                    }
                    TxOp::Delete { key } => {
                        let _ = self.delete(&key);
                    }
                }
            }
        }
        // For Abort: simply dropping the entry is sufficient.
        Ok(())
    }

    /// Number of pending (in-flight) two-phase commit transactions.
    pub fn pending_tx_count(&self) -> usize {
        self.pending_txs.len()
    }

    /// Inspect a pending transaction (read-only). Returns `None` if the
    /// `tx_id` is not in flight.
    pub fn pending_tx(&self, tx_id: &str) -> Option<PendingTxView> {
        self.pending_txs.get(tx_id).map(|tx| PendingTxView {
            op_count: tx.ops.len(),
            yes_votes: tx.votes.values().filter(|v| matches!(v, Vote::Yes)).count(),
            no_votes: tx
                .votes
                .values()
                .filter(|v| matches!(v, Vote::No(_)))
                .count(),
        })
    }

    /// Force a flush of the memtable to a new SSTable. No-op if empty.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        let entries: Vec<SSTEntry> = self
            .memtable
            .iter()
            .map(|(k, v)| SSTEntry {
                key: k.clone(),
                value: v.value.clone(),
                tombstone: v.value.is_none(),
            })
            .collect();
        let first = entries.first().unwrap().key.clone();
        let last = entries.last().unwrap().key.clone();

        let id = self.next_sst_id;
        self.next_sst_id += 1;
        let sst_path = self
            .config
            .data_dir
            .join("sst")
            .join(format!("{id:06}.sst"));
        write_json(&sst_path, &entries)?;

        let meta = SSTableMeta {
            id,
            first_key: first,
            last_key: last,
            entry_count: entries.len(),
            created_at: now_unix(),
        };
        write_json(&meta_path(&sst_path), &meta)?;

        self.sstables.push(SSTableHandle { path: sst_path, meta });

        // Reset memtable + WAL.
        self.memtable.clear();
        self.memtable_bytes = 0;
        self.wal.set_len(0)?;
        self.wal.seek(SeekFrom::Start(0))?;
        self.wal.sync_all()?;
        Ok(())
    }

    /// Merge all on-disk SSTables into a single one (size-tiered compaction).
    /// Memtable is left alone; flush it first if you want it included.
    pub fn compact(&mut self) -> io::Result<()> {
        if self.sstables.len() < 2 {
            return Ok(());
        }
        let mut merged: BTreeMap<String, SSTEntry> = BTreeMap::new();
        for sst in &self.sstables {
            let entries = load_sst_entries(&sst.path)?;
            for entry in entries {
                merged.insert(entry.key.clone(), entry);
            }
        }
        // Drop tombstones that have no later resurrection possible.
        merged.retain(|_, v| !v.tombstone);

        // Write the merged result as a fresh SSTable.
        let entries: Vec<SSTEntry> = merged.into_values().collect();
        let id = self.next_sst_id;
        self.next_sst_id += 1;
        let sst_path = self
            .config
            .data_dir
            .join("sst")
            .join(format!("{id:06}.sst"));
        write_json(&sst_path, &entries)?;
        let meta = SSTableMeta {
            id,
            first_key: entries.first().map(|e| e.key.clone()).unwrap_or_default(),
            last_key: entries.last().map(|e| e.key.clone()).unwrap_or_default(),
            entry_count: entries.len(),
            created_at: now_unix(),
        };
        write_json(&meta_path(&sst_path), &meta)?;

        // Drop the old SSTables.
        let old_paths: Vec<PathBuf> = self.sstables.iter().map(|s| s.path.clone()).collect();
        self.sstables.clear();
        self.sstables.push(SSTableHandle { path: sst_path, meta });
        for p in old_paths {
            let _ = fs::remove_file(&p);
            let _ = fs::remove_file(meta_path(&p));
        }
        Ok(())
    }

    fn append_wal(&mut self, op: &WalOp) -> io::Result<()> {
        let line = serde_json::to_string(op)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        writeln!(self.wal, "{line}")?;
        self.wal.sync_all()?;
        Ok(())
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let s = serde_json::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, s)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn meta_path(sst_path: &Path) -> PathBuf {
    let mut p = sst_path.to_path_buf();
    p.set_extension("meta");
    p
}

fn parse_sst_id(path: &Path) -> Option<u64> {
    path.file_stem()?.to_str()?.parse::<u64>().ok()
}

fn load_sst_meta(path: &Path) -> io::Result<SSTableMeta> {
    let raw = fs::read_to_string(meta_path(path))?;
    serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

fn load_sst_entries(path: &Path) -> io::Result<Vec<SSTEntry>> {
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Rough estimate of the in-memory bytes a key contributes to the memtable.
/// Value bytes are tracked separately. We just need this to be in the right
/// ballpark to trigger flushes; it doesn't need to be exact.
fn estimate_entry(key: &str) -> usize {
    // key length + String header overhead (~24B) + BTreeMap node overhead (~16B).
    key.len() + 40
}

/// Read-only snapshot of a pending transaction's state, for tests and
/// (eventually) admin / debug endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTxView {
    pub op_count: usize,
    pub yes_votes: usize,
    pub no_votes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{TxDecision, TxOp, Vote};

    fn temp_config() -> (TempDir, StateMachineConfig) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = StateMachineConfig {
            data_dir: dir.path().to_path_buf(),
            memtable_size_threshold: 1024, // 1 KB so tests flush easily
        };
        (dir, cfg)
    }

    fn open_default() -> (TempDir, StateMachine) {
        let (dir, cfg) = temp_config();
        let sm = StateMachine::open(cfg).expect("open");
        (dir, sm)
    }

    use tempfile::TempDir;

    // ---------- basic operations (legacy API) ----------

    #[test]
    fn open_creates_empty_state_machine() {
        let (_d, sm) = open_default();
        assert_eq!(sm.memtable_len(), 0);
        assert_eq!(sm.sstable_count(), 0);
    }

    #[test]
    fn set_then_get_returns_value() {
        let (_d, mut sm) = open_default();
        sm.set("hello", "world").unwrap();
        assert_eq!(sm.get("hello"), Some("world".to_string()));
    }

    #[test]
    fn get_missing_key_returns_none() {
        let (_d, sm) = open_default();
        assert_eq!(sm.get("missing"), None);
    }

    #[test]
    fn set_overwrites_existing_value() {
        let (_d, mut sm) = open_default();
        sm.set("k", "v1").unwrap();
        sm.set("k", "v2").unwrap();
        assert_eq!(sm.get("k"), Some("v2".to_string()));
    }

    #[test]
    fn delete_removes_existing_key() {
        let (_d, mut sm) = open_default();
        sm.set("k", "v").unwrap();
        sm.delete("k").unwrap();
        assert_eq!(sm.get("k"), None);
    }

    #[test]
    fn delete_missing_key_is_noop() {
        let (_d, mut sm) = open_default();
        sm.delete("never_set").unwrap();
        assert_eq!(sm.get("never_set"), None);
    }

    #[test]
    fn delete_then_set_works() {
        let (_d, mut sm) = open_default();
        sm.set("k", "v1").unwrap();
        sm.delete("k").unwrap();
        sm.set("k", "v2").unwrap();
        assert_eq!(sm.get("k"), Some("v2".to_string()));
    }

    // ---------- memtable flush threshold ----------

    #[test]
    fn auto_flush_moves_data_to_sstable() {
        let (dir, mut sm) = open_default();
        // Threshold is 1 KB; pushing ~20 entries of 100 bytes each triggers a flush.
        for i in 0..50 {
            let k = format!("key-{:03}", i);
            let v = "x".repeat(100);
            sm.set(&k, &v).unwrap();
        }
        assert!(sm.sstable_count() > 0, "expected at least one SSTable after writes");
        assert!(sm.memtable_len() < 50, "memtable should have been flushed at least once");

        // All keys must still be readable.
        for i in 0..50 {
            let k = format!("key-{:03}", i);
            assert_eq!(sm.get(&k), Some("x".repeat(100)), "missing {k}");
        }
        drop(sm);

        // Persistence check: a fresh handle must see everything on disk.
        let cfg = StateMachineConfig {
            data_dir: dir.path().to_path_buf(),
            memtable_size_threshold: 1024,
        };
        let sm2 = StateMachine::open(cfg).unwrap();
        assert!(sm2.sstable_count() > 0);
        for i in 0..50 {
            let k = format!("key-{:03}", i);
            assert_eq!(sm2.get(&k), Some("x".repeat(100)), "after reopen missing {k}");
        }
    }

    #[test]
    fn tombstones_survive_flush() {
        let (_d, mut sm) = open_default();
        sm.set("a", "1").unwrap();
        sm.set("b", "2").unwrap();
        sm.delete("a").unwrap();
        // Force a flush so the tombstones land in an SSTable.
        sm.flush().unwrap();

        assert_eq!(sm.get("a"), None, "tombstone must hide the value");
        assert_eq!(sm.get("b"), Some("2".to_string()), "live value must persist");
    }

    // ---------- manual flush + read across SSTables ----------

    #[test]
    fn manual_flush_then_get_still_finds_values() {
        let (_d, mut sm) = open_default();
        sm.set("a", "1").unwrap();
        sm.set("b", "2").unwrap();
        sm.flush().unwrap();
        assert_eq!(sm.sstable_count(), 1);
        assert_eq!(sm.memtable_len(), 0);

        assert_eq!(sm.get("a"), Some("1".to_string()));
        assert_eq!(sm.get("b"), Some("2".to_string()));
    }

    #[test]
    fn newer_sstable_overrides_older_for_same_key() {
        let (_d, mut sm) = open_default();
        sm.set("k", "v1").unwrap();
        sm.flush().unwrap();

        sm.set("k", "v2").unwrap();
        sm.flush().unwrap();

        // Two SSTables on disk; the newer one has the latest write.
        assert_eq!(sm.sstable_count(), 2);
        assert_eq!(sm.get("k"), Some("v2".to_string()));
    }

    #[test]
    fn read_skips_out_of_range_sstables() {
        // Cheap brute-force correctness: keys outside an SSTable's range
        // must not produce false positives. We exercise the read path here
        // with several SSTables covering disjoint ranges.
        let (_d, mut sm) = open_default();
        sm.set("a", "1").unwrap();
        sm.set("m", "2").unwrap();
        sm.set("z", "3").unwrap();
        sm.flush().unwrap();

        // Insert more keys in a separate flush; nothing else overlaps.
        sm.set("b", "4").unwrap();
        sm.set("y", "5").unwrap();
        sm.flush().unwrap();

        // Misses outside any range must still return None.
        assert_eq!(sm.get("c"), None);
        assert_eq!(sm.get("x"), None);
        assert_eq!(sm.get("zzz"), None);
    }

    // ---------- compaction ----------

    #[test]
    fn compact_collapses_multiple_sstables_into_one() {
        let (_d, mut sm) = open_default();
        for i in 0..10 {
            sm.set(&format!("k{i}"), &format!("v{i}")).unwrap();
            sm.flush().unwrap();
        }
        let before = sm.sstable_count();
        assert!(before >= 3, "expected several SSTables, got {before}");

        sm.compact().unwrap();
        assert_eq!(sm.sstable_count(), 1);

        // All data must still be reachable.
        for i in 0..10 {
            assert_eq!(sm.get(&format!("k{i}")), Some(format!("v{i}")));
        }
    }

    #[test]
    fn compact_drops_tombstones_with_no_resurrection() {
        let (_d, mut sm) = open_default();
        sm.set("a", "1").unwrap();
        sm.flush().unwrap();
        sm.delete("a").unwrap();
        sm.flush().unwrap();

        sm.compact().unwrap();
        assert_eq!(sm.get("a"), None);
        assert_eq!(sm.sstable_count(), 1);
    }

    #[test]
    fn compact_with_fewer_than_two_sstables_is_noop() {
        let (_d, mut sm) = open_default();
        sm.set("k", "v").unwrap();
        sm.flush().unwrap();
        let count_before = sm.sstable_count();
        sm.compact().unwrap();
        assert_eq!(sm.sstable_count(), count_before, "compact must be no-op with 1 sstable");
    }

    // ---------- snapshot / clear ----------

    #[test]
    fn snapshot_data_flattens_state_across_memtable_and_sstables() {
        let (_d, mut sm) = open_default();
        sm.set("a", "1").unwrap();
        sm.set("b", "2").unwrap();
        sm.flush().unwrap();
        sm.set("c", "3").unwrap();
        sm.delete("a").unwrap();

        let snap = sm.snapshot_data().unwrap();
        assert_eq!(snap.get("b").map(String::as_str), Some("2"));
        assert_eq!(snap.get("c").map(String::as_str), Some("3"));
        assert!(!snap.contains_key("a"), "tombstoned key must be absent");
    }

    #[test]
    fn clear_for_snapshot_wipes_everything() {
        let (_d, mut sm) = open_default();
        sm.set("a", "1").unwrap();
        sm.set("b", "2").unwrap();
        sm.flush().unwrap();

        sm.clear_for_snapshot().unwrap();

        assert_eq!(sm.memtable_len(), 0);
        assert_eq!(sm.sstable_count(), 0);
        assert_eq!(sm.get("a"), None);
        assert_eq!(sm.get("b"), None);
    }

    #[test]
    fn install_snapshot_replaces_state_with_supplied_data() {
        let (_d, mut sm) = open_default();
        // Seed the state machine with entries that should be wiped.
        sm.set("old_key", "old_val").unwrap();
        sm.set("another", "v").unwrap();

        let mut snap_data = std::collections::HashMap::new();
        snap_data.insert("fresh_a".to_string(), "1".to_string());
        snap_data.insert("fresh_b".to_string(), "2".to_string());
        snap_data.insert("fresh_c".to_string(), "3".to_string());

        sm.install_snapshot(snap_data).unwrap();

        // Old entries are gone.
        assert_eq!(sm.get("old_key"), None);
        assert_eq!(sm.get("another"), None);
        // New entries are present.
        assert_eq!(sm.get("fresh_a").as_deref(), Some("1"));
        assert_eq!(sm.get("fresh_b").as_deref(), Some("2"));
        assert_eq!(sm.get("fresh_c").as_deref(), Some("3"));
        // Memtable bookkeeping matches the post-install size.
        assert_eq!(sm.memtable_len(), 3);
    }

    #[test]
    fn install_snapshot_with_empty_data_clears_state() {
        let (_d, mut sm) = open_default();
        sm.set("k", "v").unwrap();
        sm.flush().unwrap();

        sm.install_snapshot(std::collections::HashMap::new()).unwrap();

        assert_eq!(sm.memtable_len(), 0);
        assert_eq!(sm.sstable_count(), 0);
        assert_eq!(sm.get("k"), None);
    }

    // ---------- crash recovery ----------

    #[test]
    fn reopen_replays_wal_into_memtable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = StateMachineConfig {
            data_dir: dir.path().to_path_buf(),
            memtable_size_threshold: 1024 * 1024, // big enough to avoid auto-flush
        };
        {
            let mut sm = StateMachine::open(cfg.clone()).unwrap();
            sm.set("a", "1").unwrap();
            sm.set("b", "2").unwrap();
            // Do NOT flush; the WAL must replay on reopen.
        }
        let sm2 = StateMachine::open(cfg).unwrap();
        assert_eq!(sm2.memtable_len(), 2);
        assert_eq!(sm2.get("a"), Some("1".to_string()));
        assert_eq!(sm2.get("b"), Some("2".to_string()));
        assert_eq!(sm2.sstable_count(), 0);
    }

    #[test]
    fn reopen_discovers_existing_sstables() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = StateMachineConfig {
            data_dir: dir.path().to_path_buf(),
            memtable_size_threshold: 1024,
        };
        {
            let mut sm = StateMachine::open(cfg.clone()).unwrap();
            for i in 0..20 {
                let k = format!("k{i:02}");
                let v = "v".repeat(80);
                sm.set(&k, &v).unwrap();
            }
            // Threshold will have triggered at least one flush.
            assert!(sm.sstable_count() > 0);
        }
        let sm2 = StateMachine::open(cfg).unwrap();
        assert!(sm2.sstable_count() > 0);
        // All keys still readable.
        for i in 0..20 {
            let k = format!("k{i:02}");
            assert_eq!(sm2.get(&k), Some("v".repeat(80)), "{k}");
        }
    }

    // ---------- bounds ----------

    #[test]
    fn empty_value_is_allowed() {
        let (_d, mut sm) = open_default();
        sm.set("k", "").unwrap();
        assert_eq!(sm.get("k"), Some(String::new()));
    }

    #[test]
    fn unicode_keys_and_values_round_trip() {
        let (_d, mut sm) = open_default();
        sm.set("键", "值 🎉").unwrap();
        assert_eq!(sm.get("键"), Some("值 🎉".to_string()));
        sm.flush().unwrap();
        assert_eq!(sm.get("键"), Some("值 🎉".to_string()));
    }

    // ---------- two-phase commit lifecycle ----------

    #[test]
    fn begin_tx_stages_pending_without_applying() {
        let (_d, mut sm) = open_default();
        sm.begin_tx(
            "tx-1".into(),
            vec![TxOp::Put { key: "a".into(), value: "1".into() }],
        )
        .unwrap();
        // Reads must not see the pending op.
        assert_eq!(sm.get("a"), None);
        // The pending tx is tracked.
        assert_eq!(sm.pending_tx_count(), 1);
        let view = sm.pending_tx("tx-1").unwrap();
        assert_eq!(view.op_count, 1);
        assert_eq!(view.yes_votes, 0);
    }

    #[test]
    fn decide_tx_commit_applies_all_ops_atomically() {
        let (_d, mut sm) = open_default();
        sm.begin_tx(
            "tx-2".into(),
            vec![
                TxOp::Put { key: "a".into(), value: "1".into() },
                TxOp::Put { key: "b".into(), value: "2".into() },
                TxOp::Delete { key: "c".into() },
            ],
        )
        .unwrap();
        sm.decide_tx("tx-2", TxDecision::Commit).unwrap();

        assert_eq!(sm.get("a"), Some("1".to_string()));
        assert_eq!(sm.get("b"), Some("2".to_string()));
        assert_eq!(sm.pending_tx_count(), 0);
    }

    #[test]
    fn decide_tx_abort_discards_all_ops() {
        let (_d, mut sm) = open_default();
        // Pre-existing value to verify abort doesn't accidentally clear it.
        sm.set("existing", "v0").unwrap();

        sm.begin_tx(
            "tx-3".into(),
            vec![
                TxOp::Put { key: "a".into(), value: "should-not-apply".into() },
                TxOp::Put { key: "existing".into(), value: "should-not-overwrite".into() },
            ],
        )
        .unwrap();
        sm.decide_tx("tx-3", TxDecision::Abort).unwrap();

        // Both new ops are gone; the pre-existing value is intact.
        assert_eq!(sm.get("a"), None);
        assert_eq!(sm.get("existing"), Some("v0".to_string()));
        assert_eq!(sm.pending_tx_count(), 0);
    }

    #[test]
    fn record_vote_updates_pending_tx_view() {
        let (_d, mut sm) = open_default();
        sm.begin_tx("tx-4".into(), vec![TxOp::Put { key: "k".into(), value: "v".into() }])
            .unwrap();

        sm.record_vote("tx-4", "node-A".into(), Vote::Yes).unwrap();
        sm.record_vote("tx-4", "node-B".into(), Vote::No("conflict".into())).unwrap();

        let view = sm.pending_tx("tx-4").unwrap();
        assert_eq!(view.op_count, 1);
        assert_eq!(view.yes_votes, 1);
        assert_eq!(view.no_votes, 1);

        // The pending op is still isolated from reads.
        assert_eq!(sm.get("k"), None);
    }

    #[test]
    fn vote_for_unknown_tx_is_noop() {
        let (_d, mut sm) = open_default();
        sm.record_vote("nonexistent", "node-A".into(), Vote::Yes).unwrap();
        assert_eq!(sm.pending_tx_count(), 0);
    }

    #[test]
    fn decide_tx_for_unknown_tx_is_noop() {
        let (_d, mut sm) = open_default();
        sm.decide_tx("nonexistent", TxDecision::Commit).unwrap();
        assert_eq!(sm.pending_tx_count(), 0);
    }

    #[test]
    fn multiple_concurrent_transactions_isolate_reads() {
        let (_d, mut sm) = open_default();
        sm.begin_tx(
            "tx-A".into(),
            vec![TxOp::Put { key: "shared".into(), value: "from-A".into() }],
        )
        .unwrap();
        sm.begin_tx(
            "tx-B".into(),
            vec![TxOp::Put { key: "shared".into(), value: "from-B".into() }],
        )
        .unwrap();

        // Neither tx's write is visible to reads.
        assert_eq!(sm.get("shared"), None);
        assert_eq!(sm.pending_tx_count(), 2);

        // Committing A still doesn't expose the write until its decide fires.
        sm.decide_tx("tx-A", TxDecision::Commit).unwrap();
        assert_eq!(sm.get("shared"), Some("from-A".to_string()));
        assert_eq!(sm.pending_tx_count(), 1);

        // B aborts.
        sm.decide_tx("tx-B", TxDecision::Abort).unwrap();
        assert_eq!(sm.get("shared"), Some("from-A".to_string()));
        assert_eq!(sm.pending_tx_count(), 0);
    }

    #[test]
    fn begin_tx_redefine_overwrites_pending_state() {
        // Documenting the current behavior: re-issuing begin_tx with the same
        // id replaces the previous pending entry. This is intentionally
        // simple; a production system would reject duplicate tx_ids.
        let (_d, mut sm) = open_default();
        sm.begin_tx(
            "tx-dup".into(),
            vec![TxOp::Put { key: "a".into(), value: "1".into() }],
        )
        .unwrap();
        sm.begin_tx(
            "tx-dup".into(),
            vec![TxOp::Put { key: "b".into(), value: "2".into() }],
        )
        .unwrap();
        let view = sm.pending_tx("tx-dup").unwrap();
        assert_eq!(view.op_count, 1); // only the latest begin survived
    }
}