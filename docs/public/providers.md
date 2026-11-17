# Provider support and setup

Provider recognition, declared operations, versioned live observations and
readiness of a running agent are separate facts. Use these commands before
automating a provider:

```sh
masil-agent agent providers
masil-agent agent capabilities --provider claude
masil-agent doctor
masil-agent doctor --versions
```

The first two commands work without a server. Their `support` field separates
`declarations`, `live_evidence`, `known_limitations` and `integration_readiness`.
Existing fields keep their meanings. `doctor` adds the same information to each
provider it checks. It does not invoke provider programs unless `--versions` is
given; that option runs their version commands with a two-second limit.

## Read the result correctly

| Field | Meaning |
| --- | --- |
| `declarations` | Paths implemented in masil, including manifest detection, prompt mode, interrupt keys, resume and answer-channel availability |
| `live_evidence` | Maintainer-recorded observations for listed provider versions and dates, with capability-specific limitations |
| `verification.version_evidence` | Whether a version probe matched a recorded observation; an exact match is historical evidence, not a new live test |
| `verification.status` | Remains `unverified` for local runtime readiness; static inventory cannot prove hooks, authentication or a current session |
| `integration_readiness` | Export availability and setup guidance; it does not claim a hook is installed |

The evidence catalog is in `agent/src/provider_support.rs`, with the
[recorded observations](provider-observations.md) published alongside it. It records prior
manual observations, not tests rerun by `doctor`. Provider versions without a
record stay unverified. Different releases, prereleases, terminal geometry and
configuration can change behavior. A passing mock test does not add a live
observation to this catalog.

## Configure integration before relying on status

For a provider with an exportable integration, run this inside a masil pane:

```sh
masil-agent agent integration export claude --directory /absolute/path/to/hooks
```

From another shell, specify the running server socket explicitly:

```sh
masil-agent agent --socket /path/to/masil.sock integration export claude --directory /absolute/path/to/hooks
```

This prints JSON describing files and registration instructions. It does not
install files or edit provider settings. Review that output, write the files
at the requested paths, register them in the provider and start a new managed
run. Use the provider's own configuration mechanism for registration.

Then inspect the actual run from that masil pane, or with the same `--socket`:

```sh
masil-agent agent capabilities builder
masil-agent agent get builder
```

These report evidence for the named running agent. Static export support does
not prove that callbacks arrived or that the displayed native conversation is
the one a callback names. An executable on PATH also does not prove login.

OpenCode and Kilo answer channels require an explicit launch:

```sh
masil-agent agent start builder opencode --answers
```

The answer channel checks the provider version and run. A declaration that it
exists does not bypass those checks. Replies remain explicit human actions.

## Known constraints

- Claude needs its exported hook for reliable lifecycle tracking. Without it,
  streamed answers can look idle. Do not infer readiness from the composer alone.
- Codex prompt delivery does not prove provider acceptance. Session callbacks
  and observed screen state are distinct evidence.
- Cursor uses guarded two-step paste. A mock validation and a manually observed
  key sequence do not establish end-to-end support for every installed version.
- OMP and Pi can remain unknown before a lifecycle report. An exported extension
  has to be loaded into the actual run.
- OpenCode and Kilo have version-bounded answer integrations. Kilo prompt delivery
  lacks the acceptance digest available on the OpenCode path.
- Some recorded Gemini, Kimi and Pi checks stopped at authentication. These are
  limited observations, not successful authenticated automation tests.
- Draft detection is implemented for Claude, Codex and OMP. Other providers can
  append a submitted prompt to text already in their composer.

`prompt` requires an observed idle foreground agent. `wait --state idle` observes
state; neither command proves that the task succeeded. For consequential work,
inspect the provider's result and validate the resulting files or tests.
