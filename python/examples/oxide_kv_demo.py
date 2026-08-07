#!/usr/bin/env python3
"""oxide_kv_demo — run a handful of raw KV + 2PC ops against a leader.

Start a cluster in another terminal — either the one-liner helper:

    ./deploy/scripts/bootstrap-cluster.sh start   # 3 nodes, client ports 9101-9103

or a single node by hand:

    cargo run --release -- --addr 127.0.0.1:9001 --client-addr 127.0.0.1:9101

Then run this from the repo root (or anywhere on sys.path once
you've run `pip install -e .`):

    python3 python/examples/oxide_kv_demo.py

Endpoint configuration (env vars, checked in this order):

  OXIDE_KV_ENDPOINTS  Comma-separated host:port list, e.g.
                      "127.0.0.1:9101,127.0.0.1:9102,127.0.0.1:9103".
                      With more than one endpoint the demo uses
                      Client.discover() to find the current leader
                      automatically — no need to know which node won
                      the election.
  OXIDE_KV_HOST       Single-node override (default 127.0.0.1).
  OXIDE_KV_PORT       Single-node override (default 9101).

Default: the three client ports of bootstrap-cluster.sh
(127.0.0.1:9101..9103), discovered via Client.discover().
"""

from __future__ import annotations

import os
import sys

# Make the in-tree SDK importable without `pip install`. The demo
# lives at python/examples/, the SDK package is a sibling at
# python/oxide_kv/, so the parent of __file__ is python/.
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from oxide_kv import Client, OxideKVError, TxAbortedError  # noqa: E402

# Client ports of `deploy/scripts/bootstrap-cluster.sh`:
# node-N listens on 9100 + N.
DEFAULT_ENDPOINTS = [
    ("127.0.0.1", 9101),
    ("127.0.0.1", 9102),
    ("127.0.0.1", 9103),
]


def resolve_endpoints() -> list:
    """Parse env overrides into a list of (host, port) tuples."""
    raw = os.environ.get("OXIDE_KV_ENDPOINTS", "").strip()
    if raw:
        endpoints = []
        for item in raw.split(","):
            host, _, port = item.strip().rpartition(":")
            if not host or not port:
                raise SystemExit(f"bad OXIDE_KV_ENDPOINTS entry: {item!r} (want host:port)")
            endpoints.append((host, int(port)))
        return endpoints
    # Single-node overrides stay for the hand-rolled server case.
    if "OXIDE_KV_HOST" in os.environ or "OXIDE_KV_PORT" in os.environ:
        host = os.environ.get("OXIDE_KV_HOST", "127.0.0.1")
        port = int(os.environ.get("OXIDE_KV_PORT", "9101"))
        return [(host, port)]
    return DEFAULT_ENDPOINTS


def main() -> int:
    endpoints = resolve_endpoints()

    if len(endpoints) > 1:
        print(f"→ discovering leader among {endpoints}")
        try:
            c = Client.discover(endpoints)
        except OxideKVError as e:
            raise SystemExit(
                f"✗ {e}\n  hint: is the cluster up? "
                f"Try `./deploy/scripts/bootstrap-cluster.sh start` "
                f"and check the leader with `... status`."
            )
    else:
        host, port = endpoints[0]
        print(f"→ connecting to {host}:{port}")
        try:
            c = Client.connect(host, port)
        except OSError as e:
            raise SystemExit(f"✗ cannot reach {host}:{port}: {e}")
    print(f"✓ connected to leader at {c._conn.host}:{c._conn.port}")

    with c:
        # Raw KV
        idx = c.set("hello", "world")
        print(f"  set hello=world → index={idx}")
        print(f"  get hello → {c.get('hello')!r}")
        c.delete("hello")
        print(f"  delete hello → get → {c.get('hello')!r}")

        # 2PC transaction: chain two ops + commit.
        tx_id = "demo-tx-1"
        with c.begin_tx(tx_id) as tx:
            tx.set("a", "1").delete("never-existed")
            result = tx.commit()
            print(
                f"  tx {tx_id} commit → decision={result.decision}, "
                f"begin={result.begin_index}, decide={result.decide_index}"
            )
        print(f"  get a → {c.get('a')!r}")

        # 2PC abort: demonstrate TxAbortedError mapping on a deliberately
        # conflicting tx (depends on cluster policy — kept as a hint).
        try:
            with c.begin_tx("demo-tx-abort") as tx:
                tx.set("a", "overwrite-and-abort")
                tx.abort()
        except TxAbortedError as e:
            print(f"  tx abort raised (expected if cluster aborts): {e}")

    print("✓ done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())