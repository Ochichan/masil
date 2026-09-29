# Agent native integration bridge

masil can accept optional native provider callbacks for a managed agent run. The bridge supplements process and screen observation; it does not install provider hooks, answer approvals, submit prompts, or claim that a provider accepted input.

List the supported callback targets and their evidence level:

```sh
masil-agent agent integration status
masil-agent agent integration status opencode
```

The 17 targets follow source-reviewed Herdr integrations. Native lifecycle callbacks are available for Pi, OMP, Kimi, OpenCode, Kilo, and MastraCode. Claude, Codex, Copilot, Devin, Droid, Hermes, Qoder CLI, Qwen, Cursor, Antigravity CLI (`agy`), and Grok report session identity only. For those providers, terminal screen detection remains the activity and blocker authority.

`export` prints an artifact description as JSON. It never writes into a provider configuration directory. Pass the absolute directory where you plan to place the returned files so registration fragments contain the intended paths:

```sh
masil-agent agent integration export codex --directory "$HOME/.codex/hooks" > codex-bridge.json
```

Pi and OMP exports contain TypeScript extensions using their native `pi.on` entry points. OpenCode and Kilo exports contain JavaScript plugins, and Hermes exports a Python plugin directory. Command-hook providers receive a POSIX bridge plus a provider-shaped JSON or TOML registration fragment. Every response includes file modes, absolute destinations, source-reviewed event names, and explicit `installed: false` and `live_provider_tested: false` fields. Review the current provider documentation and merge the fragment yourself. Generated adapters use the absolute `masil-agent` path, invoke it without a shell where the provider API permits, and pass callback JSON only through standard input. JavaScript and TypeScript adapters require valid `TMUX_PANE`, `MASIL_AGENT_RUN`, and absolute `MASIL_AGENT_SOCKET` values and reject callback bodies above 256 KiB before starting a subprocess.

Hooks are scoped to agents started by `masil-agent agent start`. tmux supplies `TMUX_PANE`, and masil injects `MASIL_AGENT_RUN` and `MASIL_AGENT_SOCKET` into that pane. The bridge checks the pane, run nonce, foreground provider, process state, and an increasing callback sequence before recording anything. A callback that races with respawn, resume, or another report fails without retrying against the new run.

OpenCode and Kilo also bind one root native session to the managed pane. Generic server-wide `session.created` and `session.updated` events cannot establish or replace that binding. The initial binding must come from the exported adapter's frontend-scoped `chat.message` callback; a later change requires an explicit frontend-scoped selection event. Every identity and lifecycle callback after binding must carry the same session ID. Events with `parentID`, `parentSessionID`, or their supported spelling variants are child-session evidence and are always rejected, so a child update followed by child idle cannot change the parent's identity or state.

For deterministic adapters, `MASIL_AGENT_SEQUENCE` may contain a positive integer. Otherwise the bridge uses the callback process's Unix timestamp in nanoseconds. Callback bodies must be one JSON object of at most 256 KiB. Session references are capped at 512 bytes and reject control characters.

Session-only callbacks preserve the last lifecycle report and update only the resumable session identity. Unknown events, unknown status values, invalid JSON, missing identity, and lifecycle claims from session-only providers fail closed.

The event names and identity fields were validated against Herdr commit `0d5d6f1f317e238c8297076bc6ab5c3a0cd56283`. Herdr is distributed under Apache License 2.0; the license text is in [herdr-agent-LICENSE.txt](reference/herdr-agent-LICENSE.txt). The generated masil scripts are new bridge code and do not copy Herdr hook assets.
