use rand::seq::index::IndexVec;
use serde::{Deserialize, Serialize};

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