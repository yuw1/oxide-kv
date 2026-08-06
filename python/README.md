# oxide-kv (Python SDK)

A minimal Python client for Oxide-KV. Talks the JSON line protocol to a
leader node over a plain TCP socket. **Zero external dependencies** —
stdlib only, so `pip install` doesn't drag in `protobuf` / `aiohttp` /
anything else.

The wire protocol is documented inline in `oxide_kv/client.py`; it
matches the server side in `rust/oxide-kv/src/client.rs`. See
the top-level [README](../README.md) for the matching Rust server.

## Install (in-tree development)

```bash
# Pick any writable prefix — the SDK is a pure Python package.
pip install -e .
```

For a release, this directory will publish a wheel to PyPI; the same
install line works.

## Quick start

```python
from oxide_kv import Client

# Connect to a single-node server (point at any node; the leader
# answers, followers return a NotLeaderError on mutation).
c = Client.connect("127.0.0.1", 9101)

# Or scan a list of (host, port) tuples and pick the first leader.
c = Client.discover([("127.0.0.1", 9101), ("127.0.0.1", 9102), ("127.0.0.1", 9103)])

# Raw KV.
c.set("hello", "world")          # → int (log index)
print(c.get("hello"))            # → "world"
print(c.get("missing"))          # → None
c.delete("hello")                # → int (log index)

# 2PC transaction (chainable).
with c.begin_tx("batch-1") as tx:
    tx.set("a", "1").delete("b")
    result = tx.commit()         # → TxResult(decision, begin_index, decide_index)
    print(result.decision)        # "commit"
```

## API surface

| Symbol | Purpose |
|---|---|
| `Client.connect(host, port)` | Open a single-node connection |
| `Client.discover([h, p, ...])` | Scan a list, return the first leader |
| `Client.set(key, value) → int` | Propose a Set; returns log index |
| `Client.get(key) → str \| None` | Linearizable read; `None` if missing |
| `Client.delete(key) → int` | Propose a Delete; returns log index |
| `Client.begin_tx(tx_id) → Transaction` | Buffered 2PC transaction |
| `tx.set(k, v) / tx.delete(k)` | Stage an op; returns `self` for chaining |
| `tx.commit() → TxResult` | Submit the BeginTx + drive DecideTx(Commit) |
| `tx.abort()` | Send DecideTx(Commit=false) |
| `OxideKVError` | Base exception |
| `NotLeaderError` | The contacted node isn't the leader |
| `TxAbortedError` | The coordinator aborted with the given reason |

## Tests

The test suite (`tests/test_client.py`) needs a running server on
`127.0.0.1:9101` (override with `OXIDE_KV_TEST_HOST` /
`OXIDE_KV_TEST_PORT`). Start one in another terminal:

```bash
cargo run --release -- \
  --addr 127.0.0.1:9001 \
  --client-addr 127.0.0.1:9101
```

Then:

```bash
make install
make test
```

Tests skip cleanly if the server isn't reachable — so CI doesn't
hard-fail on hosts without a running node.

## Threading

`Client` is **not thread-safe** — it owns a single socket. Use one
client per thread, or wrap calls in a `threading.Lock`. The server's
client handler is a single async task, so two requests on the same
connection without a reply in between would race anyway.

## Design choices

- **Blocking I/O**, not `asyncio`. The server is internal-network,
  low-QPS, and asyncio adds a learning hurdle without a measurable
  benefit. A future async variant can land alongside this one.
- **Plain TCP**, not TLS. Internal network only; layer TLS at a
  reverse proxy if you need it.
- **Stdlib only**. No `protobuf`, no `aiohttp`, no `requests`. Keep
  the wire contract visible.
- **No reconnect / retry**. The caller decides — fail-fast is easier
  to reason about. Wrap your own retry loop if you need one.