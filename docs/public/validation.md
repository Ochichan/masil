# Verification and performance

Run the public checks from a fresh checkout:

```sh
make all
cargo fmt --manifest-path agent/Cargo.toml --check
cargo clippy --locked --manifest-path agent/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path agent/Cargo.toml
make test-ci
make test-benchmark
python3 scripts/ci_smoke.py bin
```

`make test-ci` selects deterministic regression cases for core compatibility,
prompt replay and receipt conflicts, restore, endpoint disconnect and the local
installer. Its fixtures use private temporary directories and sockets. No real
provider credentials, model calls or separately cloned tmux tree are required.
The broader `make test` remains a local development gate and is not the public
fresh-checkout contract.

`make test-benchmark` installs the pinned measurement-only Python dependencies
in `.build/bench-venv` and tests the measurement tools. CI checks their behavior,
not wall-clock performance thresholds on shared runners.

The CI configuration runs on macOS arm64 and Ubuntu x86_64, including package
and Homebrew smoke checks. A workflow definition is not evidence that a new
commit passed remote CI. Report local results separately from hosted CI results.

## Interpreting performance results

The published terminal harness supports explicitly validated Herdr
version/protocol pairs, including 0.9.5/protocol 22. Other pairs fail closed.
The executable is selected with `--herdr`; it never updates an installed Herdr.
For a bounded alternating comparison using a separately obtained binary:

```sh
make all benchmark-env
.build/bench-venv/bin/python tests/performance/compare.py \
  --rounds 2 --products masil,herdr --output .build/comparison.json -- \
  --herdr /absolute/path/to/herdr \
  --profile smoke --pane-counts 1,15,50 \
  --inner-columns 93 --hidden-inner-columns 93 --inner-rows 39 \
  --startup-samples 10 --rtt-samples 100 --idle-seconds 5 \
  --throughput-seconds 5 --throughput-repeats 2 --output-style scroll
```

Use a new output filename for every run. The comparison wrapper reverses product
order each round, preserves raw per-run results and rejects changed inputs or
incomparable geometry. Herdr 0.8.2 requires its own validated grid settings;
do not apply 0.9.5's dimensions to it without measuring them. Each product uses
private HOME/config/socket paths. Cleanup targets those instances only.

This command is a short terminal workload comparison, not a full performance
gate. It does not enable native provider integrations, the masil coordinator,
agent desk or SSH. `--profile full` enforces larger sample counts and durations;
even that profile does not turn terminal fixtures into a full-product test.

Earlier September 2026 figures compared the masil terminal core, with its
agent layer off, against installed Herdr 0.8.2. They are historical results.
They do not establish current full-product performance, native provider
accuracy, or a C-versus-Rust advantage.

A useful comparison must record binary hashes and versions, the tested feature
set, terminal geometry, pane count, workload, process accounting and repetitions.
Include both products in alternating order, keep geometry equal, and separate
idle memory, input latency and output cost. A fast PTY write is not proof that
the terminal rendered every byte.

Measure resident masil features separately:

```sh
.build/bench-venv/bin/python tests/performance/management.py \
  --counts 1,15,50 --seconds 5 --samples 10 \
  --output .build/management.json
```

This enables inbox and coordinator, creates synthetic agents with explicit test
detection rules, measures list latency, opens the full-screen desk and samples
core/coordinator/desk RSS and CPU while draining its terminal output. Fixture
children and transient command processes are not included in resident RSS.
The report records both masil binary hashes and fixture hashes. The tool never
uses real provider credentials and closes its private server afterward.

This is a masil-only measurement, not a comparative Herdr management-overhead
claim. It does not measure a real provider, model/network latency, native hooks,
SSH, approval handling or task correctness. Comparative management overhead
remains unqualified until equivalent feature configurations are exercised on
both products. Unknown protocol versions or mismatched geometry must fail
rather than silently produce a comparison table.

Real-provider qualification requires a versioned record of startup, working,
approval, idle, prompt, interrupt and resume behavior. Authentication failures
and untested operations stay unverified. Never turn provider recognition or a
passing mock test into a live support claim.

## Local validation on 2026-11-18

- Rust formatting and warning-denying clippy checks passed. Rust tests passed
  737 cases, with one ignored case and no failures.
- `make test-ci` passed 13 cases. The same gate and runtime smoke also passed
  in a directory exported from the Git index, using copies of the current
  built binaries. This checks the published fixture/import closure; it is not
  a separate clean-source rebuild or a hosted CI run.
- `make test-benchmark` passed 18 measurement-tool tests, including disappearing
  process, changed provenance and desk-cleanup failure cases.
- A temporary-prefix source installation, its runtime smoke and repeated
  installation passed. User installation directories were not used for this check.
- Herdr 0.9.5 terminal comparison completed four runs in alternating order.
  The separate masil management probe completed at 1, 15 and 50 synthetic agents.
  [Results and limitations](performance-2026-11-18.md) are recorded separately.
- Independent review findings about export command context, missing process
  samples, measurement provenance and cleanup were corrected before handoff.

The Linux/hosted CI matrix and real-provider/SSH comparative performance were
not run in this local validation. Private full-suite tests that require an
upstream clone or local accounts are outside the public gate above.
