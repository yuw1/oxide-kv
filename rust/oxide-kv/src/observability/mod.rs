//! Observability — Prometheus metrics + tracer hooks (P8 PR 8).
//!
//! Exposes a `/metrics` HTTP endpoint on `OXIDE_METRICS_PORT`
//! (default `9100`) that returns the standard Prometheus text
//! exposition format. The endpoint is intentionally minimal — a
//! hand-rolled HTTP/1.1 reader over `tokio::net::TcpListener` — to
//! avoid pulling in `hyper` for what is effectively one GET route
//! returning text. The implementation is `~50 LOC` of parsing +
//! `~10 LOC` of response framing; everything else is delegated to
//! the `prometheus` crate's encoder.
//!
//! ## Metric inventory (P8 PR 8)
//!
//! All metrics are scoped per-node — a multi-node cluster exposes
//! each node's metrics on its own port. Aggregation happens at the
//! Prometheus scrape layer, not here.
//!
//! | Metric                                  | Type      | Source                                |
//! |-----------------------------------------|-----------|---------------------------------------|
//! | `oxide_raft_term`                       | gauge     | `RaftNode::current_term`              |
//! | `oxide_raft_commit_index`               | gauge     | `RaftNode::commit_index`              |
//! | `oxide_raft_last_applied`               | gauge     | `RaftNode::last_applied`              |
//! | `oxide_raft_log_length`                 | gauge     | `RaftNode::log.len()`                 |
//! | `oxide_raft_role`                       | gauge     | `RaftNode::state` (0=F,1=C,2=L,3=P)  |
//! | `oxide_raft_snapshot_age_seconds`       | gauge     | `now - last_snapshot_unix_ms`         |
//! | `oxide_raft_snapshot_bytes`             | gauge     | `last snapshot file size`             |
//! | `oxide_tx_pending_count`                | gauge     | `StateMachine::pending_tx_count()`    |
//! | `oxide_tx_timeout_aborted_total`        | counter   | `run_tx_timeout_loop` force-aborts    |
//! | `oxide_tx_admin_aborted_total`          | counter   | `ClientHandler::abort_tx` calls       |
//! | `oxide_peer_match_index{peer="..."}`    | gauge     | `RaftNode::match_index[peer]`         |
//! | `oxide_peer_next_index{peer="..."}`     | gauge     | `RaftNode::next_index[peer]`          |
//!
//! `peer_rtt_seconds` was on the P8 wishlist but is **deferred to
//! a follow-up PR**: emitting it requires new RTT-tracking plumbing
//! in the AppendEntries send path that doesn't currently exist,
//! and PR #8's diff is already large enough without it.
//!
//! ## Tracer
//!
//! [`Tracer`] is a typed noop interface so future PRs can swap in a
//! real OTel implementation behind the same call sites without
//! touching every call. The default [`NoopTracer`] discards all
//! spans. There is no runtime cost — the noop branch is a single
//! `inline(always)` early return.

pub mod registry;
pub mod server;
pub mod tracer;

pub use registry::{Metrics, MetricsHandle};
pub use server::run_metrics_server;
pub use tracer::{NoopTracer, Span, Tracer};
