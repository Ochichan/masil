# Performance harness

`benchmark.py` compares the built `bin/masil`, the pinned `bin/tmux-baseline`,
and the installed Herdr 0.8.2 binary using the same deterministic Python PTY
fixture. It writes raw JSON when `--output` is supplied and otherwise writes
JSON to stdout.

Run `make benchmark-env` once, then use `.build/bench-venv/bin/python` in the
commands below. The local environment pins pyte 0.8.2 and wcwidth 0.2.13 for
measurement only. Neither dependency is included in masil. pyte reconstructs
the host screen so differential redraws count as responses. The RTT timestamp
is taken at the read that supplies the completed marker, before parser or log
work. See [pyte documentation](https://pyte.readthedocs.io/en/latest/).

The bounded smoke run exercises every workload shape but is not eligible for a
performance-budget claim:

```sh
.build/bench-venv/bin/python tests/performance/benchmark.py \
  --profile smoke \
  --output /tmp/masil-benchmark-smoke.json
```

For a useful comparative run with qualifying startup and throughput sample
sizes but a deliberately shorter idle interval, keep `--profile smoke`. The
artifact will continue to say `budget_eligible: false`:

```sh
.build/bench-venv/bin/python tests/performance/benchmark.py \
  --profile smoke \
  --startup-samples 100 \
  --rtt-samples 1000 \
  --idle-seconds 30 \
  --throughput-seconds 60 \
  --throughput-repeats 3 \
  --output /tmp/masil-benchmark-comparative.json
```

The full profile enforces at least 100 startup samples, 1,000 attached RTT
samples, 10 minutes of idle observation for each 1/15/50-pane workload, and
three 60-second paced-output repeats. Passing those scheduling checks does not
itself mean that a performance budget passed; the JSON leaves
`performance_budget_pass` unset for later analysis.

```sh
.build/bench-venv/bin/python tests/performance/benchmark.py \
  --profile full \
  --output /tmp/masil-benchmark-full.json
```

The Herdr adapter starts only a foreground server with a fresh allowlisted
environment. `HOME`, every XDG directory, the config, and both socket paths are
inside a private `/tmp/msl-bench-*` directory. Inherited `HERDR_SESSION`,
`HERDR_REMOTE`, and `TMUX` are absent. Cleanup addresses that private socket;
there is no `killall`, `pkill`, or unqualified stop of a user instance.

The host attachment remains 120×40. The installed Herdr 0.8.2 reports a 93×39
inner terminal for its active root pane and 94×39 for panes on hidden tabs.
masil and tmux use matching manual window sizes for active and hidden windows,
so the unused host area on those clients and Herdr's sidebar remain product UI
cost while corresponding workload grids are identical. `--inner-columns`,
`--hidden-inner-columns`, and `--inner-rows` change the required grids; a run
fails if any fixture reports a different value.

Startup server readiness, workspace/session CLI readiness, and child fixture
readiness are separate values. Headless output measures PTY acceptance using a
completion sidechannel. Attached input RTT waits for a unique marker to return
through the real client PTY, so it is not an API acknowledgement. Server,
attached-client, and fixture-child processes are reported separately. Inner
pane dimensions are recorded for every workload; results with different inner
geometry need to be treated as different conditions.

`--output-style inplace` repeatedly writes CUP, 80 ASCII characters and EL.
It avoids ongoing history growth when products have different default history
limits. The default `scroll` stream includes wrapping and newlines, so its RSS
includes different retention policies. These are different workloads. Output
bytes include control sequences. Each output interval continues through child
completion plus a sampling grace period; completion proves PTY write
acceptance, not complete terminal parsing or display. The aggregate offered
rate stays fixed as pane count grows.

The Herdr config disables onboarding, sound, update checks and experimental
pane history. It uses `/bin/sh` in non-login mode and a private runtime
directory. It preserves the product's ordinary rendering and terminal
scrollback behavior. Serial product order, normal host background activity,
Python fixture cost and `ps` accounting resolution limit causal conclusions.
