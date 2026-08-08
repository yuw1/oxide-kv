//! Simulated transport for deterministic simulation testing (P7).
//!
//! In-memory message-passing impl of the [`Transport`] trait. Used
//! in place of [`TcpTransport`] by the future DST harness so the
//! consensus hot path runs against virtual endpoints — no real
//! sockets, no OS scheduling jitter, fully reproducible.
//!
//! ## Architecture
//!
//! A cluster shares a single [`Network`] — an `Arc<NetworkInner>`
//! holding a `HashMap<node_id, mpsc::Sender<InboundMessage>>`.
//! Each node gets its own [`SimTransport`], which holds:
//!
//!   - `Arc<Network>` — to look up peers' inbound channels and send
//!     them a message.
//!   - A `mpsc::Receiver<InboundMessage>` — the receiver end of its
//!     own inbound channel. `serve` consumes from it.
//!
//! `send_raft(to, msg)` looks up `to`'s inbound channel in the
//! shared `Network`, pushes an `InboundMessage { msg, reply: Some(tx) }`
//! in. The receiver side (`serve`) reads from its own `inbound`
//! mpsc, calls the corresponding `RaftNode::handle_*` handler
//! synchronously, and returns the reply through the oneshot.
//! The whole round-trip is one message through one mpsc slot — no
//! framing, no protobuf encoding, no TCP.
//!
//! ## Failure surface
//!
//! This first cut is **no-fault**: messages always arrive, never
//! drop, never delay. A follow-up PR will add a `FaultScheduler`
//! trait (delay / drop / partition) so the DST harness can drive
//! realistic failure scenarios. The [`TransportError`] variants
//! stay in scope (Unreachable for unknown peer id, Protocol for
//! unknown message variant) but the timeout / drop paths are
//! dormant until the fault scheduler lands.
//!
//! ## Why a fresh `SimTransport` is `Send + Sync + 'static`
//!
//! Each method returns a boxed future that's pinned for the
//! lifetime of `&self`. The trait stays object-safe (same trick
//! as `TcpTransport`).

use crate::coordination::{VoteRequest, VoteResponse};
use crate::raft::fault_scheduler::{FaultScheduler, LinkId, ScheduleOutcome};
use crate::raft::node::RaftNode;
use crate::raft::rpc::{RaftMessage, VoteResponseArgs};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::raft::net::futures;
use crate::raft::net::{StopSignal, Transport, TransportError};

/// One-shot reply channel sender type. The `Result` carries the
/// reply back to the requester or a `TransportError` if the
/// receiver side couldn't dispatch. The reply body matches the
/// inbound body type: a Raft request gets a Raft reply, a Vote
/// request gets a Vote reply. `InboundMessageBody` is the
/// discriminated wrapper that lets one channel carry both kinds.
pub(crate) type ReplySender = oneshot::Sender<Result<InboundMessageBody, TransportError>>;

/// The body of an inbound message. Wraps either a Raft consensus
/// RPC (`RaftMessage`) or a 2PC coordinator vote (`VoteRequest`).
/// These have to be distinct dispatch paths because they target
/// different `RaftNode` handlers (`handle_*` for consensus,
/// `handle_tx_vote_request` for 2PC), even though their wire-level
/// fields overlap. Routing them through one enum keeps the
/// `InboundMessage` struct flat (no two parallel channel types).
///
/// `Vote` carries the request and `VoteReply` carries the
/// response, so the inbound / outbound sides can't be confused at
/// the type level. The `Raft` variant carries both request and
/// reply because `RaftMessage` already discriminates the two via
/// its own enum.
#[derive(Debug, Clone)]
pub enum InboundMessageBody {
    /// A Raft consensus RPC. Reply is the matching `RaftMessage`
    /// reply variant (`AppendReply`, `VoteResponse`, etc.).
    Raft(RaftMessage),
    /// A 2PC coordinator vote request from the leader.
    Vote(VoteRequest),
    /// The reply to a 2PC coordinator vote request.
    VoteReply(VoteResponse),
}

/// A message addressed to a specific node. `reply` is `Some` for
/// RPCs that expect a response (every `send_raft` call today),
/// `None` for fire-and-forget (a future InstallSnapshot chunk
/// stream might use this).
///
/// Marked `pub` (not `pub(crate)`) because `Network::register`
/// and `SimTransport::new` are public APIs and Rust requires the
/// parameter / return types of `pub fn`s to be at least as
/// visible as the fn itself. The fields are `pub(crate)` so
/// external callers can construct / destructure InboundMessages
/// only via the helpers exposed by this module.
pub struct InboundMessage {
    /// Source node id. Reserved for the future fault scheduler
    /// (which link to drop, which links to delay). Not read by
    /// the current no-fault `serve` loop, but kept on the struct
    /// so the wire shape stays stable for the next PR.
    #[allow(dead_code)]
    pub(crate) from: String,
    pub(crate) body: InboundMessageBody,
    pub(crate) reply: Option<ReplySender>,
}

/// Per-node inbound channel capacity. Small enough that a runaway
/// producer back-pressures rather than OOM-ing the test; large
/// enough that a bursty leader doesn't immediately stall.
const INBOUND_CAPACITY: usize = 256;

/// Shared cluster topology. Holds the inbound senders for every
/// node in the simulated cluster so `SimTransport::send_raft` can
/// route messages to the right receiver.
#[derive(Clone)]
pub struct Network {
    inner: Arc<NetworkInner>,
}

struct NetworkInner {
    /// Node id -> sender for its inbound channel.
    peers: Mutex<HashMap<String, mpsc::Sender<InboundMessage>>>,
    /// Per-RPC virtual timeout. Mirrors `TcpTransport`'s use of
    /// `tokio::time::timeout` for the wall-clock case. The future
    /// fault scheduler will use this to decide when a message
    /// counts as dropped; for now it's the upper bound on how long
    /// `send_raft` will wait for a reply.
    rpc_timeout: Duration,
    /// The shared fault scheduler. Every outbound message
    /// consults this before being pushed into the receiver's
    /// inbound channel. Defaults to [`AlwaysDeliver`] for
    /// zero-config tests.
    scheduler: Arc<dyn FaultScheduler>,
}

impl Network {
    /// Create a fresh empty network with [`AlwaysDeliver`]
    /// scheduling and a 2-second per-RPC timeout.
    pub fn new() -> Self {
        Self::with_rpc_timeout(Duration::from_secs(2))
    }

    /// Create a fresh network with a custom per-RPC timeout. Tests
    /// driving virtual time can shrink this to milliseconds so a
    /// slow peer returns `TransportError::Timeout` quickly.
    pub fn with_rpc_timeout(rpc_timeout: Duration) -> Self {
        use crate::raft::fault_scheduler::AlwaysDeliver;
        Self {
            inner: Arc::new(NetworkInner {
                peers: Mutex::new(HashMap::new()),
                rpc_timeout,
                scheduler: Arc::new(AlwaysDeliver),
            }),
        }
    }

    /// Build a network with a custom fault scheduler. The
    /// scheduler is shared by every `SimTransport` registered on
    /// this network, so a single `partition(...)` call from the
    /// harness affects every node's outbound path.
    pub fn with_scheduler(rpc_timeout: Duration, scheduler: Arc<dyn FaultScheduler>) -> Self {
        Self {
            inner: Arc::new(NetworkInner {
                peers: Mutex::new(HashMap::new()),
                rpc_timeout,
                scheduler,
            }),
        }
    }

    /// Register a node id and return the inbound receiver half. The
    /// caller (typically the harness) wraps the receiver in a
    /// `SimTransport` via `SimTransport::new`.
    pub fn register(&self, node_id: &str) -> mpsc::Receiver<InboundMessage> {
        let (tx, rx) = mpsc::channel(INBOUND_CAPACITY);
        self.inner
            .peers
            .lock()
            .unwrap()
            .insert(node_id.to_string(), tx);
        rx
    }

    /// Look up a peer's inbound sender. Returns `None` if the peer
    /// is not registered (the equivalent of TCP connection
    /// refused).
    fn lookup(&self, to: &str) -> Option<mpsc::Sender<InboundMessage>> {
        self.inner.peers.lock().unwrap().get(to).cloned()
    }

    /// Consult the shared fault scheduler on an outbound message.
    /// Returns the outcome the caller should apply.
    async fn consult_scheduler(&self, link: &LinkId, body: &InboundMessageBody) -> ScheduleOutcome {
        self.inner.scheduler.before_send(link, body).await
    }

    /// The per-RPC timeout for this network.
    pub fn rpc_timeout(&self) -> Duration {
        self.inner.rpc_timeout
    }
}

impl Default for Network {
    fn default() -> Self {
        Self::new()
    }
}

impl Network {
    /// Re-register an existing node id, dropping the
    /// (possibly dead) sender and installing a fresh one.
    /// Returns the inbound receiver half so a SimTransport
    /// can install it via `SimTransport::replace_inbound`.
    ///
    /// Used by DST scenarios that "kill and restart" a node —
    /// after kill, the original sender is a dead handle
    /// because the previous receiver was dropped when the
    /// serve loop exited.
    pub fn re_register(&self, node_id: &str) -> mpsc::Receiver<InboundMessage> {
        let (tx, rx) = mpsc::channel(INBOUND_CAPACITY);
        self.inner
            .peers
            .lock()
            .unwrap()
            .insert(node_id.to_string(), tx);
        rx
    }
}

/// In-memory transport. Holds a clone of the shared [`Network`] and
/// its own inbound receiver. `send_raft` looks up the target in the
/// network and pushes. `serve` consumes from the inbound receiver
/// and dispatches each message to the matching `RaftNode` handler.
///
/// `Clone` is implemented by bumping the `Arc` on `inbound`. The
/// `Mutex<Option<Receiver>>` becomes `Arc<Mutex<Option<Receiver>>>`
/// so multiple handles to the same transport can each take the
/// receiver out — only the first `serve` wins; subsequent
/// `serve`s see `None` and panic.
#[derive(Clone)]
pub struct SimTransport {
    #[allow(dead_code)]
    self_id: String,
    /// The shared cluster network. Exposed so DST scenarios
    /// that need to re-register an inbound channel (e.g. after
    /// `kill_node` + `restart_node`) can do so via
    /// [`Network::re_register`].
    pub network: Network,
    inbound: Arc<Mutex<Option<mpsc::Receiver<InboundMessage>>>>,
}

impl SimTransport {
    /// Construct a `SimTransport` for a given node id, using the
    /// inbound receiver returned by [`Network::register`]. The
    /// receiver is moved into `self.inbound`; `serve` takes it back
    /// out.
    pub fn new(self_id: String, network: Network, inbound: mpsc::Receiver<InboundMessage>) -> Self {
        Self {
            self_id,
            network,
            inbound: Arc::new(Mutex::new(Some(inbound))),
        }
    }

    /// Replace the inbound receiver with a fresh one. Used by
    /// DST restart_node: after the previous serve loop has
    /// dropped the receiver (during kill_node), we install a
    /// fresh receiver here so the next `serve` call has
    /// something to read from.
    ///
    /// Panics if a receiver is still installed (the previous
    /// serve loop is still running). Callers should
    /// `stop.stop()` + sleep before this.
    pub fn replace_inbound(&self, new_inbound: mpsc::Receiver<InboundMessage>) {
        let mut guard = self.inbound.lock().unwrap();
        assert!(
            guard.is_none(),
            "SimTransport::replace_inbound called while a receiver is still installed"
        );
        *guard = Some(new_inbound);
    }
}

/// Dispatch a decoded `RaftMessage` to the matching `RaftNode`
/// handler. Returns the reply as the matching `RaftMessage`
/// variant. Sync because all underlying RaftNode handlers are sync.
pub(crate) fn dispatch_raft_message(
    node: &mut RaftNode,
    msg: RaftMessage,
) -> Result<RaftMessage, TransportError> {
    match msg {
        RaftMessage::RequestVote(args) => {
            let reply = node.handle_request_vote(&args);
            Ok(RaftMessage::VoteResponse(VoteResponseArgs {
                term: reply.term,
                vote_granted: reply.vote_granted,
            }))
        }
        // Pre-vote probe (P8 PR 5, Raft §9.6). Routed to the
        // dedicated `handle_pre_vote` handler so the simulation
        // harness exercises the same probe-only path as production.
        RaftMessage::RequestPreVote(args) => {
            let reply = node.handle_pre_vote(&args);
            Ok(RaftMessage::PreVoteResponse(VoteResponseArgs {
                term: reply.term,
                vote_granted: reply.vote_granted,
            }))
        }
        RaftMessage::AppendEntries(args) => {
            let reply = node.handle_append_entries(&args);
            Ok(RaftMessage::AppendReply(reply))
        }
        RaftMessage::InstallSnapshot(args) => {
            let reply = node.handle_install_snapshot(&args);
            Ok(RaftMessage::InstallSnapshotReply(reply))
        }
        // Reply variants inbound are a programming error (peers
        // shouldn't echo replies). Surface as Protocol.
        other => Err(TransportError::Protocol(format!(
            "unexpected inbound reply variant: {:?}",
            std::mem::discriminant(&other)
        ))),
    }
}

/// Build an `InboundMessage` for an outbound Raft RPC. Centralizes
/// the oneshot-channel plumbing so `send_raft` and `send_vote`
/// don't duplicate it.
async fn push_with_reply(
    sender: &mpsc::Sender<InboundMessage>,
    from: String,
    body: InboundMessageBody,
) -> Result<InboundMessageBody, TransportError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let inbound = InboundMessage {
        from,
        body,
        reply: Some(reply_tx),
    };
    sender
        .send(inbound)
        .await
        .map_err(|_| TransportError::Unreachable("peer inbound channel closed".to_string()))?;
    match reply_rx.await {
        Ok(result) => result,
        Err(_) => Err(TransportError::Protocol(
            "reply oneshot canceled before delivery".to_string(),
        )),
    }
}

impl Transport for SimTransport {
    fn send_raft<'a>(&'a self, to: &'a str, msg: RaftMessage) -> futures::SendRaftFuture<'a> {
        let to_owned = to.to_string();
        let self_id = self.self_id.clone();
        let network = self.network.clone();
        let rpc_timeout = network.rpc_timeout();
        let fut = Box::pin(async move {
            // Unknown peer => Unreachable (matches TCP connection
            // refused behavior).
            let sender = network.lookup(&to_owned).ok_or_else(|| {
                TransportError::Unreachable(format!("peer {} not registered", to_owned))
            })?;
            // Consult the fault scheduler on the link from
            // `self_id` to `to_owned`. A `Drop` outcome makes
            // the sender see a Timeout (no reply ever arrives);
            // a `Delay` is treated as a delivery in this PR —
            // proper Delay handling is the next PR's job.
            let link = LinkId::new(self_id.clone(), to_owned.clone());
            let body = InboundMessageBody::Raft(msg);
            match network.consult_scheduler(&link, &body).await {
                ScheduleOutcome::Drop => {
                    // Sender waits the full rpc_timeout then
                    // surfaces Timeout. We sleep here so the
                    // semantics match the wall-clock case.
                    tokio::time::sleep(rpc_timeout).await;
                    return Err(TransportError::Timeout(rpc_timeout));
                }
                ScheduleOutcome::Delay(delay) => {
                    // Real Delay impl: sleep for `delay` of
                    // wall-clock time before pushing. If the
                    // sender has a shorter `rpc_timeout`, the
                    // surrounding `tokio::time::timeout` below
                    // will fire and the message will arrive at
                    // the receiver *after* the sender has given
                    // up — modelling the classic "slow link"
                    // scenario in Raft. (Virtual-clock alignment
                    // is deferred to a future PR; today the
                    // delay is wall-clock so a Delay > rpc_timeout
                    // matches the documented semantics.)
                    tokio::time::sleep(delay).await;
                }
                ScheduleOutcome::Duplicate(delay) => {
                    // Push the original now; push the duplicate
                    // after `delay`. We do the duplicate on a
                    // separate spawned task so the sender's
                    // RPC isn't held open by it. If the sender's
                    // rpc_timeout fires before the duplicate
                    // task gets to push, the duplicate still
                    // arrives (it doesn't gate on the sender).
                    let dup_sender = sender.clone();
                    let dup_from = self_id.clone();
                    let dup_body = body.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = push_with_reply(&dup_sender, dup_from, dup_body).await;
                    });
                }
                ScheduleOutcome::Deliver => {}
            }
            let push_fut = push_with_reply(&sender, self_id, body);
            let body = match tokio::time::timeout(rpc_timeout, push_fut).await {
                Ok(result) => result,
                Err(_) => return Err(TransportError::Timeout(rpc_timeout)),
            }?;
            match body {
                InboundMessageBody::Raft(reply) => Ok(reply),
                InboundMessageBody::Vote(_) => Err(TransportError::Protocol(
                    "raft peer returned a vote request instead of a raft reply".to_string(),
                )),
                InboundMessageBody::VoteReply(_) => Err(TransportError::Protocol(
                    "raft peer returned a vote reply instead of a raft reply".to_string(),
                )),
            }
        });
        futures::SendRaftFuture(fut)
    }

    fn send_vote<'a>(&'a self, to: &'a str, req: VoteRequest) -> futures::SendVoteFuture<'a> {
        let to_owned = to.to_string();
        let self_id = self.self_id.clone();
        let network = self.network.clone();
        let rpc_timeout = network.rpc_timeout();
        let fut = Box::pin(async move {
            let sender = network.lookup(&to_owned).ok_or_else(|| {
                TransportError::Unreachable(format!("peer {} not registered", to_owned))
            })?;
            let link = LinkId::new(self_id.clone(), to_owned.clone());
            let body = InboundMessageBody::Vote(req);
            match network.consult_scheduler(&link, &body).await {
                ScheduleOutcome::Drop => {
                    tokio::time::sleep(rpc_timeout).await;
                    return Err(TransportError::Timeout(rpc_timeout));
                }
                ScheduleOutcome::Delay(delay) => {
                    // Real Delay impl: sleep `delay` before
                    // pushing. See send_raft's matching branch
                    // for the semantics.
                    tokio::time::sleep(delay).await;
                }
                ScheduleOutcome::Duplicate(delay) => {
                    // See send_raft's matching branch. Vote
                    // duplication is unusual but the same
                    // mechanism applies.
                    let dup_sender = sender.clone();
                    let dup_from = self_id.clone();
                    let dup_body = body.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = push_with_reply(&dup_sender, dup_from, dup_body).await;
                    });
                }
                ScheduleOutcome::Deliver => {}
            }
            let push_fut = push_with_reply(&sender, self_id, body);
            let body = match tokio::time::timeout(rpc_timeout, push_fut).await {
                Ok(result) => result,
                Err(_) => return Err(TransportError::Timeout(rpc_timeout)),
            }?;
            match body {
                InboundMessageBody::VoteReply(reply) => Ok(reply),
                InboundMessageBody::Vote(_) => Err(TransportError::Protocol(
                    "vote peer returned a vote request instead of a vote reply".to_string(),
                )),
                InboundMessageBody::Raft(other) => Err(TransportError::Protocol(format!(
                    "vote peer returned non-vote reply: {:?}",
                    std::mem::discriminant(&other)
                ))),
            }
        });
        futures::SendVoteFuture(fut)
    }

    fn serve<'a>(
        &'a self,
        raft_node: Arc<RwLock<RaftNode>>,
        stop: StopSignal,
    ) -> futures::ServeFuture<'a> {
        // Take the inbound receiver out of the Mutex<Option<>>;
        // `serve` can only be called once per SimTransport.
        let inbound = self
            .inbound
            .lock()
            .unwrap()
            .take()
            .expect("SimTransport::serve called more than once");
        let fut = Box::pin(async move {
            let mut inbound = inbound;
            loop {
                tokio::select! {
                    biased;
                    _ = stop.0.notified() => {
                        return Err(TransportError::Canceled);
                    }
                    maybe = inbound.recv() => {
                        match maybe {
                            Some(InboundMessage { body, reply, .. }) => {
                                let result = {
                                    let mut node = raft_node.write().unwrap();
                                    match body {
                                        InboundMessageBody::Raft(msg) => {
                                            // Raft consensus dispatch.
                                            dispatch_raft_message(&mut node, msg)
                                                .map(InboundMessageBody::Raft)
                                        }
                                        InboundMessageBody::Vote(req) => {
                                            // 2PC coordinator vote dispatch.
                                            let reply_msg = node.handle_tx_vote_request(&req);
                                            Ok(InboundMessageBody::VoteReply(reply_msg))
                                        }
                                        InboundMessageBody::VoteReply(_) => {
                                            // A peer echoed back a vote
                                            // reply instead of a vote
                                            // request — protocol error.
                                            Err(TransportError::Protocol(
                                                "received VoteReply without a preceding Vote request"
                                                    .to_string(),
                                            ))
                                        }
                                    }
                                };
                                // If the requester dropped the
                                // oneshot (e.g. timed out before
                                // we got here), ignore the send
                                // error.
                                if let Some(tx) = reply {
                                    let _ = tx.send(result);
                                }
                                // No reply means a fire-and-forget
                                // inbound — nothing to send back.
                                // Currently nothing produces these.
                            }
                            None => {
                                // All senders dropped — cluster
                                // has shut down. Exit cleanly.
                                return Ok(());
                            }
                        }
                    }
                }
            }
        });
        futures::ServeFuture(fut)
    }
}

// Compile-time check: SimTransport and Network must be Send + Sync
// + 'static so they can live behind `Arc<dyn Transport>` or be
// shared across test tasks.
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<SimTransport>();
    assert_send_sync_static::<Network>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::clock::SystemClock;
    use crate::raft::rpc::{
        AppendEntriesArgs, AppendReplyArgs, InstallSnapshotArgs, RequestVoteArgs,
    };

    /// Helper: build a fresh `RaftNode` for tests. Mirrors the
    /// `new_with_storage` constructor used by the integration test
    /// harness so a SimTransport test doesn't accidentally exercise
    /// disk paths. Uses a private data dir under `tempfile`.
    fn fresh_node(node_id: &str, peers: Vec<String>) -> Arc<RwLock<RaftNode>> {
        let tmp = tempfile::tempdir().unwrap();

        let sm_config = crate::state_machine::StateMachineConfig {
            data_dir: tmp.path().join("sm"),
            memtable_size_threshold: 4 * 1024 * 1024,
        };
        let sm = std::sync::Arc::new(std::sync::RwLock::new(
            crate::state_machine::StateMachine::open(sm_config).unwrap(),
        ));
        let storage = crate::raft::storage::RaftStorage::new_with_paths(
            tmp.path().join("wal").to_string_lossy().to_string(),
            tmp.path().join("meta").to_string_lossy().to_string(),
            tmp.path().join("snap").to_string_lossy().to_string(),
        );
        Arc::new(RwLock::new(RaftNode::new_with_clock(
            node_id.to_string(),
            peers,
            sm,
            storage,
            Arc::new(SystemClock),
        )))
    }

    #[tokio::test]
    async fn sim_transport_send_raft_unknown_peer_is_unreachable() {
        let net = Network::new();
        let rx = net.register("n1");
        let t = SimTransport::new("n1".into(), net, rx);
        let result = t
            .send_raft(
                "n-does-not-exist",
                RaftMessage::RequestVote(RequestVoteArgs {
                    term: 1,
                    candidate_id: "n1".into(),
                    last_log_index: 0,
                    last_log_term: 0,
                }),
            )
            .await;
        assert!(
            matches!(result, Err(TransportError::Unreachable(_))),
            "expected Unreachable, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn dispatch_raft_message_routes_request_vote_to_handler() {
        let node = fresh_node("n2", vec!["n1".to_string()]);
        let mut n = node.write().unwrap();
        let result = dispatch_raft_message(
            &mut n,
            RaftMessage::RequestVote(RequestVoteArgs {
                term: 1,
                candidate_id: "n1".into(),
                last_log_index: 0,
                last_log_term: 0,
            }),
        )
        .expect("dispatch should succeed for request vote");
        match result {
            RaftMessage::VoteResponse(reply) => {
                // Fresh follower with empty log + candidate with
                // matching empty log → vote is granted (both are
                // "up-to-date", §5.4.1 election restriction).
                assert!(
                    reply.vote_granted,
                    "empty-log follower should grant vote to empty-log candidate"
                );
                assert_eq!(reply.term, 1);
            }
            other => panic!(
                "expected VoteResponse, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn dispatch_raft_message_routes_append_entries_to_handler() {
        let node = fresh_node("n2", vec!["n1".to_string()]);
        let mut n = node.write().unwrap();
        let result = dispatch_raft_message(
            &mut n,
            RaftMessage::AppendEntries(AppendEntriesArgs {
                term: 1,
                leader_id: "n1".into(),
                prev_log_index: 0,
                prev_log_term: 0,
                entries: vec![],
                leader_commit: 0,
            }),
        )
        .expect("dispatch should succeed for append entries");
        match result {
            RaftMessage::AppendReply(reply) => {
                assert!(
                    reply.success,
                    "empty append with matching prev should succeed"
                );
                assert_eq!(reply.term, 1);
            }
            other => panic!(
                "expected AppendReply, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn dispatch_raft_message_routes_install_snapshot_to_handler() {
        let node = fresh_node("n2", vec!["n1".to_string()]);
        let mut n = node.write().unwrap();
        let result = dispatch_raft_message(
            &mut n,
            RaftMessage::InstallSnapshot(InstallSnapshotArgs {
                term: 1,
                leader_id: "n1".into(),
                last_included_index: 0,
                last_included_term: 0,
                snapshot: crate::protocol::Snapshot {
                    last_included_index: 0,
                    last_included_term: 0,
                    data: std::collections::HashMap::new(),
                },
            }),
        )
        .expect("dispatch should succeed for install snapshot");
        match result {
            RaftMessage::InstallSnapshotReply(reply) => {
                assert_eq!(reply.term, 1);
            }
            other => panic!(
                "expected InstallSnapshotReply, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn dispatch_raft_message_rejects_inbound_reply_variant() {
        let node = fresh_node("n2", vec!["n1".to_string()]);
        let mut n = node.write().unwrap();
        let result = dispatch_raft_message(
            &mut n,
            RaftMessage::AppendReply(AppendReplyArgs {
                term: 1,
                success: true,
            }),
        );
        assert!(
            matches!(result, Err(TransportError::Protocol(_))),
            "expected Protocol, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn sim_transport_serve_dispatches_through_node_handler() {
        // End-to-end: send RequestVote via t_n1.send_raft, run
        // t_n2.serve in the background, assert the reply.
        let net = Network::with_rpc_timeout(Duration::from_secs(2));
        let rx_n1 = net.register("n1");
        let rx_n2 = net.register("n2");
        let t_n1 = SimTransport::new("n1".into(), net.clone(), rx_n1);
        let t_n2 = SimTransport::new("n2".into(), net.clone(), rx_n2);
        let node_n2 = fresh_node("n2", vec!["n1".to_string()]);
        let stop = StopSignal::new();

        // Run t_n2's serve loop in the background.
        let serve_node = node_n2.clone();
        let serve_stop = stop.clone();
        let serve_handle = tokio::spawn(async move { t_n2.serve(serve_node, serve_stop).await });

        // Send a RequestVote from n1 to n2.
        let reply = t_n1
            .send_raft(
                "n2",
                RaftMessage::RequestVote(RequestVoteArgs {
                    term: 1,
                    candidate_id: "n1".into(),
                    last_log_index: 0,
                    last_log_term: 0,
                }),
            )
            .await
            .expect("send_raft should succeed for registered peer");

        match reply {
            RaftMessage::VoteResponse(v) => {
                // Fresh follower with empty log + candidate with
                // matching empty log → vote is granted (both are
                // "up-to-date", §5.4.1 election restriction).
                assert!(
                    v.vote_granted,
                    "empty-log follower should grant vote to empty-log candidate"
                );
                assert_eq!(v.term, 1);
            }
            other => panic!(
                "expected VoteResponse, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Clean up.
        stop.stop();
        let _ = tokio::time::timeout(Duration::from_secs(1), serve_handle).await;
    }

    #[tokio::test]
    async fn sim_transport_send_vote_returns_vote_response() {
        // End-to-end for the 2PC vote surface: send a VoteRequest
        // via t_n1.send_vote, run t_n2.serve, get a VoteResponse.
        let net = Network::with_rpc_timeout(Duration::from_secs(2));
        let rx_n1 = net.register("n1");
        let rx_n2 = net.register("n2");
        let t_n1 = SimTransport::new("n1".into(), net.clone(), rx_n1);
        let t_n2 = SimTransport::new("n2".into(), net.clone(), rx_n2);
        let node_n2 = fresh_node("n2", vec!["n1".to_string()]);
        let stop = StopSignal::new();

        let serve_node = node_n2.clone();
        let serve_stop = stop.clone();
        let serve_handle = tokio::spawn(async move { t_n2.serve(serve_node, serve_stop).await });

        let reply = t_n1
            .send_vote(
                "n2",
                VoteRequest {
                    term: 1,
                    tx_id: "tx-1".to_string(),
                    last_log_index: 0,
                    last_log_term: 0,
                },
            )
            .await
            .expect("send_vote should succeed for registered peer");
        assert_eq!(reply.term, 1);
        // Fresh follower's pending_txs is empty, so the 2PC
        // coordinator's "tx not pending" deny applies — SimTransport
        // surfaces vote_granted=false (no_reason is filled on the
        // TcpTransport path; here we keep the field empty).
        assert!(
            !reply.vote_granted,
            "vote should be denied for unknown tx_id, got granted"
        );

        stop.stop();
        let _ = tokio::time::timeout(Duration::from_secs(1), serve_handle).await;
    }

    #[tokio::test]
    async fn sim_transport_round_trip_append_entries_to_follower() {
        // Two-node in-memory cluster: n1 sends an empty
        // AppendEntries (heartbeat) to n2; n2's serve loop
        // dispatches to handle_append_entries and returns
        // AppendReply.
        let net = Network::with_rpc_timeout(Duration::from_secs(2));
        let rx_n1 = net.register("n1");
        let rx_n2 = net.register("n2");
        let t_n1 = SimTransport::new("n1".into(), net.clone(), rx_n1);
        let t_n2 = SimTransport::new("n2".into(), net.clone(), rx_n2);
        let node_n2 = fresh_node("n2", vec!["n1".to_string()]);
        let stop = StopSignal::new();

        let serve_handle = {
            let serve_node = node_n2.clone();
            let serve_stop = stop.clone();
            tokio::spawn(async move { t_n2.serve(serve_node, serve_stop).await })
        };

        // Heartbeat from n1.
        let reply = t_n1
            .send_raft(
                "n2",
                RaftMessage::AppendEntries(AppendEntriesArgs {
                    term: 1,
                    leader_id: "n1".into(),
                    prev_log_index: 0,
                    prev_log_term: 0,
                    entries: vec![],
                    leader_commit: 0,
                }),
            )
            .await
            .expect("heartbeat should succeed");
        match reply {
            RaftMessage::AppendReply(r) => {
                assert!(
                    r.success,
                    "empty heartbeat with matching prev should succeed"
                );
                assert_eq!(r.term, 1);
            }
            other => panic!(
                "expected AppendReply, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        stop.stop();
        let _ = tokio::time::timeout(Duration::from_secs(1), serve_handle).await;
    }
}
