# Performance observations, 2026-11-18

The [machine-readable selected metrics](performance-2026-11-18.json) record
binary and harness hashes, workload parameters and per-round measurements.
They omit raw process commands and temporary paths. The full local reports are
`.build/masil-herdr-0.9.5-alternating-corrected.json` and
`.build/management-2026-11-18-validated.json`; their SHA-256 digests are recorded
in the published summary. [Reproduction instructions](validation.md) use the
same tracked measurement tools.

## Terminal comparison

The official [Herdr 0.9.5 release](https://github.com/herdrdev/herdr/releases/tag/v0.9.5)
macOS arm64 binary was downloaded into a private build directory and checked
against its published SHA-256. Its API protocol was 22. The installed Herdr
binary and user configurations were not changed.

The macOS arm64 run used two alternating rounds: masil then Herdr, followed by
Herdr then masil. Each round/product used ten startup samples, 100 input RTT
samples, five-second idle intervals and two five-second paced-output repeats
at approximately 1 MiB/s aggregate. The burst size was 1 MiB. Counts were 1,
15 and 50 panes in separate windows/tabs, not 50 simultaneously visible splits.
Both active and hidden fixture grids were 93×39, with a 120×40 host PTY.

All four runs completed with matching grids and stable binary, harness and
host metadata. Output used ASCII scrolling with product-default history
retention. The masil core ran without its agent management layer. No actual
provider, model, remote connection or native integration was exercised.

| Measure | masil core | Herdr 0.9.5 |
| --- | --- | --- |
| 50-pane idle server RSS | 4.69–4.73 MiB | 27.63–27.92 MiB |
| Input RTT median | 2.44–2.47 ms | 3.03–3.11 ms |
| Input RTT P99 | 2.59–2.84 ms | 3.24–3.32 ms |
| Single-pane paced-output CPU | 4.63–5.16% | 2.58–2.78% |
| 50-pane paced-output CPU | 4.32–4.84% | 9.80–10.62% |

Ranges cover observed rounds/repeats, not confidence intervals. CPU is a
percentage of one logical core; it uses process CPU accounting, not whole-machine
utilization. RSS is not PSS and shared pages can be double-counted. RTT ends when
the host PTY supplies a marker reconstructed by pyte, not at physical monitor
presentation. Output completion proves fixture PTY writes, not complete rendering.

These short smoke-profile results are not budget-eligible. Normal host activity,
short intervals, warm caches and default scrollback policies limit interpretation.
The input-latency gap is much smaller than the historical Herdr 0.8.2 result;
do not reuse the earlier ratio as a current-product claim. Neither comparison
establishes full-product superiority or a language-level performance advantage.

## Resident management features

A separate masil-only run enabled inbox and coordinator, started 1/15/50
synthetic agents with explicit test detection rules, queried their list ten
times, and opened the full-screen desk. Each resident sample lasted five seconds.
The summary records core, coordinator and desk RSS/CPU separately, plus all list
latency samples. It excludes synthetic child processes and transient CLI process
memory. Missing processes fail measurement instead of producing zero-cost results.

The provider is a local C fixture, not Codex or another actual provider. The desk
uses a real PTY with a controlling terminal. Cleanup closes the desk and its
private server even when rendering or graceful shutdown fails. Binary and
measurement-code identity are checked before reporting success.

This establishes a reproducible measurement path for resident features. It does
not qualify native callbacks, model/network latency, SSH, every terminal/IME, or
equivalent Herdr management overhead. Those remain separate validation work.
