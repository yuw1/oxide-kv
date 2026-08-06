#!/usr/bin/env python3
"""oxide_kv_demo — run a handful of raw KV + 2PC ops against a leader.

Start a single-node server in another terminal:

    cargo run --release -- --addr 127.0.0.1:9001 --client-addr 127.0.0.1:9101

Then run this from the python/ directory (or anywhere on sys.path
once you've run `pip install -e .`):

    python3 python/examples/oxide_kv_demo.py

Override the endpoint via OXIDE_KV_HOST / OXIDE_KV_PORT env vars for
multi-node setups (the demo picks the first reachable leader).
"""

from __future__ import annotations

import os
import sys

# Make the in-tree SDK importable without `pip install`. The demo
# lives at python/examples/, the SDK package is a sibling at
# python/oxide_kv/, so the parent of __file__ is python/.
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from oxide_kv import Client, TxAbortedError  # noqa: E402


def main() -> int:
    host = os.environ.get("OXIDE_KV_HOST", "127.0.0.1")
    port = int(os.environ.get("OXIDE_KV_PORT", "9101"))

    print(f"→ connecting to {host}:{port}")
    with Client.connect(host, port) as c:
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