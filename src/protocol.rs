use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Command {
    Set { key: String, value: String },
    Get { key: String },
    Delete { key: String },
    Compact,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogEntry {
    pub(crate) term: u64,
    pub index: usize,
    pub(crate) command: Command,
}

/// A serialized state-machine snapshot at a known log position.
///
/// `last_included_index` / `last_included_term` identify the log entry whose
/// effect is fully captured by `data`. All log entries at indices
/// `<= last_included_index` can be discarded after the snapshot is installed.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Snapshot {
    pub last_included_index: u64,
    pub last_included_term: u64,
    pub data: HashMap<String, String>,
}