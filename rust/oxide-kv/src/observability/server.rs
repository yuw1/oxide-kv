//! `/metrics` HTTP server (P8 PR 8).
//!
//! Minimal HTTP/1.1 reader built on `tokio::net::TcpListener`.
//! Serves one route: `GET /metrics` returning the Prometheus
//! text-format body. Every other path / method returns `404`.
//!
//! Why hand-rolled rather than `hyper` / `axum`?
//!
//! - The route surface is one line. Pulling in a full HTTP
//!   framework for one GET request is overkill.
//! - Avoids transitive `hyper` deps (`bytes`, `futures-util`,
//!   `http`, `http-body`, `tokio-util` ...). A 1-route server is
//!   ~100 LOC; the dep tree would add 10+ crates.
//! - The wire format is fixed; we don't need middleware,
//!   routing, content negotiation, or anything else.
//!
//! Caveats (intentional):
//!
//! - No keep-alive. The scrape is short-lived anyway, and
//!   `Connection: close` keeps the read loop simple.
//! - No TLS. The deployment guide (P8 PR 4 / #36) already
//!   mandates a TLS-terminating reverse proxy on the metrics
//!   port; serving plaintext behind it is the standard pattern.
//! - No authentication. Same rationale — Prometheus scrapes from
//!   a private network; auth belongs on the proxy.
//! - Per-request buffers are bounded at 8 KiB. A malformed
//!   request longer than that is dropped, which is fine for a
//!   metrics endpoint.

use crate::observability::MetricsHandle;
use crate::raft::net::StopSignal;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

/// How often the per-connection loop re-accepts after a clean
/// shutdown. There isn't a tight bound here — 50 ms keeps the
/// shutdown latency low without burning CPU.
const ACCEPT_LOOP_RECHECK_MS: u64 = 50;

/// Spawn the `/metrics` HTTP server on `addr`. Blocks until
/// `stop` fires, then drains in-flight requests and returns.
///
/// Errors binding the listener are returned synchronously;
/// per-connection errors are logged via `eprintln` and the
/// connection is dropped.
pub async fn run_metrics_server(
    metrics: MetricsHandle,
    addr: String,
    stop: StopSignal,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    info!(endpoint = %addr, "metrics endpoint listening");
    loop {
        tokio::select! {
            biased;
            _ = stop.0.notified() => {
                info!("metrics stop signal received; draining listener");
                return Ok(());
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let m = metrics.clone();
                        let stop = stop.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, m, stop).await {
                                warn!(error = %e, "metrics connection error");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "metrics accept error");
                        // Brief backoff so we don't busy-loop on a
                        // pathological accept failure.
                        tokio::time::sleep(std::time::Duration::from_millis(
                            ACCEPT_LOOP_RECHECK_MS,
                        ))
                        .await;
                    }
                }
            }
        }
    }
}

pub async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    metrics: MetricsHandle,
    stop: StopSignal,
) -> std::io::Result<()> {
    // Read the request head. 8 KiB is comfortably above any
    // reasonable `GET /metrics` request (Prometheus sends a
    // short header block).
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    let header_end;

    loop {
        // Stop-aware read so a mid-request shutdown doesn't hang.
        tokio::select! {
            biased;
            _ = stop.0.notified() => {
                // Best-effort close; ignore errors.
                let _ = stream.shutdown().await;
                return Ok(());
            }
            n = stream.read(&mut buf[total..]) => {
                let n = n?;
                if n == 0 {
                    // EOF before request completed; drop.
                    return Ok(());
                }
                total += n;
                if let Some(pos) = find_double_crlf(&buf[..total]) {
                    header_end = pos + 4;
                    break;
                }
                if total == buf.len() {
                    // Pathological request — header > 8 KiB.
                    return Ok(());
                }
            }
        }
    }

    let request_head = std::str::from_utf8(&buf[..header_end])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Parse method + path. We only support `GET /metrics`.
    let mut lines = request_head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let response = if method == "GET" && (path == "/metrics" || path.starts_with("/metrics?")) {
        match metrics.gather_text() {
            Ok(body) => http_response(200, "text/plain; version=0.0.4; charset=utf-8", &body),
            Err(e) => http_response(
                500,
                "text/plain; charset=utf-8",
                format!("metrics encode failed: {}", e).as_bytes(),
            ),
        }
    } else if method == "GET" && (path == "/health" || path.starts_with("/health?")) {
        // Bonus: a cheap liveness probe. Returns `ok` as
        // text/plain so a curl from any pod-to-pod liveness
        // probe gets a sane response without a Prometheus
        // dependency.
        http_response(200, "text/plain; charset=utf-8", b"ok\n")
    } else {
        http_response(404, "text/plain; charset=utf-8", b"not found\n")
    };

    stream.write_all(&response).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Build an HTTP/1.1 response with the given status, content type,
/// and body. Always `Connection: close` — see module doc.
fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut out = Vec::with_capacity(128 + body.len());
    use std::io::Write as _;
    let _ = write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        status = status,
        reason = reason,
        ct = content_type,
        len = body.len(),
    );
    out.extend_from_slice(body);
    out
}

/// Find the `\r\n\r\n` that ends an HTTP request head. Returns
/// the index of the first `\r` so the caller can advance by `+4`.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::Metrics;

    #[test]
    fn http_response_formats_status_line_and_headers() {
        let body = b"hello world";
        let resp = http_response(200, "text/plain", body);
        let s = std::str::from_utf8(&resp).expect("utf-8");
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "status line: {}", s);
        assert!(s.contains("Content-Type: text/plain\r\n"), "ct: {}", s);
        assert!(s.contains("Content-Length: 11\r\n"), "len: {}", s);
        assert!(s.contains("Connection: close\r\n"), "conn: {}", s);
        assert!(s.ends_with("hello world"), "body: {}", s);
    }

    #[test]
    fn http_response_handles_404() {
        let resp = http_response(404, "text/plain", b"not found\n");
        let s = std::str::from_utf8(&resp).expect("utf-8");
        assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"), "status: {}", s);
        assert!(s.contains("Content-Length: 10\r\n"), "len: {}", s);
    }

    #[test]
    fn find_double_crlf_finds_terminator() {
        let buf = b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\nbody";
        assert_eq!(find_double_crlf(buf), Some(buf.len() - b"body".len() - 4));
    }

    #[test]
    fn find_double_crlf_returns_none_when_missing() {
        let buf = b"GET /metrics HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(find_double_crlf(buf), None);
    }

    #[test]
    fn metrics_endpoint_round_trip() {
        // End-to-end: bind on an ephemeral port, GET /metrics,
        // assert the response body is a valid Prometheus text
        // exposition containing the pre-seeded gauges.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let metrics = Metrics::new().expect("registry");
            metrics.raft_term.set(13);
            metrics.raft_role.set(2);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = StopSignal::new();
            let m = metrics.clone();
            let s = stop.clone();
            let server = tokio::spawn(async move {
                // Run a single accept + handle, then return.
                if let Ok((stream, _)) = listener.accept().await {
                    let _ = handle_connection(stream, m, s).await;
                }
            });

            // Client side: connect, write a request, read response.
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client
                .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut resp = Vec::new();
            client.read_to_end(&mut resp).await.unwrap();
            let s = std::str::from_utf8(&resp).expect("utf-8");
            assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "status: {}", s);
            assert!(s.contains("oxide_raft_term 13"), "term: {}", s);
            assert!(s.contains("oxide_raft_role 2"), "role: {}", s);

            stop.stop();
            let _ = server.await;
        });
    }

    #[test]
    fn unknown_path_returns_404() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let metrics = Metrics::new().expect("registry");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = StopSignal::new();
            let m = metrics.clone();
            let s = stop.clone();
            let server = tokio::spawn(async move {
                if let Ok((stream, _)) = listener.accept().await {
                    let _ = handle_connection(stream, m, s).await;
                }
            });

            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client
                .write_all(b"GET /not-metrics HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut resp = Vec::new();
            client.read_to_end(&mut resp).await.unwrap();
            let s = std::str::from_utf8(&resp).expect("utf-8");
            assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"), "status: {}", s);

            stop.stop();
            let _ = server.await;
        });
    }
}