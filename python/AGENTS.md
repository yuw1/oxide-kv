# Python Guidelines

Also see root `AGENTS.md` for cross-language standards.

## Scope

This directory is the **Oxide-KV Python SDK**. It is a pure-Python
client that talks the JSON line wire protocol to a leader node
over a plain TCP socket. **Zero external runtime dependencies** —
stdlib only.

The wire format is documented inline in `oxide_kv/client.py`; it
matches the server side in `rust/oxide-kv/src/client.rs`.

## Commands

Use `pip` (not `uv` / `poetry` / `pdm`) — the project is small
enough that a single `pyproject.toml` with `[project]` +
`[project.optional-dependencies]` covers everything, and the SDK
has zero runtime deps so the resolver isn't a moving target.

| Task | Command |
|---|---|
| Install (editable + dev deps) | `make install` |
| Run tests | `make test` (assumes a leader node on `127.0.0.1:9101`; override with `OXIDE_KV_TEST_HOST` / `OXIDE_KV_TEST_PORT`) |
| Lint | `make lint` (ruff) |
| Format | `make format` (ruff) |
| Clean build / cache artifacts | `make clean` |

## Style

- Target Python 3.9+ (`requires-python = ">=3.9"` in `pyproject.toml`).
- No external runtime dependencies. If you need a feature that
  isn't in stdlib, open an issue before adding it — the SDK is
  designed to be `pip install`-able with no resolver.
- Public API surface lives in `oxide_kv/__init__.py` (re-exports
  from `client.py`). Add new types there, not at the import path
  `oxide_kv.client`.
- All exceptions inherit from `OxideKVError`. New error types must
  follow the same naming convention (`NotLeaderError`,
  `TxAbortedError`, ...).

## Testing

- Tests are deliberately scoped to the wire contract, not the
  server's correctness (that's the Rust test suite's job).
- Each test that needs a server reads `OXIDE_KV_TEST_HOST` /
  `OXIDE_KV_TEST_PORT` env vars; defaults to
  `127.0.0.1:9101`.
- **No CI workflow for the SDK** (the former
  `.github/workflows/python.yml` was deleted: it compiled the full
  Rust binary per matrix cell just to run seven socket tests).
  Validate SDK changes locally:
  `python -m pip install -e ".[dev]" && python -m pytest tests/`
  against a running server.

## Future work

- **PyO3 binding** — when we ship a Rust extension for the
  client, swap `pyproject.toml`'s `build-backend` from
  `setuptools.build_meta` to `maturin` and add a `[tool.maturin]`
  block. The `Makefile` targets stay the same.
- **Type stubs** — once the SDK is stable, ship `oxide_kv.pyi`
  for editor autocomplete. Until then, the type hints in
  `client.py` are sufficient.