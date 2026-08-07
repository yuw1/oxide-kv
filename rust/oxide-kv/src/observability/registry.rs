//! Prometheus metric registry + collection glue (P8 PR 8).
//!
//! [`Metrics`] owns the typed handles for every gauge / counter
//! we export. [`MetricsHandle`] is the `Arc`-shared wrapper that
//! `RaftNode` / `StateMachine` / `coordinator::run_tx_timeout_loop`
//! / `ClientHandler::abort_tx` hold — they call typed setters
//! (`commit_index.set(v)`) at every state transition, and the
//! `/metrics` HTTP handler calls [`Registry::gather`] to encode the
//! current snapshot.
//!
//! ## Why `Arc<Metrics>` rather than a global?
//!
//! `Arc<Metrics>` keeps the dependency direction clean: the
//! observability module depends on `raft::node` for the role enum,
//! but `raft::node` only depends on `observability::MetricsHandle`
//! for the typed setters. A `OnceLock` global would couple every
//! callsite to module-level initialization order.
//!
//! ## Update cadence
//!
//! Metrics are **eagerly updated** at every state transition
//! (`become_leader`, `apply_logs`, `maybe_snapshot`, ...). They are
//! NOT pulled at scrape time — that would require holding the
//! `RaftNode` lock from the HTTP handler, which is a bad fit for
//! the scrape rate (Prometheus default = 15 s) and any tail latency
//! in `apply_logs`. Eager updates cost a few `i64` stores per
//! transition, which is negligible.
//!
//! Per-peer gauges (`peer_match_index`, `peer_next_index`) are
//! updated at the end of every AppendEntries reply, in `sync_logs`.

use prometheus::{Encoder, IntCounter, IntGauge, IntGaugeVec, Registry, TextEncoder};
use std::sync::Arc;

/// All typed metric handles. Holds the `prometheus::Registry`
/// underneath; `gather()` returns the text-format encoded snapshot.
pub struct Metrics {
    /// Backing Prometheus registry. Every gauge / counter
    /// registered here is included in `gather()`.
    pub registry: Registry,

    /// `oxide_raft_term` — `current_term` on this node. Monotonic
    /// within a term; bumps on every observed higher term.
    pub raft_term: IntGauge,

    /// `oxide_raft_commit_index` — index of highest log entry
    /// known to be replicated to a quorum.
    pub raft_commit_index: IntGauge,

    /// `oxide_raft_last_applied` — index of highest log entry
    /// applied to the state machine.
    pub raft_last_applied: IntGauge,

    /// `oxide_raft_log_length` — `self.log.len()` on the node.
    /// Diverges from `last_applied` by the in-flight window.
    pub raft_log_length: IntGauge,

    /// `oxide_raft_role` — encoded `NodeState`. Numeric values:
    ///   0 = Follower
    ///   1 = Candidate
    ///   2 = Leader
    ///   3 = PreCandidate (P8 PR 5)
    /// Numeric encoding lets ops alert on
    /// `oxide_raft_role > 0` to detect instability.
    pub raft_role: IntGauge,

    /// `oxide_raft_snapshot_age_seconds` — wall-clock seconds since
    /// the most recent snapshot was installed on this node. `-1`
    /// means "no snapshot taken yet" (treat as `Inf` in alerting).
    pub raft_snapshot_age_seconds: IntGauge,

    /// `oxide_raft_snapshot_bytes` — on-disk size of the most
    /// recent snapshot file. `-1` means "no snapshot yet".
    pub raft_snapshot_bytes: IntGauge,

    /// `oxide_tx_pending_count` — number of 2PC transactions in
    /// the state machine's `pending_txs` map at the moment of last
    /// collection.
    pub tx_pending_count: IntGauge,

    /// `oxide_tx_timeout_aborted_total` — monotonic counter of
    /// stuck-tx force-aborts performed by the coordinator sweep
    /// (`coordinator::run_tx_timeout_loop`). Pairs with the
    /// `client::abort_tx` counter for an ops view of how many txs
    /// are dying for ops / protocol reasons vs. application
    /// reasons.
    pub tx_timeout_aborted_total: IntCounter,

    /// `oxide_tx_admin_aborted_total` — monotonic counter of
    /// client-driven `AbortTx` JSON RPC calls that succeeded.
    /// Errors do not increment this counter.
    pub tx_admin_aborted_total: IntCounter,

    /// `oxide_peer_match_index{peer="..."}` — per-peer known match
    /// index. Diverges from leader's `commit_index` for any peer
    /// that is currently partitioned / lagging.
    pub peer_match_index: IntGaugeVec,

    /// `oxide_peer_next_index{peer="..."}` — per-peer next-index
    /// hint. Diverges from `match_index + 1` only during the
    /// backoff window after an `AppendEntries` rejection.
    pub peer_next_index: IntGaugeVec,
}

/// `Arc`-shared wrapper so `RaftNode` / `StateMachine` /
/// `coordinator::run_tx_timeout_loop` / `ClientHandler` can call
/// typed setters without cloning every gauge.
pub type MetricsHandle = Arc<Metrics>;

impl Metrics {
    /// Build a fresh registry with all gauges / counters
    /// pre-registered. `initial_peers` is the optional list of
    /// peer addresses known at construction time — passing them
    /// ensures `peer_match_index` / `peer_next_index` show up in
    /// the very first `/metrics` response with `0` values rather
    /// than disappearing until the first AppendEntries reply.
    pub fn new() -> Result<MetricsHandle, prometheus::Error> {
        Self::with_peers(&[])
    }

    /// Like [`Metrics::new`] but pre-creates `peer_match_index` /
    /// `peer_next_index` label instances for every address in
    /// `initial_peers` (so they show up as `0` in the first
    /// scrape, rather than being absent until the first
    /// AppendEntries reply).
    pub fn with_peers(initial_peers: &[String]) -> Result<MetricsHandle, prometheus::Error> {
        let registry = Registry::new_custom(Some("oxide".to_string()), None)?;

        let raft_term = IntGauge::new("raft_term", "Raft current term on this node")?;
        let raft_commit_index = IntGauge::new("raft_commit_index", "Raft commit_index")?;
        let raft_last_applied = IntGauge::new("raft_last_applied", "Raft last_applied")?;
        let raft_log_length = IntGauge::new("raft_log_length", "Raft log length")?;
        let raft_role = IntGauge::new("raft_role", "Raft role (0=F,1=C,2=L,3=P)")?;
        let raft_snapshot_age_seconds = IntGauge::new(
            "raft_snapshot_age_seconds",
            "Seconds since the most recent snapshot was installed (-1 = never)",
        )?;
        let raft_snapshot_bytes = IntGauge::new(
            "raft_snapshot_bytes",
            "On-disk size of the most recent snapshot file (-1 = never)",
        )?;

        let tx_pending_count = IntGauge::new("tx_pending_count", "Pending 2PC transactions")?;
        let tx_timeout_aborted_total = IntCounter::new(
            "tx_timeout_aborted_total",
            "Force-aborts by coordinator sweep",
        )?;
        let tx_admin_aborted_total = IntCounter::new(
            "tx_admin_aborted_total",
            "Successful AbortTx admin RPC calls",
        )?;

        let peer_match_index = IntGaugeVec::new(
            prometheus::Opts::new("peer_match_index", "Per-peer match index"),
            &["peer"],
        )?;
        let peer_next_index = IntGaugeVec::new(
            prometheus::Opts::new("peer_next_index", "Per-peer next_index"),
            &["peer"],
        )?;

        registry.register(Box::new(raft_term.clone()))?;
        registry.register(Box::new(raft_commit_index.clone()))?;
        registry.register(Box::new(raft_last_applied.clone()))?;
        registry.register(Box::new(raft_log_length.clone()))?;
        registry.register(Box::new(raft_role.clone()))?;
        registry.register(Box::new(raft_snapshot_age_seconds.clone()))?;
        registry.register(Box::new(raft_snapshot_bytes.clone()))?;
        registry.register(Box::new(tx_pending_count.clone()))?;
        registry.register(Box::new(tx_timeout_aborted_total.clone()))?;
        registry.register(Box::new(tx_admin_aborted_total.clone()))?;
        registry.register(Box::new(peer_match_index.clone()))?;
        registry.register(Box::new(peer_next_index.clone()))?;

        // Pre-seed role gauge to "Follower" so the first scrape
        // (which may arrive before any state transition) returns
        // a defined value rather than nothing.
        raft_role.set(0);

        // Pre-seed "no snapshot yet" sentinel so the gauge exists
        // from t=0; it transitions to wall-clock seconds the first
        // time `update_snapshot` runs.
        raft_snapshot_age_seconds.set(-1);
        raft_snapshot_bytes.set(-1);

        // Pre-create per-peer label instances with sentinel
        // values. Without this, `IntGaugeVec` metrics only show
        // up in `gather()` output after the first labeled
        // `set()` — so a brand-new cluster would have a metrics
        // scrape with no `peer_*` rows until the first
        // AppendEntries reply. Sentinel `0` matches "we haven't
        // observed any reply from this peer yet".
        for peer in initial_peers {
            peer_match_index.with_label_values(&[peer.as_str()]).set(0);
            peer_next_index.with_label_values(&[peer.as_str()]).set(0);
        }

        Ok(Arc::new(Metrics {
            registry,
            raft_term,
            raft_commit_index,
            raft_last_applied,
            raft_log_length,
            raft_role,
            raft_snapshot_age_seconds,
            raft_snapshot_bytes,
            tx_pending_count,
            tx_timeout_aborted_total,
            tx_admin_aborted_total,
            peer_match_index,
            peer_next_index,
        }))
    }

    /// Encode the current snapshot in Prometheus text exposition
    /// format. Cheap — the `prometheus` crate's encoder just walks
    /// the registry.
    pub fn gather_text(&self) -> prometheus::Result<Vec<u8>> {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&metric_families, &mut buf)?;
        Ok(buf)
    }

    /// Update a per-peer match-index gauge. No-op for an unknown
    /// peer (the gauge stays at its default 0, which is correct —
    /// "we haven't observed any reply from this peer").
    pub fn set_peer_match_index(&self, peer: &str, value: i64) {
        self.peer_match_index.with_label_values(&[peer]).set(value);
    }

    /// Update a per-peer next-index gauge.
    pub fn set_peer_next_index(&self, peer: &str, value: i64) {
        self.peer_next_index.with_label_values(&[peer]).set(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_registers_all_metrics() {
        let m =
            Metrics::with_peers(&["127.0.0.1:9002".to_string()]).expect("registry construction");
        // `gather_text` returning a non-empty buffer implies every
        // registered gauge / counter was encoded.
        let buf = m.gather_text().expect("encode");
        let s = std::str::from_utf8(&buf).expect("utf-8");
        assert!(s.contains("oxide_raft_term"), "missing raft_term: {}", s);
        assert!(
            s.contains("oxide_raft_commit_index"),
            "missing raft_commit_index"
        );
        assert!(
            s.contains("oxide_raft_last_applied"),
            "missing raft_last_applied"
        );
        assert!(
            s.contains("oxide_raft_log_length"),
            "missing raft_log_length"
        );
        assert!(s.contains("oxide_raft_role"), "missing raft_role");
        assert!(
            s.contains("oxide_raft_snapshot_age_seconds"),
            "missing raft_snapshot_age_seconds"
        );
        assert!(
            s.contains("oxide_raft_snapshot_bytes"),
            "missing raft_snapshot_bytes"
        );
        assert!(
            s.contains("oxide_tx_pending_count"),
            "missing tx_pending_count"
        );
        assert!(
            s.contains("oxide_tx_timeout_aborted_total"),
            "missing tx_timeout_aborted_total"
        );
        assert!(
            s.contains("oxide_tx_admin_aborted_total"),
            "missing tx_admin_aborted_total"
        );
        assert!(
            s.contains("oxide_peer_match_index"),
            "missing peer_match_index"
        );
        assert!(
            s.contains("oxide_peer_next_index"),
            "missing peer_next_index"
        );
    }

    #[test]
    fn typed_setters_appear_in_gather() {
        let m =
            Metrics::with_peers(&["127.0.0.1:9002".to_string()]).expect("registry construction");
        m.raft_term.set(7);
        m.raft_commit_index.set(42);
        m.raft_log_length.set(99);
        m.raft_role.set(2); // Leader
        m.set_peer_match_index("127.0.0.1:9002", 17);
        m.set_peer_next_index("127.0.0.1:9002", 18);
        m.tx_pending_count.set(3);
        m.tx_admin_aborted_total.inc();

        let buf = m.gather_text().expect("encode");
        let s = std::str::from_utf8(&buf).expect("utf-8");

        // Spot-check value lines. The Prometheus text format is:
        //   name{labels} value [timestamp]
        // We don't pin the timestamp, so we just look for the
        // value-bearing line.
        assert!(
            s.lines()
                .any(|l| l.starts_with("oxide_raft_term ") && l.ends_with(" 7")),
            "raft_term value missing:\n{}",
            s
        );
        assert!(
            s.lines()
                .any(|l| l.starts_with("oxide_raft_commit_index ") && l.ends_with(" 42")),
            "commit_index missing:\n{}",
            s
        );
        assert!(
            s.lines().any(|l| l.starts_with("oxide_peer_match_index{")
                && l.contains("9002")
                && l.ends_with(" 17")),
            "peer_match_index missing:\n{}",
            s
        );
        assert!(
            s.lines()
                .any(|l| l.starts_with("oxide_tx_admin_aborted_total ") && l.ends_with(" 1")),
            "admin_aborted_total missing:\n{}",
            s
        );
    }

    #[test]
    fn counters_are_monotonic() {
        let m = Metrics::new().expect("registry construction");
        m.tx_timeout_aborted_total.inc_by(3);
        m.tx_admin_aborted_total.inc();
        let buf = m.gather_text().expect("encode");
        let s = std::str::from_utf8(&buf).expect("utf-8");
        assert!(
            s.lines()
                .any(|l| l.starts_with("oxide_tx_timeout_aborted_total ") && l.ends_with(" 3")),
            "timeout counter wrong:\n{}",
            s
        );
        assert!(
            s.lines()
                .any(|l| l.starts_with("oxide_tx_admin_aborted_total ") && l.ends_with(" 1")),
            "admin counter wrong:\n{}",
            s
        );
    }

    #[test]
    fn registry_with_custom_namespace() {
        // `Registry::new_custom(Some("oxide".to_string()), None)`
        // prefixes every metric with `oxide_`. Pin that behavior.
        let m = Metrics::new().expect("registry construction");
        let buf = m.gather_text().expect("encode");
        let s = std::str::from_utf8(&buf).expect("utf-8");
        // `# HELP oxide_raft_term` and `oxide_raft_term 0` are both
        // present; nothing should appear without the `oxide_` prefix.
        assert!(
            s.contains("# HELP oxide_raft_term"),
            "namespace prefix missing on HELP: {}",
            s
        );
        assert!(
            s.contains("oxide_raft_term 0"),
            "namespace prefix missing on value line: {}",
            s
        );
        // HELP lines for the unprefixed name should NOT appear.
        assert!(
            !s.contains("# HELP raft_term\n"),
            "unprefixed HELP line leaked: {}",
            s
        );
    }
}
