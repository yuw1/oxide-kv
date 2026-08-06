//! Fault scheduling for deterministic simulation testing (P7).
//!
//! A [`FaultScheduler`] sits between [`SimTransport::send_raft`]
//! and the receiver's inbound channel. Every outbound message
//! passes through the scheduler, which decides one of three
//! outcomes:
//!
//! - **Deliver**: forward the message immediately.
//! - **Drop**: silently discard the message (the sender sees a
//!   `TransportError::Timeout` if it was waiting on a reply — the
//!   receiver never sees the message, so no reply will arrive).
//! - **Delay(d)**: forward after `d` of virtual time has elapsed.
//!   The sender's `rpc_timeout` may fire first and return
//!   `Timeout`; the message may still arrive later.
//!
//! ## Why the scheduler is a separate trait
//!
//! Decoupling the scheduler from `SimTransport` lets the future
//! [`SimHarness`] inject deterministic faults (drops on
//! specific RPC types, partitions that flip back after N virtual
//! ticks, jitter) without re-implementing `SimTransport`. The
//! cluster setup stays the same; only the scheduler changes.
//!
//! ## Object-safe
//!
//! Uses `Box<dyn Future + Send>` for the async `before_send`
//! return so the trait is dyn-compatible. The caller
//! (`SimTransport`) stores the scheduler as
//! `Arc<dyn FaultScheduler + Send + Sync>` so it can be shared
//! across the cluster's SimTransports.
//!
//! ## Why a `Delay` outcome at all
//!
//! A delayed message still arrives, just later. The sender's
//! `rpc_timeout` may fire before delivery (returning
//! `TransportError::Timeout`), in which case the sender treats
//! the round-trip as failed but the receiver will eventually
//! see the late message. This models the classic "slow link"
//! scenario in Raft testing.
//!
//! Note: the `Delay` outcome is now properly honoured —
//! `SimTransport::send_raft` and `send_vote` sleep the
//! requested delay (in wall-clock time, since `tokio::time::sleep`
//! is what they use) before pushing. If the delay exceeds the
//! `rpc_timeout`, the sender surfaces `TransportError::Timeout`
//! while the receiver still receives the late message — modelling
//! the classic "slow link" scenario in Raft testing. Virtual-clock
//! alignment of the delay is deferred to a future PR (would
//! require threading `Clock` through `Network`).

use crate::raft::sim_transport::InboundMessageBody;
use std::sync::Mutex;
use std::time::Duration;

/// Identifies a directed link in the simulated cluster. A
/// partition on `(from="n1", to="n2")` does NOT affect the
/// `(from="n2", to="n1")` direction — each direction is its own
/// link state. This matches how real network partitions behave
/// in practice (asymmetric link failures exist).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LinkId {
    pub from: String,
    pub to: String,
}

impl LinkId {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }
}

/// What the scheduler decided for an outbound message.
#[derive(Debug)]
pub enum ScheduleOutcome {
    /// Forward the message to the receiver immediately.
    Deliver,
    /// Discard the message. The sender's RPC will time out (if
    /// it was waiting on a reply) — the sender never sees a
    /// reply.
    Drop,
    /// Forward the message after `delay` of virtual time has
    /// elapsed. The sender may time out before delivery.
    Delay(Duration),
    /// Forward the message twice — once immediately and once
    /// after `delay`. Models packet duplication on a lossy
    /// link. The duplicated message has the same `from` /
    /// `body` as the original. Useful for verifying that the
    /// receiver's handlers are idempotent under duplicate
    /// delivery (Raft AppendEntries and RequestVote are by
    /// construction; snapshot install needs special care).
    Duplicate(Duration),
}

/// Future returned by [`FaultScheduler::before_send`]. Resolves to
/// the outcome the caller should apply.
pub type ScheduleFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = ScheduleOutcome> + Send>>;

/// A fault scheduler decides, per outbound message, whether to
/// deliver it / drop it / delay it. The scheduler is consulted
/// on the SENDER side before the message is pushed into the
/// receiver's inbound channel.
///
/// The scheduler is held as `Arc<dyn FaultScheduler + Send +
/// Sync>` by each `SimTransport` (and shared across the cluster
/// via the [`Network`]). It must be thread-safe and cheap to
/// clone (it's already an Arc).
///
/// The `clock` field gives the scheduler access to virtual time
/// (e.g. for the [`PartitionedNetwork`] impl that flips back to
/// `Deliver` after a deadline). Today's public `Clock` trait has
/// only `now()` and `sleep()`; we expose both via a small helper
/// on the trait below — see `clock()` for now.
pub trait FaultScheduler: Send + Sync + 'static {
    /// Decide what to do with an outbound message on `(from, to)`.
    ///
    /// The `from` is the sender's own node id (so a scheduler
    /// can distinguish "n1 -> n2" from "n3 -> n2"). The `to` is
    /// the destination node id. The `body` is the message itself;
    /// some schedulers may want to inspect the message kind
    /// (e.g. "drop all heartbeats but deliver AppendEntries").
    ///
    /// `body` is passed by reference to avoid cloning. If a
    /// scheduler needs ownership it can clone the inner variant.
    fn before_send<'a>(
        &'a self,
        link: &'a LinkId,
        body: &'a InboundMessageBody,
    ) -> ScheduleFuture;

    /// Optional side-channel for schedulers that need to react
    /// to a message *after* it's been delivered (e.g. an
    /// adaptive scheduler that tracks message counts). Today
    /// nothing implements this; the default is a no-op so a
    /// future PR can add it without breaking existing
    /// implementors.
    fn after_deliver(&self, _link: &LinkId, _body: &InboundMessageBody) {}

    /// Test-only: list the directed links this scheduler is
    /// currently treating as partitioned. Used by the test
    /// harness to verify partition state after a sequence of
    /// events. The default is a no-op (most schedulers don't
    /// partition anything), so a future PR can add it without
    /// breaking existing implementors.
    #[allow(dead_code)]
    fn partitioned_links(&self) -> Vec<LinkId> {
        Vec::new()
    }
}

/// Test helper: a [FaultScheduler] that delivers when `cond` is
/// true and drops otherwise. Useful for asserting specific
/// scenarios without the noise of a full partition.
pub struct DropUnless<F: Fn(&LinkId, &InboundMessageBody) -> bool + Send + Sync + 'static> {
    cond: F,
}

impl<F: Fn(&LinkId, &InboundMessageBody) -> bool + Send + Sync + 'static> DropUnless<F> {
    pub fn new(cond: F) -> Self {
        Self { cond }
    }
}

impl<F: Fn(&LinkId, &InboundMessageBody) -> bool + Send + Sync + 'static> FaultScheduler
    for DropUnless<F>
{
    fn before_send<'a>(
        &'a self,
        link: &'a LinkId,
        body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        let keep = (self.cond)(link, body);
        Box::pin(async move {
            if keep {
                ScheduleOutcome::Deliver
            } else {
                ScheduleOutcome::Drop
            }
        })
    }
}

/// Always deliver. Passthrough. Used in tests that want the
/// scheduling surface (so they can be promoted to faulted
/// scenarios later) but no fault injection today.
pub struct AlwaysDeliver;

impl FaultScheduler for AlwaysDeliver {
    fn before_send<'a>(
        &'a self,
        _link: &'a LinkId,
        _body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        Box::pin(async { ScheduleOutcome::Deliver })
    }
}

/// Drop a single (directed) link. Messages sent from `from` to
/// `to` are silently discarded. The reverse direction is
/// untouched. Useful for testing "leader -> one follower"
/// asymmetry (e.g. follower falls behind, leader gets quorum from
/// the other two).
pub struct DropLink {
    pub from: String,
    pub to: String,
}

impl FaultScheduler for DropLink {
    fn before_send<'a>(
        &'a self,
        link: &'a LinkId,
        _body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        let matches = link.from == self.from && link.to == self.to;
        Box::pin(async move {
            if matches {
                ScheduleOutcome::Drop
            } else {
                ScheduleOutcome::Deliver
            }
        })
    }
}

/// Drop a message with a fixed probability. The harness passes
/// a closure that draws a `f64 ∈ [0.0, 1.0)`. `rng` is invoked
/// once per outbound message.
///
/// `R: FnMut() -> f64 + Send + 'static` — the closure may be a
/// closure over a seeded RNG (e.g. ChaCha20) for deterministic
/// tests. Each `before_send` call consumes one sample from the
/// RNG.
///
/// The threshold (when to drop vs deliver) is hard-coded at 0.5:
/// the harness that wants a different probability should bias
/// the RNG's output. We don't expose `p` as a field because the
/// only realistic use is "stress test with random drops" — and
/// for that, biasing the RNG is more flexible.
///
/// Implementation note: the RNG closure must be held inside a
/// `Mutex` so `before_send` (`&self`) can mutate it across
/// await-free synchronous sampling. We sample the RNG inside
/// `before_send` (before entering the async block) and move the
/// boolean decision into the future, so the RNG closure is
/// never held across an `.await`.
pub struct RandomDrop<R: FnMut() -> f64 + Send + 'static> {
    rng: Mutex<R>,
}

impl<R: FnMut() -> f64 + Send + 'static> RandomDrop<R> {
    pub fn new(rng: R) -> Self {
        Self {
            rng: Mutex::new(rng),
        }
    }
}

impl<R: FnMut() -> f64 + Send + 'static> FaultScheduler for RandomDrop<R> {
    fn before_send<'a>(
        &'a self,
        _link: &'a LinkId,
        _body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        // Sample inside the sync part (no await). Then move
        // the boolean decision into the future so the future
        // owns only a `bool` (which is Send).
        let drop = self.rng.lock().unwrap()() < 0.5;
        Box::pin(async move {
            if drop {
                ScheduleOutcome::Drop
            } else {
                ScheduleOutcome::Deliver
            }
        })
    }
}

/// A network partition: a set of `(from, to)` links that are
/// currently partitioned. Messages on partitioned links are
/// silently dropped until [`PartitionedNetwork::heal`] is
/// called.
///
/// This models the classic "network split" used in Raft
/// correctness tests: a minority partition can't elect a new
/// leader because its heartbeats don't reach the majority.
///
/// Auto-heal by virtual-time deadline is **not** modelled here
/// — it's hard to do correctly without an `epoch()` accessor on
/// the [`Clock`] trait, which is a bigger refactor than this PR
/// should carry. Tests that want a partition to flip back to
/// `Deliver` after N virtual ticks can simply call
/// [`PartitionedNetwork::heal`] from the harness after
/// `Clock::sleep(N)` completes.
pub struct PartitionedNetwork {
    /// The set of currently partitioned directed links.
    partitioned: Mutex<std::collections::HashSet<LinkId>>,
}

impl PartitionedNetwork {
    /// Create a new partition. The partition is permanent until
    /// [`PartitionedNetwork::heal`] is called.
    pub fn new() -> Self {
        Self {
            partitioned: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Add a directed link to the partition set. Future
    /// messages on this link are dropped until [`Self::heal`].
    pub fn partition(&self, link: LinkId) {
        self.partitioned.lock().unwrap().insert(link);
    }

    /// Clear the partition: every link is deliverable again.
    pub fn heal(&self) {
        self.partitioned.lock().unwrap().clear();
    }
}

impl Default for PartitionedNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultScheduler for PartitionedNetwork {
    fn before_send<'a>(
        &'a self,
        link: &'a LinkId,
        _body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        let is_partitioned = self.partitioned.lock().unwrap().contains(link);
        Box::pin(async move {
            if is_partitioned {
                ScheduleOutcome::Drop
            } else {
                ScheduleOutcome::Deliver
            }
        })
    }
}

/// Test scheduler: each outbound message is delivered with
/// probability `1 - p_delay` immediately, or delayed by `delay`
/// with probability `p_delay`. The delayed message still arrives
/// — the sender may time out before delivery, modelling a slow
/// link.
///
/// `R: FnMut() -> f64 + Send + 'static` is the random source. The
/// harness typically wraps a seeded ChaCha20 (or any deterministic
/// `f64` generator) so a test is replayable bit-for-bit.
///
/// Implementation note: the RNG closure must be held inside a
/// `Mutex` so `before_send` (`&self`) can mutate it. We sample
/// inside the sync part (no await), then move the boolean into
/// the future so the future only owns a `bool` (Send).
pub struct RandomDelay<R: FnMut() -> f64 + Send + 'static> {
    rng: Mutex<R>,
    p_delay: f64,
    delay: Duration,
}

impl<R: FnMut() -> f64 + Send + 'static> RandomDelay<R> {
    /// `p_delay ∈ [0.0, 1.0]`: probability that any given message
    /// is delayed. `delay`: how long delayed messages sleep
    /// before being pushed.
    pub fn new(rng: R, p_delay: f64, delay: Duration) -> Self {
        let p_delay = p_delay.clamp(0.0, 1.0);
        Self {
            rng: Mutex::new(rng),
            p_delay,
            delay,
        }
    }
}

impl<R: FnMut() -> f64 + Send + 'static> FaultScheduler for RandomDelay<R> {
    fn before_send<'a>(
        &'a self,
        _link: &'a LinkId,
        _body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        // Sample inside the sync part so the RNG closure is
        // released before any await.
        let sample = self.rng.lock().unwrap()();
        let delayed = sample < self.p_delay;
        let delay = self.delay;
        Box::pin(async move {
            if delayed {
                ScheduleOutcome::Delay(delay)
            } else {
                ScheduleOutcome::Deliver
            }
        })
    }
}

/// Test scheduler: every message is delivered AND duplicated
/// — i.e. the receiver's inbound channel sees the same message
/// body twice in a row.
///
/// This is a strong model for "packet duplication on a lossy
/// link" — useful for verifying that idempotent RPC handlers
/// (Raft consensus, which is itself idempotent for
/// AppendEntries and RequestVote) handle duplicates correctly.
///
/// Implementation: `before_send` returns `Duplicate(delay)`
/// where `delay` is the inter-duplicate spacing. The transport
/// honours the `Duplicate` outcome by pushing the message body
/// once immediately and once again after `delay`.
///
/// Today only `DuplicateAll` exists; a probabilistic variant
/// (`RandomDuplicate { p, delay }`) is straightforward to add
/// if tests need it.
pub struct DuplicateAll {
    delay: Duration,
}

impl DuplicateAll {
    /// `delay`: how long after the first delivery the duplicate
    /// is sent. Set to `Duration::ZERO` to send both
    /// simultaneously (the receiver may see them in either
    /// order — its inbound channel is an mpsc).
    pub fn new(delay: Duration) -> Self {
        Self { delay }
    }
}

impl Default for DuplicateAll {
    fn default() -> Self {
        Self::new(Duration::from_millis(50))
    }
}

impl FaultScheduler for DuplicateAll {
    fn before_send<'a>(
        &'a self,
        _link: &'a LinkId,
        _body: &'a InboundMessageBody,
    ) -> ScheduleFuture {
        let delay = self.delay;
        Box::pin(async move { ScheduleOutcome::Duplicate(delay) })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::net::Transport;
    use crate::raft::net::TransportError;
    use crate::raft::rpc::{
        AppendEntriesArgs, InstallSnapshotArgs, RaftMessage, RequestVoteArgs,
    };
    use crate::raft::sim_transport::{InboundMessageBody, Network, SimTransport};
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    fn request_vote(term: u64) -> InboundMessageBody {
        InboundMessageBody::Raft(RaftMessage::RequestVote(RequestVoteArgs {
            term,
            candidate_id: "n1".into(),
            last_log_index: 0,
            last_log_term: 0,
        }))
    }

    fn append_entries(term: u64) -> InboundMessageBody {
        InboundMessageBody::Raft(RaftMessage::AppendEntries(AppendEntriesArgs {
            term,
            leader_id: "n1".into(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        }))
    }

    fn install_snapshot(term: u64) -> InboundMessageBody {
        InboundMessageBody::Raft(RaftMessage::InstallSnapshot(InstallSnapshotArgs {
            term,
            leader_id: "n1".into(),
            last_included_index: 0,
            last_included_term: 0,
            snapshot: crate::protocol::Snapshot {
                last_included_index: 0,
                last_included_term: 0,
                data: std::collections::HashMap::new(),
            },
        }))
    }

    #[tokio::test]
    async fn always_deliver_returns_deliver_outcome() {
        let link = LinkId::new("n1", "n2");
        let outcome = AlwaysDeliver.before_send(&link, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));
    }

    #[tokio::test]
    async fn drop_link_drops_only_directed_link() {
        let drop = DropLink {
            from: "n1".to_string(),
            to: "n2".to_string(),
        };
        let fwd = LinkId::new("n1", "n2");
        let back = LinkId::new("n2", "n1");
        let to_n3 = LinkId::new("n1", "n3");

        // Forward direction is dropped.
        let outcome = drop.before_send(&fwd, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Drop));

        // Reverse direction is delivered.
        let outcome = drop.before_send(&back, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));

        // Unrelated link is delivered.
        let outcome = drop.before_send(&to_n3, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));
    }

    #[tokio::test]
    async fn random_drop_is_seeded_deterministic() {
        // Sample 0.3 < 0.5 -> Drop. Sample 0.7 >= 0.5 ->
        // Deliver. Both samples are independent (the impl
        // samples on every before_send call).
        let drop = RandomDrop::new(|| 0.3);
        let link = LinkId::new("n1", "n2");
        let outcome = drop.before_send(&link, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Drop));

        let drop = RandomDrop::new(|| 0.7);
        let outcome = drop.before_send(&link, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));
    }

    #[tokio::test]
    async fn partitioned_network_drops_directed_links_until_heal() {
        let part = PartitionedNetwork::new();
        let n1_to_n2 = LinkId::new("n1", "n2");
        let n1_to_n3 = LinkId::new("n1", "n3");

        // No partitions yet -> everything delivered.
        let outcome = part.before_send(&n1_to_n2, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));

        // Partition n1 -> n2.
        part.partition(n1_to_n2.clone());
        let outcome = part.before_send(&n1_to_n2, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Drop));
        // n1 -> n3 is unaffected.
        let outcome = part.before_send(&n1_to_n3, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));

        // Heal.
        part.heal();
        let outcome = part.before_send(&n1_to_n2, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));
    }

    #[tokio::test]
    async fn drop_unless_filters_by_predicate() {
        let drop_unless = DropUnless::new(|link, body| {
            // Keep = deliver. Drop everything from n1 -> n2
            // EXCEPT AppendEntries (the keep-result must be true
            // for AppendEntries, false otherwise).
            link.from == "n1"
                && link.to == "n2"
                && matches!(
                    body,
                    InboundMessageBody::Raft(RaftMessage::AppendEntries(_))
                )
        });
        let link_n1_n2 = LinkId::new("n1", "n2");
        let link_n1_n3 = LinkId::new("n1", "n3");

        // n1 -> n2 / RequestVote: keep=false -> drop.
        let outcome = drop_unless
            .before_send(&link_n1_n2, &request_vote(1))
            .await;
        assert!(matches!(outcome, ScheduleOutcome::Drop));

        // n1 -> n2 / AppendEntries: keep=true -> deliver.
        let outcome = drop_unless
            .before_send(&link_n1_n2, &append_entries(1))
            .await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));

        // n1 -> n3 / InstallSnapshot: keep=false (pred doesn't match) -> drop.
        let outcome = drop_unless
            .before_send(&link_n1_n3, &install_snapshot(1))
            .await;
        assert!(matches!(outcome, ScheduleOutcome::Drop));
    }

    /// End-to-end: a SimTransport wired with a DropLink
    /// scheduler sees `TransportError::Timeout` for send_raft
    /// on the dropped link. The Timeout duration matches the
    /// configured per-RPC timeout.
    #[tokio::test]
    async fn sim_transport_with_drop_link_returns_timeout() {
        let scheduler = Arc::new(DropLink {
            from: "n1".to_string(),
            to: "n2".to_string(),
        });
        let net = Network::with_scheduler(Duration::from_millis(200), scheduler);
        let _rx_n1 = net.register("n1");
        let _rx_n2 = net.register("n2");
        let t_n1 = SimTransport::new("n1".into(), net.clone(), _rx_n1);

        let start = tokio::time::Instant::now();
        let result = t_n1
            .send_raft(
                "n2",
                RaftMessage::RequestVote(RequestVoteArgs {
                    term: 1,
                    candidate_id: "n1".into(),
                    last_log_index: 0,
                    last_log_term: 0,
                }),
            )
            .await;
        let elapsed = start.elapsed();
        match result {
            Err(TransportError::Timeout(d)) => assert_eq!(d, Duration::from_millis(200)),
            other => panic!("expected Timeout, got {:?}", other),
        }
        // The Drop outcome sleeps the full rpc_timeout before
        // returning Timeout, so wall-clock elapsed should be
        // ~rpc_timeout (no virtual-time machinery in the
        // current Drop impl).
        assert!(
            elapsed >= Duration::from_millis(200),
            "expected wall-clock to wait the rpc_timeout, but elapsed = {:?}",
            elapsed
        );
    }

    /// End-to-end: a SimTransport wired with a partition sees
    /// send_raft return Timeout while partitioned, then Ok
    /// after heal.
    #[tokio::test]
    async fn sim_transport_with_partition_heals_on_demand() {
        use crate::raft::clock::SystemClock;
        use crate::raft::net::StopSignal;
        use crate::raft::node::RaftNode;
        use tempfile::tempdir;

        let part = Arc::new(PartitionedNetwork::new());
        let net = Network::with_scheduler(Duration::from_millis(500), part.clone());
        let rx_n1 = net.register("n1");
        let rx_n2 = net.register("n2");
        let t_n1 = SimTransport::new("n1".into(), net.clone(), rx_n1);

        // Build a fresh RaftNode for n2 to consume inbound.
        let tmp = tempdir().unwrap();
        let sm_config = crate::state_machine::StateMachineConfig {
            data_dir: tmp.path().join("sm"),
            memtable_size_threshold: 4 * 1024 * 1024,
        };
        let sm = Arc::new(RwLock::new(
            crate::state_machine::StateMachine::open(sm_config).unwrap(),
        ));
        let storage = crate::raft::storage::RaftStorage::new_with_paths(
            tmp.path().join("wal").to_string_lossy().to_string(),
            tmp.path().join("meta").to_string_lossy().to_string(),
            tmp.path().join("snap").to_string_lossy().to_string(),
        );
        let node_n2: Arc<RwLock<RaftNode>> = Arc::new(RwLock::new(
            RaftNode::new_with_clock(
                "n2".to_string(),
                vec!["n1".to_string()],
                sm,
                storage,
                Arc::new(SystemClock),
            ),
        ));
        let stop = StopSignal::new();
        let t_n2 = SimTransport::new("n2".into(), net.clone(), rx_n2);
        let serve_handle = {
            let node = node_n2.clone();
            let stop = stop.clone();
            tokio::spawn(async move { t_n2.serve(node, stop).await })
        };

        // Partition n1 -> n2.
        part.partition(LinkId::new("n1", "n2"));

        // n1 -> n2 send_raft should Timeout (no reply arrives).
        let result = t_n1
            .send_raft(
                "n2",
                RaftMessage::RequestVote(RequestVoteArgs {
                    term: 1,
                    candidate_id: "n1".into(),
                    last_log_index: 0,
                    last_log_term: 0,
                }),
            )
            .await;
        assert!(
            matches!(result, Err(TransportError::Timeout(_))),
            "expected Timeout while partitioned, got {:?}",
            result
        );

        // Heal the partition.
        part.heal();

        // Now the next send_raft should succeed (the receiver
        // is still serving).
        let result = t_n1
            .send_raft(
                "n2",
                RaftMessage::RequestVote(RequestVoteArgs {
                    term: 1,
                    candidate_id: "n1".into(),
                    last_log_index: 0,
                    last_log_term: 0,
                }),
            )
            .await;
        match result {
            Ok(RaftMessage::VoteResponse(reply)) => {
                // Empty-log follower grants vote to empty-log
                // candidate (both up-to-date, §5.4.1).
                assert!(reply.vote_granted);
            }
            other => panic!("expected VoteResponse Ok, got {:?}", other),
        }

        stop.stop();
        let _ = tokio::time::timeout(Duration::from_secs(2), serve_handle).await;
    }

    /// Unit test: RandomDelay with p=1.0 always delays; p=0.0
    /// always delivers. Boundary inclusive.
    #[tokio::test]
    async fn random_delay_with_p1_always_delays() {
        let sched = RandomDelay::new(|| 0.5, 1.0, Duration::from_millis(10));
        let link = LinkId::new("n1", "n2");
        let outcome = sched.before_send(&link, &request_vote(1)).await;
        match outcome {
            ScheduleOutcome::Delay(d) => assert_eq!(d, Duration::from_millis(10)),
            other => panic!("expected Delay, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn random_delay_with_p0_always_delivers() {
        let sched = RandomDelay::new(|| 0.5, 0.0, Duration::from_millis(10));
        let link = LinkId::new("n1", "n2");
        let outcome = sched.before_send(&link, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));
    }

    #[tokio::test]
    async fn random_delay_p_clamps_to_unit_interval() {
        // p=2.0 should be treated as 1.0 (always delay).
        let sched = RandomDelay::new(|| 0.5, 2.0, Duration::from_millis(10));
        let link = LinkId::new("n1", "n2");
        let outcome = sched.before_send(&link, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Delay(_)));
        // p=-0.5 should be treated as 0.0 (always deliver).
        let sched = RandomDelay::new(|| 0.5, -0.5, Duration::from_millis(10));
        let outcome = sched.before_send(&link, &request_vote(1)).await;
        assert!(matches!(outcome, ScheduleOutcome::Deliver));
    }

    /// Unit test: DuplicateAll returns a Duplicate outcome with
    /// the configured spacing.
    #[tokio::test]
    async fn duplicate_all_returns_duplicate_outcome() {
        let sched = DuplicateAll::new(Duration::from_millis(7));
        let link = LinkId::new("n1", "n2");
        let outcome = sched.before_send(&link, &request_vote(1)).await;
        match outcome {
            ScheduleOutcome::Duplicate(d) => assert_eq!(d, Duration::from_millis(7)),
            other => panic!("expected Duplicate, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn duplicate_all_default_spacing_is_50ms() {
        let sched = DuplicateAll::default();
        let link = LinkId::new("n1", "n2");
        let outcome = sched.before_send(&link, &request_vote(1)).await;
        match outcome {
            ScheduleOutcome::Duplicate(d) => {
                assert_eq!(d, Duration::from_millis(50))
            }
            other => panic!("expected Duplicate, got {:?}", other),
        }
    }
}
