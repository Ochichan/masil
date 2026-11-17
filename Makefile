PREFIX ?= $(HOME)/.local

.PHONY: all core agent baseline install test test-ci test-benchmark benchmark benchmark-env clean

all: core agent

core:
	python3 scripts/build.py

agent:
	python3 scripts/build.py --agent

baseline:
	python3 scripts/build.py --baseline

install: all
	python3 scripts/install.py --prefix "$(PREFIX)"

# This set is intentionally independent of the private upstream tmux clone and
# third-party Python packages. Keep the names explicit so ignored local tests
# cannot enter the public gate by accident.
test-ci: core agent
	PYTHONPATH=tests python3 -m unittest -v \
		test_compatibility.BasicContracts \
		test_core_ledger.CoreLedger.test_replay_conflict_and_nonretained_receipts \
		test_core_prompt.CorePrompt.test_delivery_replay_uses_the_core_receipt_and_fences_old_options \
		test_core_prompt.CorePrompt.test_a_32k_multiline_prompt_arrives_once_with_bracket_markers \
		test_install \
		test_managed_restore.ManagedRestore.test_restore_and_repeat_do_not_duplicate_an_agent \
		test_restore.Restore.test_a_previewed_restore_saves_the_files_first_and_writes_only_them \
		test_remote_links.RemoteLinks.test_disconnect_ends_the_stream_and_starts_nothing_there

# Every tests/test_*.py runs, so a new suite is never left out. A failure
# does not stop the rest, the Rust tests included; the failed suites are
# listed at the end.
test: core baseline agent benchmark-env
	@failed=; for suite in tests/test_*.py; do \
		echo "== $$suite"; \
		.build/bench-venv/bin/python "$$suite" || failed="$$failed $$suite"; \
	done; \
	cargo test --locked --manifest-path agent/Cargo.toml || failed="$$failed cargo-test"; \
	if [ -n "$$failed" ]; then echo "failed:$$failed"; exit 1; fi

benchmark-env:
	python3 -m venv .build/bench-venv
	PIP_DISABLE_PIP_VERSION_CHECK=1 .build/bench-venv/bin/python -m pip install -r tests/performance/requirements.txt

test-benchmark: benchmark-env
	.build/bench-venv/bin/python tests/test_benchmark_harness.py
	.build/bench-venv/bin/python tests/test_performance_compare.py
	.build/bench-venv/bin/python tests/test_management_probe.py

benchmark: core baseline benchmark-env
	.build/bench-venv/bin/python tests/performance/benchmark.py

clean:
	python3 scripts/build.py --clean
