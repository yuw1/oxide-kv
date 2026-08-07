//! Integration tests for the `/metrics` HTTP endpoint (P8 PR 8).
//!
//! Drives the `observability::server::handle_connection` function
//! directly through ephemeral TCP sockets — no full node boot, no
//! line-delimited JSON client. The full client-server round-trip
//! is covered by the existing `tests/integration_2pc.rs` and
//! `tests/tx_timeout_abort.rs` suites; here we just want to pin
//! the wire shape of `/metrics` so future refactors can't
//! silently break Prometheus scraping.
//!
//! Three scenarios:
//!   1. `GET /metrics` returns 200 + valid Prometheus text
//!      containing every pre-registered gauge / counter name.
//!   2. `GET /health` returns 200 + "ok" body (cheap liveness
//!      probe for pod-to-pod checks).
//!   3. `GET /something-else` returns 404 (no surprise routes).
//!
//! All tests bind ephemeral ports (`127.0.0.1:0`) so they can run
//! in parallel with the rest of the integration suite.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use oxide_kv::observability::Metrics;
use oxide_kv::observability::server::handle_connection;
use oxide_kv::raft::net::StopSignal;

async fn drive_one_request(metrics_port: u16, request: &[u8]) -> String {
    let metrics =
        Metrics::with_peers(&["127.0.0.1:9002".to_string(), "127.0.0.1:9003".to_string()])
            .expect("registry");
    // Seed a couple of gauges so the response carries real
    // values (not just pre-registered zeros).
    metrics.raft_term.set(13);
    metrics.raft_role.set(2);
    metrics.tx_admin_aborted_total.inc_by(7);

    let stop = StopSignal::new();
    let m = metrics.clone();
    let s = stop.clone();
    let metrics_addr = format!("127.0.0.1:{}", metrics_port);
    // Bind the actual metrics port the test will hit.
    let metrics_listener = TcpListener::bind(&metrics_addr).await.unwrap();
    let serve = tokio::spawn(async move {
        // Single-request pattern: accept once, handle, exit. This
        // mirrors how the lib test in `observability::server::tests`
        // exercises the handler.
        if let Ok((stream, _)) = metrics_listener.accept().await {
            let _ = handle_connection(stream, m, s).await;
        }
    });

    // Connect to the SAME port the metrics server is listening on.
    let mut client = TcpStream::connect(("127.0.0.1", metrics_port))
        .await
        .expect("connect metrics port");
    client.write_all(request).await.unwrap();
    client.shutdown().await.ok(); // signal EOF to server's read loop
    let mut resp = Vec::new();
    // Bound the read with a timeout so a hung server doesn't hang
    // the test forever.
    let read = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut resp)).await;
    let _ = read.expect("server read timeout").expect("read");

    stop.stop();
    let _ = serve.await;
    String::from_utf8(resp).expect("utf-8")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_serves_prometheus_text() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let metrics_port = listener.local_addr().unwrap().port();
    drop(listener);

    let s = drive_one_request(metrics_port, b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;

    assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "status: {}", s);
    assert!(
        s.contains("Content-Type: text/plain"),
        "Content-Type missing: {}",
        s
    );
    // Spot-check every registered metric name.
    for name in [
        "oxide_raft_term",
        "oxide_raft_commit_index",
        "oxide_raft_last_applied",
        "oxide_raft_log_length",
        "oxide_raft_role",
        "oxide_raft_snapshot_age_seconds",
        "oxide_raft_snapshot_bytes",
        "oxide_tx_pending_count",
        "oxide_tx_timeout_aborted_total",
        "oxide_tx_admin_aborted_total",
        "oxide_peer_match_index",
        "oxide_peer_next_index",
    ] {
        assert!(s.contains(name), "missing {} in response", name);
    }
    // Pinned values from drive_one_request.
    assert!(s.contains("oxide_raft_term 13"), "term value: {}", s);
    assert!(s.contains("oxide_raft_role 2"), "role value: {}", s);
    assert!(
        s.contains("oxide_tx_admin_aborted_total 7"),
        "admin aborted: {}",
        s
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_health_returns_ok() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let metrics_port = listener.local_addr().unwrap().port();
    drop(listener);

    let s = drive_one_request(metrics_port, b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "got: {}", s);
    assert!(s.ends_with("ok\n"), "body: {}", s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_unknown_path_returns_404() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let metrics_port = listener.local_addr().unwrap().port();
    drop(listener);

    let s = drive_one_request(
        metrics_port,
        b"GET /not-metrics HTTP/1.1\r\nHost: x\r\n\r\n",
    )
    .await;
    assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"), "got: {}", s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_post_returns_404() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let metrics_port = listener.local_addr().unwrap().port();
    drop(listener);

    // POST is not a method we support. Only GETs. Pin this so a
    // future refactor that mistakenly switches on `path` alone
    // doesn't accidentally allow POST.
    let s = drive_one_request(
        metrics_port,
        b"POST /metrics HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"), "got: {}", s);
}
