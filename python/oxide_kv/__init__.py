"""oxide_kv — Python SDK for Oxide-KV.

See `oxide_kv.client` for the public API; this file just re-exports
the names callers should reach for.
"""

from .client import (
    Client,
    NotLeaderError,
    OxideKVError,
    Transaction,
    TxAbortedError,
    TxResult,
)

__all__ = [
    "Client",
    "NotLeaderError",
    "OxideKVError",
    "Transaction",
    "TxAbortedError",
    "TxResult",
]

__version__ = "0.1.0"