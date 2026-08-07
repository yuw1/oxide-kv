//! Cross-process 3-node smoke test (P8 PR #9).
//!
//! Why this test exists
//! --------------------
//! Every other integration test in this crate drives the Raft core
//! either in-process (no real network) or against a single node.
//! Neither path exercises the production deployment: real TCP
//! across processes, the bootstrap script, the systemd unit, and
//! the Prometheus `/metrics` surface. This file is the gap.
//!
//! What this test does (infrastructure scope)
//! ------------------------------------------
//! 1. **Boot 3 real `oxide-kv` processes** via
//!    `deploy/scripts/bootstrap-cluster.sh start`, each on its own
//!    data dir under a private `BASE`. The script writes a
//!    `cluster.jsonl` (NDJSON, one record per node) so the test can
//!    discover each node's `raft` / `client` / `metrics` ports
//!    without scraping log files.
//! 2. **Drive the operator path**: scrape `/metrics` on every node,
//!    assert the Prometheus text format is served (regression guard
//!    for P8 PR #8). We do **not** assert that exactly one node
//!    reports `role=2` — that's a consensus-level invariant whose
//!    pre-vote tie bug surfaced this test; see "Bugs surfaced"
//!    below.
//! 3. **Verify the JSON client protocol** speaks the right
//!    envelope on at least one node's client port (any node will
//!    reply with `{"error":"Not a leader..."}` if it's not the
//!    current leader, which is itself proof that the line protocol
//!    + JSON parsing round-trips correctly).
//! 4. **Teardown**: `bootstrap-cluster.sh stop` + `clean`.
//!    Re-run-safe: a fresh `BASE` per test run means stale
//!    processes don't survive from one `cargo test` invocation to
//!    the next.
//!
//! What this test does NOT do (consensus scope)
//! --------------------------------------------
//! Historical note: three consensus-scope tests in this file
//! (`single_leader_converges_within_15_seconds`,
//! `commit_index_advances_after_set_on_cluster`,
//! `set_then_get_on_leader_returns_written_value`) were
//! originally `#[ignore]`-gated on the pre-vote tie / split-brain
//! and multi-node commit-advance bugs. Both were fixed by PR #50
//! (bootstrap peer-list off-by-one) and PR #51 (pre-vote must not
//! self-vote; incumbent leader must not demote on same-term AE),
//! and the three tests now run un-gated — verified locally before
//! un-gating and in CI ever since.
//!
//! Bugs surfaced by this test (fixed in later PRs, not here)
//! ----------------------------------------------------------
//! - **Metrics port collision**: before P8 PR #8, the metrics
//!   endpoint wasn't configurable. After PR #8, the bootstrap
//!   script (now) defaults node-N to `127.0.0.1:9100 + N*100`
//!   (9100/9200/9300). The collision was caught when the second
//!   node tried to bind 9100 and failed with `EADDRINUSE`. The
//!   fix lives in `bootstrap-cluster.sh::start_one`; this test
//!   is the regression guard.
//! - **Pre-vote tie / split-brain** (FIXED in PR #51): two nodes
//!   entering pre-vote ~simultaneously could both pass pre-vote
//!   (each gets 2 of 3 yes votes including the other's), both
//!   promote to Candidate, both win a real vote round, and both
//!   become Leader. We observed 30+ consecutive split-brain
//!   observations in a single 15-second `find_leader` window
//!   before bailing. Root causes: pre-vote allowed a self-vote,
//!   and an incumbent leader demoted itself on receiving a
//!   same-term AppendEntries from a rival. `single_leader_*` and
//!   `set_then_get_*` below are the regression guards.
//! - **`commit_index` stuck at 0** (FIXED in PR #50) after a
//!   successful Set: the bootstrap script's off-by-one peer list
//!   broke replication so `commit_index` never advanced.
//!   `commit_index_advances_after_set_on_cluster` is the
//!   regression guard.
//!
//! CI integration
//! --------------
//! The active tests in this file are wired into
//! `.github/workflows/rust.yml` as a separate step that runs
//! *after* `cargo build --release --bin oxide-kv`. It uses the
//! freshly built binary in `target/release/oxide-kv`, which is
//! what `bootstrap-cluster.sh` defaults to via `$BIN`.
//!
//! Locally on a dev box: `cargo test --test cross_process_smoke
//! -- --test-threads=1` will build the binary, start 3 nodes,
//! run the suite, and tear down. Use `-- --test-threads=1` to
//! serialize the three runs (each spins up its own cluster).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Default `target/release/oxide-kv` path. CI runners lay the
/// binary out at the workspace root; local builds follow the
/// same convention unless `$OXIDE_KV_BIN` is set.
fn binary_path() -> PathBuf {
    if let Ok(p) = std::env::var("OXIDE_KV_BIN") {
        return PathBuf::from(p);
    }
    // CARGO_MANIFEST_DIR is set by `cargo test` to the crate
    // root. The binary lives at `<workspace>/target/{release,debug}/oxide-kv`.
    // Prefer release (the dedicated CI step ships a release binary),
    // but fall back to debug so local `cargo test` works without
    // an explicit `cargo build --release` first.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR must be set by cargo test");
    let manifest = PathBuf::from(manifest_dir);
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let release = workspace.join("target").join("release").join("oxide-kv");
    if release.exists() {
        return release;
    }
    let debug = workspace.join("target").join("debug").join("oxide-kv");
    if debug.exists() {
        return debug;
    }
    panic!(
        "oxide-kv binary not found at {} or {}. Run `cargo build --release` \
         (or `cargo build`) first, or set $OXIDE_KV_BIN.",
        release.display(),
        debug.display()
    );
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set"),
    );
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // `raft` / `pid` / `data_dir` / `log` are
                    // captured for diagnostics + future tests but
                    // not all read today.
struct NodeRecord {
    node: String,
    raft: u16,
    client: u16,
    metrics: u16,
    pid: u32,
    data_dir: String,
    log: String,
}

/// Read `cluster.jsonl` from the bootstrap-script `start` step.
/// Each line is one JSON object describing a single node.
fn read_cluster(base: &std::path::Path) -> Vec<NodeRecord> {
    let jsonl = base.join("cluster.jsonl");
    let raw = std::fs::read_to_string(&jsonl)
        .unwrap_or_else(|e| panic!("read {}: {}", jsonl.display(), e));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("parse jsonl line `{}`: {}", l, e));
            NodeRecord {
                node: v["node"].as_str().unwrap().to_string(),
                raft: v["raft"].as_u64().unwrap() as u16,
                client: v["client"].as_u64().unwrap() as u16,
                metrics: v["metrics"].as_u64().unwrap() as u16,
                pid: v["pid"].as_u64().unwrap() as u32,
                data_dir: v["data_dir"].as_str().unwrap().to_string(),
                log: v["log"].as_str().unwrap().to_string(),
            }
        })
        .collect()
}

/// Wait until every node's client port and metrics port are
/// accepting TCP connections. Without this, the test races
/// against `oxide-kv`'s startup sequence (data dir replay,
/// meta restore, listener bind) and gets ECONNREFUSED for
/// nodes that are still warming up. Bounded by `timeout`;
/// panics with a clear message if any port doesn't bind.
fn wait_for_ports(records: &[NodeRecord], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut pending: Vec<(String, u16)> = Vec::new();
    for rec in records {
        pending.push((format!("{}.client", rec.node), rec.client));
        pending.push((format!("{}.metrics", rec.node), rec.metrics));
    }
    let mut idx = 0;
    while idx < pending.len() && Instant::now() < deadline {
        let (label, port) = &pending[idx];
        match TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], *port)),
            Duration::from_millis(200),
        ) {
            Ok(_) => {
                idx += 1;
                continue;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let _ = label;
    }
    if idx < pending.len() {
        let remaining: Vec<_> = pending[idx..].iter().collect();
        panic!(
            "{} ports never came up within {:?}: {:?}",
            remaining.len(),
            timeout,
            remaining
        );
    }
}

/// Start the cluster. `BASE` is a private temp dir; `bootstrap-cluster.sh`
/// writes `<BASE>/cluster.jsonl` after the start succeeds. We do not
/// fail on metrics-port warnings in the per-node log — the test
/// is here to catch them.
fn start_cluster(base: &std::path::Path) {
    let bin = binary_path();
    assert!(
        bin.exists(),
        "oxide-kv binary not found at {}. Run `cargo build --release` first or set $OXIDE_KV_BIN.",
        bin.display()
    );
    let script = workspace_root().join("deploy/scripts/bootstrap-cluster.sh");
    let status = Command::new("bash")
        .arg(&script)
        .arg("start")
        .env("BASE", base)
        .env("BIN", &bin)
        .status()
        .expect("spawn bootstrap-cluster.sh start");
    assert!(
        status.success(),
        "bootstrap-cluster.sh start exited with {}",
        status
    );
}

/// Minimal HTTP GET that reads until the server closes the
/// socket. Used only for the local `/metrics` endpoint, which
/// the server always closes after writing the response.
fn http_get(url: &str, timeout: Duration) -> std::io::Result<String> {
    let stripped = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let (host_port, path) = stripped
        .split_once('/')
        .unwrap_or((stripped, "metrics"));
    let mut stream = TcpStream::connect(host_port)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let req = format!(
        "GET /{} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        path
    );
    stream.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let s = String::from_utf8_lossy(&buf).into_owned();
    let body = s.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok(body)
}

/// Parse a Prometheus text exposition and return the value of
/// the metric whose line starts with `name ` (no labels).
/// Returns the *first* match — for labeled metrics use
/// `parse_labeled_metric`.
fn parse_metric(body: &str, name: &str) -> Option<i64> {
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(name) {
            let trimmed = rest.trim_start();
            if trimmed.starts_with('{') {
                continue;
            }
            if let Some(v) = trimmed.split_whitespace().next() {
                return v.parse().ok();
            }
        }
    }
    None
}

/// Issue one JSON-RPC command to a node's client port. Reads
/// one line-delimited JSON response. Used for Set / Get /
/// AbortTx. Times out after `timeout` if the server doesn't
/// respond.
#[allow(dead_code)]
fn issue_command(
    client_port: u16,
    cmd: &str,
    timeout: Duration,
) -> serde_json::Value {
    let mut stream = TcpStream::connect(("127.0.0.1", client_port))
        .expect("connect client port");
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.set_write_timeout(Some(timeout)).unwrap();
    stream.write_all(cmd.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut s = String::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                s.push(byte[0] as char);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => panic!("read error: {}", e),
        }
    }
    serde_json::from_str(s.trim()).unwrap_or_else(|e| {
        panic!(
            "non-JSON response from client port {} for cmd `{}`: `{}` (err: {})",
            client_port, cmd, s, e
        )
    })
}

/// RAII guard that runs `bootstrap-cluster.sh stop` + `clean`
/// when dropped, regardless of whether the test panicked.
/// Without this, a failed assertion (or an unexpected panic)
/// leaves the 3 oxide-kv child processes running, which then
/// bind the same ports on the next `cargo test` invocation
/// and cause EADDRINUSE / "Connection refused" flakes. The
/// guard owns the TempDir so the data dirs are also removed.
struct ClusterGuard {
    base: PathBuf,
    _tmp: TempDir,
}

impl Drop for ClusterGuard {
    fn drop(&mut self) {
        // Best-effort: never panic during Drop.
        let script = workspace_root().join("deploy/scripts/bootstrap-cluster.sh");
        let _ = Command::new("bash")
            .arg(&script)
            .arg("stop")
            .env("BASE", &self.base)
            .output();
        let _ = Command::new("bash")
            .arg(&script)
            .arg("clean")
            .env("BASE", &self.base)
            .output();
        // Belt + suspenders: nuke any stragglers. `pkill -f` is
        // intentionally broad — this only runs in test contexts
        // and the test owns its own data dir.
        let _ = Command::new("pkill")
            .arg("-9")
            .arg("-f")
            .arg("oxide-kv --addr")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Run the full smoke sequence: start cluster, hand the records
/// to the test body, then tear down. The ClusterGuard ensures
/// teardown runs even if the test body panics.
fn run_smoke<F: FnOnce(&[NodeRecord])>(assertions: F) {
    let tmp = TempDir::new().expect("tempdir");
    let base = tmp.path().to_path_buf();
    let _guard = ClusterGuard {
        base: base.clone(),
        _tmp: tmp,
    };
    start_cluster(&base);
    let records = read_cluster(&base);
    assert_eq!(
        records.len(),
        3,
        "expected 3 nodes in cluster.jsonl, got {}",
        records.len()
    );
    // 30s is generous; even on a cold CI runner, oxide-kv
    // binds all 6 ports within 2-3 seconds. The slack absorbs
    // a snapshot-replay delay or a slow data-dir restore.
    wait_for_ports(&records, Duration::from_secs(30));
    assertions(&records);
    // _guard drops here and runs the teardown.
}

// =========================================================================
// Active tests (regression guard for the deploy path itself)
// =========================================================================

#[test]
fn metrics_endpoint_on_all_three_nodes_responds_200() {
    run_smoke(|records| {
        for rec in records {
            let url = format!("http://127.0.0.1:{}/metrics", rec.metrics);
            let body = http_get(&url, Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("scrape {}: {}", url, e));
            // Pin a couple of metric names — the regression
            // guard for "someone refactored the metrics module
            // name".
            assert!(
                body.contains("oxide_raft_role"),
                "node {}: missing oxide_raft_role in /metrics: {}",
                rec.node,
                body.lines().take(3).collect::<Vec<_>>().join("\n")
            );
            assert!(
                body.contains("oxide_raft_term"),
                "node {}: missing oxide_raft_term in /metrics",
                rec.node
            );
        }
    });
}

#[test]
fn client_port_serves_json_protocol() {
    // The line-delimited JSON client protocol must work on every
    // node's client port, regardless of whether the node is the
    // current leader. A non-leader node replies with
    // `{"error":"Not a leader..."}`, which is itself proof that
    // the protocol round-trips correctly. A leader node replies
    // with `{"status":"ok",...}` or an empty value, depending on
    // the command. Either response is a valid JSON object, which
    // is what we assert here.
    run_smoke(|records| {
        for rec in records {
            let resp = issue_command(
                rec.client,
                r#"{"Get":{"key":"smoke-probe"}}"#,
                Duration::from_secs(2),
            );
            let obj = resp.as_object()
                .unwrap_or_else(|| panic!(
                    "node {}: response is not a JSON object: {}",
                    rec.node, resp
                ));
            // Must be either a success object or an error
            // object — never an empty body or a parse error.
            let is_success = obj.contains_key("status")
                || obj.contains_key("data");
            let is_error = obj.contains_key("error");
            assert!(
                is_success || is_error,
                "node {}: response has neither status/data nor error: {}",
                rec.node,
                resp
            );
        }
    });
}

#[test]
fn logs_show_election_completed_within_timeout() {
    // The bootstrap cluster reaches *some* leader election
    // within 10 seconds — we don't require a stable single
    // leader (see module docstring on the pre-vote tie bug),
    // just evidence that election machinery ran. We assert by
    // counting "Leader" / "PreVote" / "Vote" log lines across
    // all three node logs.
    run_smoke(|records| {
        std::thread::sleep(Duration::from_secs(10));
        let mut election_events: u32 = 0;
        for rec in records {
            let log = std::fs::read_to_string(&rec.log)
                .unwrap_or_else(|e| panic!("read log {}: {}", rec.log, e));
            // Election activity is logged with one of these
            // prefixes. We don't pin the exact wording (the log
            // format has been refactored several times in P8);
            // we just want to know *something* happened.
            for line in log.lines() {
                if line.contains("Leader")
                    || line.contains("PreVote")
                    || line.contains("pre-vote")
                    || line.contains("Vote")
                {
                    election_events += 1;
                }
            }
        }
        assert!(
            election_events >= 3,
            "expected at least 3 election-related log lines across all 3 nodes, got {}",
            election_events
        );
    });
}

// =========================================================================
// Gated tests (consensus-level invariants; off until follow-up PRs land)
// =========================================================================
//
// The pre-vote tie / split-brain bug (and the multi-node
// commit-advance bug) are real, reproducible defects that this
// smoke test surfaces. The active tests above verify the
// *infrastructure* (deploy path, metrics, JSON protocol,
// election machinery); the gated tests below verify the
// *consensus invariants* a healthy cluster should hold. Remove
// the `#[ignore]` attribute when the corresponding follow-up
// PR merges. The test bodies are written to *fail* today if
// you remove the ignore — they're not commented out, they're
// honest about the gap.

/// Single-stable-leader convergence. Regression guard for the
/// pre-vote tie / split-brain bug fixed in PR #51.
#[test]
fn single_leader_converges_within_15_seconds() {
    run_smoke(|records| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let mut role2_count = 0;
            for rec in records {
                let url = format!("http://127.0.0.1:{}/metrics", rec.metrics);
                if let Ok(body) = http_get(&url, Duration::from_secs(1)) {
                    let role = parse_metric(&body, "oxide_raft_role").unwrap_or(-1);
                    if role == 2 {
                        role2_count += 1;
                    }
                }
            }
            if role2_count == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "no single stable leader within 15s; pre-vote tie / split-brain still present"
        );
    });
}

/// Multi-node commit advance after a successful Set. Regression
/// guard for the commit-advance bug fixed in PR #50.
#[test]
fn commit_index_advances_after_set_on_cluster() {
    run_smoke(|records| {
        // Find the leader — same retry loop as the active
        // Set test, but here we also need to wait for the
        // post-Set commit to replicate.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut leader_client: Option<u16> = None;
        while Instant::now() < deadline {
            for rec in records {
                let url = format!("http://127.0.0.1:{}/metrics", rec.metrics);
                if let Ok(body) = http_get(&url, Duration::from_secs(1)) {
                    let role = parse_metric(&body, "oxide_raft_role").unwrap_or(-1);
                    if role == 2 {
                        leader_client = Some(rec.client);
                        break;
                    }
                }
            }
            if leader_client.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let leader_client = leader_client.expect("no leader within 15s");
        let resp = issue_command(
            leader_client,
            r#"{"Set":{"key":"k1","value":"v1"}}"#,
            Duration::from_secs(5),
        );
        assert_eq!(
            resp.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "Set failed: {}",
            resp
        );
        std::thread::sleep(Duration::from_secs(2));
        let mut any_committed = false;
        for rec in records {
            let body = http_get(
                &format!("http://127.0.0.1:{}/metrics", rec.metrics),
                Duration::from_secs(2),
            )
            .unwrap();
            let commit = parse_metric(&body, "oxide_raft_commit_index")
                .unwrap_or(-1);
            let role = parse_metric(&body, "oxide_raft_role").unwrap_or(-1);
            eprintln!(
                "node {}: role={} commit_index={}",
                rec.node, role, commit
            );
            if commit >= 1 {
                any_committed = true;
            }
        }
        assert!(
            any_committed,
            "no node reports commit_index >= 1 after a successful Set"
        );
    });
}

/// Set + Get round trip on the same leader client port.
/// Regression guard for the pre-vote tie / split-brain bug
/// fixed in PR #51 (requires a stable leader for both ops).
#[test]
fn set_then_get_on_leader_returns_written_value() {
    run_smoke(|records| {
        // Find the current leader (retry loop handles churn).
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut leader_client: Option<u16> = None;
        while Instant::now() < deadline {
            for rec in records {
                let url = format!("http://127.0.0.1:{}/metrics", rec.metrics);
                if let Ok(body) = http_get(&url, Duration::from_secs(1)) {
                    let role = parse_metric(&body, "oxide_raft_role").unwrap_or(-1);
                    if role == 2 {
                        leader_client = Some(rec.client);
                        break;
                    }
                }
            }
            if leader_client.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let leader_client = leader_client.expect("no leader within 15s");
        let set_resp = issue_command(
            leader_client,
            r#"{"Set":{"key":"k","value":"v"}}"#,
            Duration::from_secs(5),
        );
        assert_eq!(
            set_resp.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "Set failed: {}",
            set_resp
        );
        std::thread::sleep(Duration::from_secs(2));
        // Re-find the leader for the Get (it may have rotated).
        let mut get_leader: Option<u16> = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            for rec in records {
                let url = format!("http://127.0.0.1:{}/metrics", rec.metrics);
                if let Ok(body) = http_get(&url, Duration::from_secs(1)) {
                    let role = parse_metric(&body, "oxide_raft_role").unwrap_or(-1);
                    if role == 2 {
                        get_leader = Some(rec.client);
                        break;
                    }
                }
            }
            if get_leader.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let get_leader = get_leader.expect("no leader within 15s for Get");
        let get_resp = issue_command(
            get_leader,
            r#"{"Get":{"key":"k"}}"#,
            Duration::from_secs(5),
        );
        assert_eq!(
            get_resp.get("data").and_then(|v| v.as_str()),
            Some("v"),
            "Get returned wrong value: {}",
            get_resp
        );
    });
}