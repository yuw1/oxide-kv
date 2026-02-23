use crate::raft::node::{NodeState, RaftNode};
use rand::Rng;
use std::sync::{Arc, RwLock};
use tokio::time::Duration;
use crate::config::Config;

pub async fn run_election_timer(raft: Arc<RwLock<RaftNode>>) {
    loop {
        // 1. Randomize timeout duration to prevent split votes
        let timeout_ms = {
            let mut rng = rand::thread_rng();
            // Using associated functions from Config as established previously
            rand::Rng::gen_range(&mut rng, Config::min_election_timeout_ms()..Config::max_election_timeout_ms())
        };
        let timeout = Duration::from_millis(timeout_ms);

        tokio::time::sleep(timeout).await;

        // 2. Check if election is needed (use read lock first for better performance)
        let should_elect = {
            let node = raft.read().unwrap();
            // If not a Leader and heartbeat has timed out, start election
            if node.state != NodeState::Leader && node.last_heartbeat.elapsed() >= timeout {
                println!("⏰ Election timeout ({}ms), starting election...", timeout_ms);
                true
            } else {
                false
            }
        };

        if should_elect {
            // Initiate candidate state and request votes
            RaftNode::become_candidate(raft.clone());
        }
    }
}