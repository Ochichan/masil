.PHONY: all core agent baseline test benchmark benchmark-env clean

all: core agent

core:
	python3 scripts/build.py

agent:
	python3 scripts/build.py --agent

baseline:
	python3 scripts/build.py --baseline

test: core baseline agent benchmark-env
	python3 tests/test_compatibility.py
	python3 tests/test_bridge.py
	python3 tests/test_watch.py
	python3 tests/test_watch_cli.py
	@if [ "$$(uname -s)" = Darwin ]; then python3 tests/test_respawn_failure.py; fi
	.build/bench-venv/bin/python tests/test_benchmark_harness.py
	cargo test --locked --manifest-path agent/Cargo.toml

benchmark-env:
	python3 -m venv .build/bench-venv
	PIP_DISABLE_PIP_VERSION_CHECK=1 .build/bench-venv/bin/python -m pip install -r tests/performance/requirements.txt

benchmark: core baseline benchmark-env
	.build/bench-venv/bin/python tests/performance/benchmark.py

clean:
	python3 scripts/build.py --clean
