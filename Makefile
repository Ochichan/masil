.PHONY: all core agent baseline test benchmark benchmark-env clean

all: core agent

core:
	python3 scripts/build.py

agent:
	python3 scripts/build.py --agent

baseline:
	python3 scripts/build.py --baseline

# Every tests/test_*.py runs, so a new suite is never left out. A failure
# does not stop the rest; the failed suites are listed at the end.
test: core baseline agent benchmark-env
	@failed=; for suite in tests/test_*.py; do \
		echo "== $$suite"; \
		.build/bench-venv/bin/python "$$suite" || failed="$$failed $$suite"; \
	done; \
	if [ -n "$$failed" ]; then echo "failed:$$failed"; exit 1; fi
	cargo test --locked --manifest-path agent/Cargo.toml

benchmark-env:
	python3 -m venv .build/bench-venv
	PIP_DISABLE_PIP_VERSION_CHECK=1 .build/bench-venv/bin/python -m pip install -r tests/performance/requirements.txt

benchmark: core baseline benchmark-env
	.build/bench-venv/bin/python tests/performance/benchmark.py

clean:
	python3 scripts/build.py --clean
