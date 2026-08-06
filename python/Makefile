# Oxide-KV Python SDK Makefile
#
# Conventions follow lance-rs/python/Makefile (per
# python/AGENTS.md). We use setuptools for the build backend (no
# maturin / Rust extension yet — `oxide_kv` is pure-Python over a
# JSON line socket). The day we ship a PyO3 binding, swap the
# backend to maturin and add a [tool.maturin] block; the targets
# below don't change.

.PHONY: help install test lint format clean

help:
	@echo "Targets:"
	@echo "  install  - pip install -e '.[test]'"
	@echo "  test     - pytest tests/"
	@echo "  lint     - ruff check oxide_kv/ tests/"
	@echo "  format   - ruff format oxide_kv/ tests/"
	@echo "  clean    - remove build / cache artifacts"

install:
	pip install -e ".[test]"

test:
	pytest tests/ -v

lint:
	ruff check oxide_kv/ tests/

format:
	ruff format oxide_kv/ tests/

clean:
	rm -rf build/ dist/ .pytest_cache/ .ruff_cache/
	find . -type d -name __pycache__ -exec rm -rf {} +
	find . -type d -name oxide_kv.egg-info -exec rm -rf {} +