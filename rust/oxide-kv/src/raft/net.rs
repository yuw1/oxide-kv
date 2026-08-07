//! Transport abstraction for the Raft + 2PC network surface.
//!
//! P7 foundation (second half): alongside
//! [`crate::raft::clock::Clock`], this trait lets a future
//! `SimTransport` swap real TCP for in-memory channels routed by a
//! fault scheduler — without touching the framing, encoding, or
//! consensus layers.
//!
//! ## Trait shape
//!
//! Message-level, not byte-level. The framing layer in
//! [`crate::raft::transport`] and the protobuf encoding in
//! [`crate::raft::proto`] / [`crate::coordination`] are exercised by
//! the existing test suite and don't need to be re-implemented for
//! simulation. The transport trait hands decoded messages between
//! endpoints; the [`TcpTransport`] impl wraps the existing TCP path;
//! a future `SimTransport` will hand messages directly to a virtual
//! scheduler.
//!
//! ## Failure surface
//!
//! All send methods return [`TransportError`]. The existing callers
//! (`RaftNode`'s heartbeat / AppendEntries / RequestVote paths, and
//! `coordinator::coordinate_tx`'s vote fan-out) all already handle
//! RPC failure / timeout / unreachable peer, so this is a clean swap.

use crate::coordination::VoteResponse;
use crate::raft::node::RaftNode;
use crate::raft::rpc::{RpcClient, RpcServer, RaftMessage};
use std::sync::{Arc, RwLock};
use tracing::warn;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// Errors that any `Transport` impl may surface. Mirrors the set of
/// failure modes the existing TCP path can produce:
///
/// - `Unreachable`: peer listener not accepting connections (TCP
///   `connect` refused, DNS failure).
/// - `Timeout`: the per-RPC timeout fired before a reply arrived.
/// - `Protocol`: a frame was malformed, a discriminator was unknown,
///   or the peer closed the connection mid-frame.
/// - `Canceled`: the listener was stopped mid-`serve` (used by the
///   simulation harness for clean teardown).
#[derive(Debug)]
pub enum TransportError {
    Unreachable(String),
    Timeout(Duration),
    Protocol(String),
    Canceled,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Unreachable(s) => write!(f, "peer unreachable: {}", s),
            TransportError::Timeout(d) => write!(f, "rpc timeout after {:?}", d),
            TransportError::Protocol(s) => write!(f, "protocol error: {}", s),
            TransportError::Canceled => write!(f, "transport canceled"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Signal passed to [`Transport::serve`] so the listener can be asked
/// to stop. Wraps a `tokio::sync::Notify`; the real impl awaits it
/// between accepts; a future `SimTransport` will do the same.
#[derive(Clone)]
pub struct StopSignal(pub Arc<Notify>);

impl StopSignal {
    pub fn new() -> Self {
        Self(Arc::new(Notify::new()))
    }

    pub fn stop(&self) {
        self.0.notify_waiters();
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Future types returned by [`Transport`] methods. Modeled as opaque
/// boxes so the trait stays object-safe (`async fn` in traits requires
/// either `async_trait` or boxing). The future produced by each call
/// resolves to `Result<_, TransportError>`.
///
/// This module-level pattern mirrors [`crate::raft::clock::futures`]:
/// cheap, zero-dependency, easy to migrate to `async_trait` later if
/// we want cleaner call-site syntax.
pub mod futures {
    use super::{RaftMessage, TransportError, VoteResponse};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct SendRaftFuture<'a>(
        pub Pin<Box<dyn Future<Output = Result<RaftMessage, TransportError>> + Send + 'a>>,
    );

    impl<'a> Future for SendRaftFuture<'a> {
        type Output = Result<RaftMessage, TransportError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.as_mut().poll(cx)
        }
    }

    pub struct SendVoteFuture<'a>(
        pub Pin<Box<dyn Future<Output = Result<VoteResponse, TransportError>> + Send + 'a>>,
    );

    impl<'a> Future for SendVoteFuture<'a> {
        type Output = Result<VoteResponse, TransportError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.as_mut().poll(cx)
        }
    }

    pub struct ServeFuture<'a>(
        pub Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>>,
    );

    impl<'a> Future for ServeFuture<'a> {
        type Output = Result<(), TransportError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.as_mut().poll(cx)
        }
    }
}

/// Abstract transport. Production: [`TcpTransport`]. Future: a
/// `SimTransport` for the P7 deterministic simulation testing harness.
pub trait Transport: Send + Sync + 'static {
    /// Send a Raft consensus message to `to` and await its reply.
    /// Used by the leader's heartbeat / AppendEntries / RequestVote /
    /// InstallSnapshot paths, all of which today go through
    /// `RpcClient::call`.
    ///
    /// **Note:** the trait does not validate that `msg` is a request
    /// variant. `TcpTransport` will faithfully encode and send any
    /// `RaftMessage`; callers are responsible for sending request
    /// variants and matching the reply variant. A future `SimTransport`
    /// can choose to short-circuit reply variants to a Protocol error
    /// if that aids deterministic simulation, but the abstraction
    /// itself is intentionally neutral.
    fn send_raft<'a>(
        &'a self,
        to: &'a str,
        msg: RaftMessage,
    ) -> futures::SendRaftFuture<'a>;

    /// Send a 2PC coordinator `VoteRequest` to `to` and await its
    /// `VoteResponse`. Used by the leader's vote fan-out during a
    /// 2PC round (`coordinator::coordinate_tx`).
    ///
    /// Per-RPC timeout is applied by the caller (typically via
    /// `tokio::time::timeout`). `TcpTransport::send_vote` itself does
    /// not add a timeout because the test / simulation harness may
    /// want to drive virtual time without a hard wall-clock deadline.
    fn send_vote<'a>(
        &'a self,
        to: &'a str,
        req: crate::coordination::VoteRequest,
    ) -> futures::SendVoteFuture<'a>;

    /// Accept inbound connections and route by `DispatchKind` to the
    /// appropriate handler (today: `RpcServer::dispatch`). Returns
    /// when [`StopSignal::stop`] is invoked.
    fn serve<'a>(
        &'a self,
        raft_node: Arc<RwLock<RaftNode>>,
        stop: StopSignal,
    ) -> futures::ServeFuture<'a>;
}

/// Production transport. Wraps the existing TCP path:
/// `RpcClient::call` / `RpcClient::send_tx_vote_rpc` for outbound,
/// `RpcServer::dispatch` on the listener side. Framing and encoding
/// stay in `crate::raft::transport` and `crate::raft::rpc` /
/// `crate::raft::proto` — those are exercised independently by the
/// existing test suite and don't need to be re-implemented here.
///
/// Stateless. The listener is owned by the bound `TcpListener`, which
/// the caller passes into [`TcpTransport::with_listener`]. Send
/// methods don't need the listener.
pub struct TcpTransport {
    listener: Option<Arc<TcpListener>>,
}

impl TcpTransport {
    /// Construct a `TcpTransport` for outbound calls only (no
    /// listener). Used by followers / voters / non-leader nodes that
    /// only ever initiate RPCs.
    pub fn new() -> Self {
        Self { listener: None }
    }

    /// Construct a `TcpTransport` with a pre-bound `TcpListener`.
    /// The listener is taken over by `serve` for the lifetime of the
    /// accept loop. Tests that want to drive `serve` on `127.0.0.1:0`
    /// pass a `TcpListener::bind("127.0.0.1:0").await.unwrap()` here.
    pub fn with_listener(listener: TcpListener) -> Self {
        Self {
            listener: Some(Arc::new(listener)),
        }
    }
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: a fresh `Arc<dyn Transport>` wrapping `TcpTransport`
/// (send-only, no listener). Tests that want to drive `serve` use
/// `TcpTransport::with_listener` directly and pass the resulting
/// `Arc<dyn Transport>` into `new_with_clock_and_transport`.
pub fn system_transport() -> Arc<dyn Transport> {
    Arc::new(TcpTransport::new())
}

/// Translate the underlying RPC errors into [`TransportError`].
///
/// `RpcClient::call` / `RpcClient::send_tx_vote_rpc` use
/// `anyhow::Error`; we inspect the kind via the source chain to
/// decide between `Unreachable` (TCP connect failed), `Timeout`
/// (the `tokio::time::timeout` fired), and `Protocol` (anything else).
fn classify_rpc_err(err: anyhow::Error) -> TransportError {
    let chain = format!("{:?}", err);
    if chain.contains("Connection refused") || chain.contains("No route to host") {
        TransportError::Unreachable(format!("{}", err))
    } else if chain.contains("deadline") || chain.contains("timed out") || chain.contains("Timeout") {
        // `tokio::time::timeout` returns an `Elapsed` error whose
        // Display is "deadline has elapsed"; anyhow forwards it as-is.
        TransportError::Timeout(Duration::from_secs(0))
    } else {
        TransportError::Protocol(format!("{}", err))
    }
}

impl Transport for TcpTransport {
    fn send_raft<'a>(
        &'a self,
        to: &'a str,
        msg: RaftMessage,
    ) -> futures::SendRaftFuture<'a> {
        // Pick a per-RPC timeout that matches the pre-trait behavior:
        // RequestVote gets `rpc_request_vote_timeout_ms()`; AppendEntries
        // and InstallSnapshot get the longer `rpc_append_entries_timeout_ms()`.
        // `RpcClient::call` internally wraps `TcpStream::connect` in a
        // `tokio::time::timeout(timeout_duration, ...)`; we forward the
        // same value so the connect-timeout behavior is preserved.
        let timeout_ms = match &msg {
            RaftMessage::RequestVote(_) => {
                crate::config::Config::rpc_request_vote_timeout_ms()
            }
            RaftMessage::AppendEntries(_) | RaftMessage::InstallSnapshot(_) => {
                crate::config::Config::rpc_append_entries_timeout_ms()
            }
            // Reply variants should never be sent outbound; if they
            // are, the call will return Protocol. Default to the
            // shorter vote timeout.
            _ => crate::config::Config::rpc_request_vote_timeout_ms(),
        };
        let to_owned = to.to_string();
        let fut = Box::pin(async move {
            RpcClient::call(
                &to_owned,
                msg,
                Duration::from_millis(timeout_ms),
            )
            .await
            .map_err(classify_rpc_err)
        });
        futures::SendRaftFuture(fut)
    }

    fn send_vote<'a>(
        &'a self,
        to: &'a str,
        req: crate::coordination::VoteRequest,
    ) -> futures::SendVoteFuture<'a> {
        // Per-call wall-clock timeout is the caller's responsibility
        // (see `Transport::send_vote` doc). For TcpTransport, callers
        // typically wrap with `tokio::time::timeout`; the underlying
        // `RpcClient::send_tx_vote_rpc` only times out the TCP connect.
        let to_owned = to.to_string();
        let fut = Box::pin(async move {
            RpcClient::send_tx_vote_rpc(&to_owned, req, Duration::from_secs(5))
                .await
                .map_err(classify_rpc_err)
        });
        futures::SendVoteFuture(fut)
    }

    fn serve<'a>(
        &'a self,
        raft_node: Arc<RwLock<RaftNode>>,
        stop: StopSignal,
    ) -> futures::ServeFuture<'a> {
        // The listener must have been provided at construction
        // time. If `serve` is called on a listener-less TcpTransport
        // (e.g. a follower-only node), we surface Protocol immediately.
        let Some(listener) = self.listener.clone() else {
            return futures::ServeFuture(Box::pin(async move {
                Err(TransportError::Protocol(
                    "TcpTransport::serve called without a listener".to_string(),
                ))
            }));
        };

        let fut = async move {
            loop {
                tokio::select! {
                    _ = stop.0.notified() => {
                        return Ok(());
                    }
                    accept = listener.accept() => {
                        match accept {
                            Ok((stream, _)) => {
                                let n = raft_node.clone();
                                tokio::spawn(async move {
                                    RpcServer::dispatch(stream, n).await;
                                });
                            }
                            Err(e) => {
                                warn!(error = %e, "raft listener accept error");
                                // Don't return — keep accepting. Real
                                // accept errors are usually transient
                                // (per-process fd exhaustion, etc.).
                            }
                        }
                    }
                }
            }
        };
        futures::ServeFuture(Box::pin(fut))
    }
}

// (no future-type imports needed; the SendRaftFuture / SendVoteFuture /
// ServeFuture structs in `futures` already do the boxing.)

// Convenience for downstream consumers (tests / future sim code).
//
// `tokio::io::split` returns `(ReadHalf, WriteHalf)`. `RpcServer::dispatch`
// already takes ownership of a `TcpStream` and calls `.split()` itself,
// so we just forward the stream as-is.
#[allow(dead_code)]
fn _assert_stream<S>(_: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rpc_err_connection_refused_is_unreachable() {
        // anyhow::Error::msg produces a chain that mentions the
        // message; we use a known-shape string here to verify the
        // classifier picks the right variant. The real TCP layer
        // surfaces "Connection refused" via the std::io::Error
        // message wrapped in anyhow's context chain.
        let err = anyhow::anyhow!("Connection refused (os error 61)");
        match classify_rpc_err(err) {
            TransportError::Unreachable(_) => {}
            other => panic!("expected Unreachable, got {:?}", other),
        }
    }

    #[test]
    fn classify_rpc_err_deadline_is_timeout() {
        let err = anyhow::anyhow!("deadline has elapsed");
        match classify_rpc_err(err) {
            TransportError::Timeout(_) => {}
            other => panic!("expected Timeout, got {:?}", other),
        }
    }

    #[test]
    fn classify_rpc_err_other_is_protocol() {
        let err = anyhow::anyhow!("malformed envelope");
        match classify_rpc_err(err) {
            TransportError::Protocol(_) => {}
            other => panic!("expected Protocol, got {:?}", other),
        }
    }

    #[test]
    fn stop_signal_can_be_cloned_and_triggered() {
        let stop = StopSignal::new();
        let stop2 = stop.clone();
        // notify_waiters only wakes already-waiting tasks; calling
        // it without an active waiter is a no-op and must not panic.
        stop2.stop();
    }

    #[tokio::test]
    async fn tcp_transport_serve_without_listener_is_protocol_error() {
        // `serve` on a send-only TcpTransport must fail fast rather
        // than hang waiting for a listener that will never arrive.
        let t = TcpTransport::new();
        let node = {
            // Build a throwaway node so we have something to hand
            // serve(). We don't actually exercise it; serve() must
            // fail before touching the node.
            use crate::raft::storage::RaftStorage;
            use crate::state_machine::{StateMachine, StateMachineConfig};
            let dir = tempfile::tempdir().unwrap();
            let storage = RaftStorage::new_with_paths(
                dir.path().join("t.wal").to_str().unwrap().to_string(),
                dir.path().join("t_meta.json").to_str().unwrap().to_string(),
                dir.path().join("t_snapshot.json").to_str().unwrap().to_string(),
            );
            let sm_dir = dir.path().join("t_sm");
            let sm = Arc::new(RwLock::new(StateMachine::open(StateMachineConfig {
                data_dir: sm_dir,
                memtable_size_threshold: 1024 * 1024,
            }).unwrap()));
            Arc::new(RwLock::new(RaftNode::new_with_storage(
                "t".to_string(),
                vec![],
                sm,
                storage,
            )))
        };
        let result = t.serve(node, StopSignal::new()).await;
        assert!(matches!(result, Err(TransportError::Protocol(_))));
    }
}