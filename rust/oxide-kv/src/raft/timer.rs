use crate::raft::node::{NodeState, RaftNode};
use crate::raft::clock::{system_clock, Clock};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use crate::config::Config;
use tracing::info;

/// Pure decision function: should the node start a new election at `now`?
///
/// Rules (Raft §5.2):
///   - The Leader must never election-timeout; it renews authority via heartbeats.
///   - Followers and Candidates elect when `last_heartbeat` is older than the
///     election threshold (`min_election_timeout_ms`).
///
/// This is intentionally a pure function so the unit tests can cover the
/// branch logic without spinning up a tokio runtime or a cluster.
pub fn should_start_election(
    state: NodeState,
    last_heartbeat: Instant,
    now: Instant,
    election_threshold: Duration,
) -> bool {
    if state == NodeState::Leader {
        return false;
    }
    now.duration_since(last_heartbeat) >= election_threshold
}

pub async fn run_election_timer(raft: Arc<RwLock<RaftNode>>) {
    // Fixed threshold for "have we gone too long without hearing from a leader?".
    // Using the minimum election timeout (not the sleep duration) is critical:
    // the previous implementation compared `last_heartbeat.elapsed()` against the
    // *sleep* duration, which is always true after a sleep, causing a candidate
    // election on every loop iteration → term inflation → leader churn.
    let election_threshold = Duration::from_millis(Config::min_election_timeout_ms());

    // Pull the clock out once. The election timer reads `now()` against
    // `last_heartbeat` (held inside RaftNode), so it must use the same
    // clock as the node does. Pulling it from the node guarantees
    // consistency; in simulation, this is the SimClock the harness
    // injected at construction time.
    let clock = raft.read().unwrap().clock.clone();

    loop {
        // 1. Randomize the *sleep* duration to spread elections across nodes
        //    and avoid split votes. The sleep is just a polling cadence; it is
        //    NOT the threshold for starting an election.
        let sleep_ms = {
            let mut rng = rand::thread_rng();
            rand::Rng::gen_range(
                &mut rng,
                Config::min_election_timeout_ms()..Config::max_election_timeout_ms(),
            )
        };
        clock.sleep(Duration::from_millis(sleep_ms)).await;

        // 2. Snapshot the state under the read lock, then release before
        //    deciding + sleeping. This keeps the critical section short.
        let (state, last_heartbeat) = {
            let node = raft.read().unwrap();
            (node.state, node.last_heartbeat)
        };

        if !should_start_election(state, last_heartbeat, clock.now(), election_threshold) {
            continue;
        }

        // 3. Small post-threshold jitter so two followers who timed out in the
        //    same tick don't immediately re-collide on the same term.
        let jitter_ms = {
            let mut rng = rand::thread_rng();
            rand::Rng::gen_range(&mut rng, 0u64..200u64)
        };
        clock.sleep(Duration::from_millis(jitter_ms)).await;

        // 4. Re-check after jitter: another path (incoming heartbeat, vote
        //    reply with higher term, etc.) may have refreshed `last_heartbeat`
        //    or changed our role, and we must avoid starting an unnecessary
        //    election round.
        let (state2, last_heartbeat2) = {
            let node = raft.read().unwrap();
            (node.state, node.last_heartbeat)
        };
        if should_start_election(state2, last_heartbeat2, clock.now(), election_threshold) {
            info!(
                sleep_ms,
                jitter_ms,
                "election timeout; starting pre-vote probe"
            );
            // P8 PR 5 (Raft §9.6): the timer entry point now goes
            // through the pre-vote probe phase rather than bumping
            // `current_term` directly. This avoids the disruptive
            // server problem on partition recovery: a freshly
            // recovered follower used to immediately `become_candidate`,
            // which forced the live leader to step down on the next
            // AppendEntries, churning the term until the partition
            // fully healed. With pre-vote, the recovered follower
            // probes first; if the cluster still has a live leader,
            // the probe is refused (election restriction /
            // higher-term-peer check), and `current_term` stays put.
            // Tests / simulation harnesses still call `become_candidate`
            // directly when they want to skip the probe.
            RaftNode::become_pre_candidate(raft.clone());
        }
    }
}

/// Helper used by `main.rs` and tests to bootstrap the election
/// timer on a freshly constructed `RaftNode` that already has a
/// `clock` injected. Today this is just `system_clock()`; it exists
/// so future test wiring has a single seam to substitute a `SimClock`
/// without grepping for every callsite.
#[allow(dead_code)]
pub fn default_clock() -> Arc<dyn Clock> {
    system_clock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::time::Duration;

    #[test]
    fn leader_never_elects_even_after_long_silence() {
        let t = Instant::now();
        let threshold = Duration::from_millis(Config::min_election_timeout_ms());
        // Even after 10 minutes of silence, a Leader must not start an election.
        let far_future = t + Duration::from_secs(600);
        assert!(!should_start_election(
            NodeState::Leader,
            t,
            far_future,
            threshold
        ));
    }

    #[test]
    fn follower_within_threshold_does_not_elect() {
        let t = Instant::now();
        let threshold = Duration::from_millis(Config::min_election_timeout_ms());
        let just_before = t + threshold - Duration::from_millis(1);
        assert!(!should_start_election(
            NodeState::Follower,
            t,
            just_before,
            threshold
        ));
    }

    #[test]
    fn follower_at_threshold_elects() {
        let t = Instant::now();
        let threshold = Duration::from_millis(Config::min_election_timeout_ms());
        let exactly = t + threshold;
        assert!(should_start_election(
            NodeState::Follower,
            t,
            exactly,
            threshold
        ));
    }

    #[test]
    fn follower_past_threshold_elects() {
        let t = Instant::now();
        let threshold = Duration::from_millis(Config::min_election_timeout_ms());
        let way_after = t + 2 * threshold;
        assert!(should_start_election(
            NodeState::Follower,
            t,
            way_after,
            threshold
        ));
    }

    #[test]
    fn candidate_also_uses_threshold() {
        let t = Instant::now();
        let threshold = Duration::from_millis(Config::min_election_timeout_ms());
        // Candidates re-elect when their last heartbeat is stale too.
        let stale = t + 2 * threshold;
        assert!(should_start_election(
            NodeState::Candidate,
            t,
            stale,
            threshold
        ));
    }

    #[test]
    fn ratio_constraint_holds_for_config() {
        // Heartbeat must fit comfortably inside the election window so a single
        // dropped heartbeat doesn't cause an election. Industry standard is
        // heartbeat : election ≤ 1:10; we ship 1:20 to be safe.
        let hb = Config::heartbeat_interval_ms();
        let min = Config::min_election_timeout_ms();
        assert!(
            min >= 10 * hb,
            "min election timeout ({}ms) must be ≥ 10× heartbeat ({}ms)",
            min,
            hb
        );
    }
}