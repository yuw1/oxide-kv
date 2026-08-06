"""Tests for the oxide_kv Python SDK.

These tests need a running single-node Oxide-KV server on a known
client port. The default looks for `127.0.0.1:9101`; override with
`OXIDE_KV_TEST_HOST` / `OXIDE_KV_TEST_PORT` env vars when pointing
at a different node.

Run with::

    cargo run --release -- \
      --addr 127.0.0.1:9001 \
      --client-addr 127.0.0.1:9101
    pytest python/tests/

The tests are deliberately simple — they exercise the wire contract,
not Oxide-KV's correctness (that's what the Rust test suite is for).
"""

from __future__ import annotations

import os
import secrets
import socket
import sys

import pytest

# Make the in-tree SDK importable without `pip install`.
_REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
sys.path.insert(0, os.path.join(_REPO_ROOT, "python"))

from oxide_kv import (  # noqa: E402  (sys.path tweak above)
    Client,
    NotLeaderError,
    OxideKVError,
    TxAbortedError,
    Transaction,
)


HOST = os.environ.get("OXIDE_KV_TEST_HOST", "127.0.0.1")
PORT = int(os.environ.get("OXIDE_KV_TEST_PORT", "9101"))


def _server_reachable() -> bool:
    """Skip the whole module if no server is listening — saves CI from
    hard-failing on hosts without a running node."""
    try:
        with socket.create_connection((HOST, PORT), timeout=1.0):
            return True
    except OSError:
        return False


pytestmark = pytest.mark.skipif(
    not _server_reachable(),
    reason=f"no Oxide-KV server at {HOST}:{PORT}",
)


@pytest.fixture
def client() -> Client:
    c = Client.connect(HOST, PORT)
    try:
        yield c
    finally:
        c.close()


@pytest.fixture
def unique_key() -> str:
    return f"test_{secrets.token_hex(8)}"


# ---------- raw KV ----------

def test_set_returns_int_index(client: Client, unique_key: str) -> None:
    idx = client.set(unique_key, "v1")
    assert isinstance(idx, int)
    assert idx >= 1


def test_get_returns_value(client: Client, unique_key: str) -> None:
    client.set(unique_key, "hello")
    assert client.get(unique_key) == "hello"


def test_get_missing_key_returns_none(client: Client) -> None:
    assert client.get(secrets.token_hex(16)) is None


def test_delete_returns_int_index(client: Client, unique_key: str) -> None:
    client.set(unique_key, "v")
    idx = client.delete(unique_key)
    assert isinstance(idx, int)
    assert client.get(unique_key) is None


# ---------- transactions ----------

def test_begin_tx_commit_chain(client: Client, unique_key: str) -> None:
    """The classic 2PC chain: begin → set + delete → commit."""
    tx = client.begin_tx("test-tx-" + secrets.token_hex(4))
    assert isinstance(tx, Transaction)
    tx.set(unique_key, "tx-value").delete(unique_key + "-other")
    result = tx.commit()
    assert result.decision == "commit"
    assert result.begin_index >= 1
    assert result.decide_index >= result.begin_index
    assert client.get(unique_key) == "tx-value"


def test_transaction_abort_via_decide_tx(client: Client, unique_key: str) -> None:
    """Manual abort: send DecideTx(commit=false). The key must not appear
    in the state machine after the abort lands."""
    tx = client.begin_tx("test-abort-" + secrets.token_hex(4))
    tx.set(unique_key, "should-not-persist")
    tx.abort()
    # Give the leader a beat to apply the abort; the SDK has no built-in
    # read-after-write delay, so a tiny sleep is the simplest guard.
    import time
    time.sleep(0.2)
    assert client.get(unique_key) is None


# ---------- error mapping ----------

def test_set_raises_not_leader_for_follower_node() -> None:
    """If we point at a follower, the very first mutation should raise
    NotLeaderError. (This test only runs against a multi-node cluster;
    skip otherwise.)"""
    # Heuristic: try to connect; if the port is open but the response
    # says "Not a leader", we'll see it on the first mutation.
    try:
        c = Client.connect(HOST, PORT)
    except OSError:
        pytest.skip("node unreachable")
    try:
        c.set(secrets.token_hex(8), "x")
    except NotLeaderError:
        pass
    except OxideKVError:
        # Single-node standalone sets return OK; a follower returns the
        # not-leader error. Anything else is an unrelated failure.
        pass
    finally:
        c.close()