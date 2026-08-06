"""
oxide_kv.client — Python SDK for Oxide-KV.

Talks the JSON line protocol to a leader node over a plain TCP socket.
Designed for low-throughput / internal-network use (the same audience
as the Rust server). 0 external dependencies — stdlib only.

Wire format (matches `src/client.rs`):
    Request :  one JSON object per line, terminated by '\\n'
    Response:  one JSON object per line, terminated by '\\n'

Example:
    >>> c = Client.connect([("127.0.0.1", 9101), ("127.0.0.1", 9102)])
    >>> c.set("hello", "world")
    1
    >>> c.get("hello")
    'world'
    >>> c.delete("hello")
    2
    >>> with c.begin_tx("t-1") as tx:
    ...     tx.set("a", "1").delete("b")
    ...     tx.commit()
    TxResult(decision='commit', begin_index=3, decide_index=4)
"""

from __future__ import annotations

import json
import socket
from contextlib import contextmanager
from dataclasses import dataclass, field
from typing import Iterator, List, Optional, Tuple, Union


# Public API surface
__all__ = [
    "Client",
    "Transaction",
    "TxResult",
    "OxideKVError",
    "NotLeaderError",
    "TxAbortedError",
]


# ---------- exceptions ----------

class OxideKVError(Exception):
    """Base class for all SDK-side errors."""


class NotLeaderError(OxideKVError):
    """The contacted node is not the current leader."""


class TxAbortedError(OxideKVError):
    """The coordinator aborted the transaction with the given reason."""

    def __init__(self, tx_id: str, reason: str) -> None:
        super().__init__(f"tx {tx_id!r} aborted: {reason}")
        self.tx_id = tx_id
        self.reason = reason


# ---------- result types ----------

@dataclass
class TxResult:
    """Outcome of a committed transaction."""
    decision: str          # "commit"
    begin_index: int
    decide_index: int
    tx_id: str


# ---------- core client ----------

class _Connection:
    """A single TCP connection speaking the JSON line protocol.

    The connection is **not thread-safe** — wrap with a lock or open a
    new `Client` per thread. The server's `ClientHandler` is a single
    async task, so two requests on the same connection without a reply
    in between would race.
    """

    def __init__(self, host: str, port: int, timeout: float = 5.0) -> None:
        self.host = host
        self.port = port
        self.timeout = timeout
        self._sock: Optional[socket.socket] = None
        self._buf: str = ""

    def __enter__(self) -> "_Connection":
        self._sock = socket.create_connection(
            (self.host, self.port), timeout=self.timeout
        )
        self._buf = ""
        return self

    def __exit__(self, *exc) -> None:
        if self._sock is not None:
            try:
                self._sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self._sock.close()
            self._sock = None

    def _send(self, payload: dict) -> None:
        assert self._sock is not None, "Connection not open"
        line = json.dumps(payload, separators=(",", ":")) + "\n"
        self._sock.sendall(line.encode("utf-8"))

    def _recv(self) -> dict:
        assert self._sock is not None, "Connection not open"
        while "\n" not in self._buf:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise ConnectionError(f"server {self.host}:{self.port} closed the connection")
            self._buf += chunk.decode("utf-8", errors="replace")
        line, self._buf = self._buf.split("\n", 1)
        return json.loads(line)

    def request(self, payload: dict) -> dict:
        """Send one request and return the decoded JSON response."""
        self._send(payload)
        return self._recv()


# ---------- transactions ----------

class Transaction:
    """Build a 2PC transaction op-by-op, then commit or abort.

    Returned by `Client.begin_tx(tx_id)`. The ops are buffered in
    `self.ops`; nothing is sent to the server until `.commit()` or
    `.abort()` is called. Each call returns `self` so you can chain:

        tx = client.begin_tx("t-1")
        tx.set("a", "1").delete("b")
        result = tx.commit()
    """

    def __init__(self, client: "Client", tx_id: str) -> None:
        self._client = client
        self.tx_id = tx_id
        self.ops: List[dict] = []

    def set(self, key: str, value: str) -> "Transaction":
        self.ops.append({"Put": {"key": key, "value": value}})
        return self

    def delete(self, key: str) -> "Transaction":
        self.ops.append({"Delete": {"key": key}})
        return self

    def commit(self) -> TxResult:
        return self._client._commit_tx(self.tx_id, self.ops)

    def abort(self) -> None:
        # 2PC abort is a DecideTx(commit=false) on the wire; the
        # server treats it identically to a coordinator-driven abort.
        self._client._send_decide(self.tx_id, commit=False)

    # ----- context manager -----

    def __enter__(self) -> "Transaction":
        # The context manager doesn't enforce a server-side abort on
        # exception; that would require an async hook we don't have
        # on this sync SDK. Callers who want auto-abort should
        # explicitly `.abort()` in an except / finally block.
        return self

    def __exit__(self, *exc) -> None:
        # No-op. We don't auto-commit or auto-abort — the caller
        # decides via .commit() / .abort(). This matches Rust's
        # typical `let tx = ...; tx.commit()?;` style.
        return None


# ---------- public client ----------

class Client:
    """A blocking client for Oxide-KV.

    Connect to one node, or use `Client.discover(...)` to scan a list
    of (host, port) tuples and return the first one that claims to be
    the leader. `Client.discover` is best-effort — it tries each node
    in order and falls back on the first non-leader rejection rather
    than doing a Raft-aware redirect.
    """

    def __init__(self, conn: _Connection) -> None:
        # Internal constructor; use `Client.connect` / `Client.discover`
        # instead. The bare `_Connection` is wrapped here so callers
        # don't need to know about it.
        self._conn = conn

    # ----- factory methods -----

    @classmethod
    def connect(cls, host: str, port: int, timeout: float = 5.0) -> "Client":
        """Open a single-node connection. Will raise NotLeaderError
        on the first mutation if the node isn't a leader."""
        conn = _Connection(host, port, timeout)
        conn.__enter__()
        try:
            return cls(conn)
        except BaseException:
            conn.__exit__()
            raise

    @classmethod
    def discover(
        cls,
        endpoints: List[Tuple[str, int]],
        timeout: float = 5.0,
    ) -> "Client":
        """Try each (host, port) until one accepts a Set without
        returning the not-leader error. Returns a connected Client
        pointing at that leader; raises OxideKVError if none worked.

        The probe is a no-op `Get` against a key the leader can't
        possibly have written — collisions are extremely unlikely
        (16 random hex chars = 64 bits).
        """
        import secrets
        probe_key = f"__probe_{secrets.token_hex(16)}"
        for host, port in endpoints:
            try:
                c = cls.connect(host, port, timeout=timeout)
            except OSError:
                continue
            try:
                c.get(probe_key)
                return c
            except NotLeaderError:
                c.close()
                continue
            except OxideKVError:
                c.close()
                continue
        raise OxideKVError(
            f"no leader found among {endpoints!r}"
        )

    def close(self) -> None:
        self._conn.__exit__()

    # ----- context manager -----

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # ----- raw KV -----

    def set(self, key: str, value: str) -> int:
        """Write a value. Returns the assigned log index."""
        resp = self._request({"Set": {"key": key, "value": value}})
        self._expect_ok(resp, want_index=True)
        return int(resp["index"])

    def get(self, key: str) -> Optional[str]:
        """Read a value. Returns None if the key doesn't exist."""
        resp = self._request({"Get": {"key": key}})
        if "error" in resp:
            raise NotLeaderError(resp["error"])
        status = resp.get("status")
        if status == "ok":
            return resp.get("data")
        if status == "not_found":
            return None
        raise OxideKVError(f"unexpected Get response: {resp!r}")

    def delete(self, key: str) -> int:
        """Delete a key. Returns the assigned log index."""
        resp = self._request({"Delete": {"key": key}})
        self._expect_ok(resp, want_index=True)
        return int(resp["index"])

    # ----- transactions -----

    def begin_tx(self, tx_id: str) -> Transaction:
        """Begin a 2PC transaction. Buffered until commit()/abort()."""
        return Transaction(self, tx_id)

    def _commit_tx(self, tx_id: str, ops: List[dict]) -> TxResult:
        resp = self._request({"BeginTx": {"tx_id": tx_id, "ops": ops}})
        if "error" in resp and resp.get("status") != "error":
            raise NotLeaderError(resp["error"])
        status = resp.get("status")
        if status == "ok":
            return TxResult(
                decision=resp["decision"],
                begin_index=int(resp["begin_index"]),
                decide_index=int(resp["decide_index"]),
                tx_id=tx_id,
            )
        if status == "aborted":
            raise TxAbortedError(tx_id, resp.get("reason", ""))
        raise OxideKVError(f"unexpected BeginTx response: {resp!r}")

    def _send_decide(self, tx_id: str, commit: bool) -> None:
        """Send a manual DecideTx (used by Transaction.abort()).

        The Rust side's `Command::DecideTx { decision: TxDecision }`
        serializes TxDecision with serde's default externally-tagged
        representation, so the wire shape is
        `{"DecideTx": {"tx_id": "...", "decision": {"Commit": null}}}`
        for commit / `{"Abort": null}` for abort.
        """
        decision = {"Commit": None} if commit else {"Abort": None}
        resp = self._request({"DecideTx": {"tx_id": tx_id, "decision": decision}})
        # DecideTx returns the same shape as Set (status + index); we
        # just discard the index.
        self._expect_ok(resp, want_index=False)

    # ----- internals -----

    def _request(self, payload: dict) -> dict:
        resp = self._conn.request(payload)
        # Generic not-leader guard. Mutations return this as
        # {"error": "Not a leader. ..."}; Get does its own check.
        if "error" in resp and "Not a leader" in resp["error"]:
            raise NotLeaderError(resp["error"])
        return resp

    @staticmethod
    def _expect_ok(resp: dict, want_index: bool) -> None:
        if "error" in resp:
            raise OxideKVError(resp["error"])
        if resp.get("status") != "ok":
            raise OxideKVError(f"unexpected response: {resp!r}")
        if want_index and "index" not in resp:
            raise OxideKVError(f"response missing index: {resp!r}")