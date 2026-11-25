# masil

**English** | [한국어](README.ko.md)

A tmux-compatible terminal workspace built for running multiple AI coding agents side by side, with an optional mobile app to monitor, steer, and launch agents from your phone.

Run Claude Code, Codex, OpenCode, Antigravity CLI, Gemini CLI, Cursor, and other terminal coding tools in real terminal panes without losing your tmux key bindings, `~/.tmux.conf`, or shell speed. When you run several agents at once, masil shows which ones are working, waiting for input, or done, lets you jump straight to the right pane, and optionally syncs conversations and git changes to your phone over end-to-end encryption.

## Why masil

If you use terminal AI coding tools every day, the same friction shows up quickly:

- You start three or four agents across different repos and lose track of which pane is working, idle, or blocked on a permission prompt.
- Switching tabs to inspect diffs, roll back a bad edit, or launch another agent in a fresh git worktree breaks focus.
- Once you step away from your desk, you cannot check progress or send the next prompt without setting up an ad-hoc SSH session on a tiny phone screen.

masil solves this without wrapping your terminal in a heavy Electron app or putting a proxy in front of your shell:

- **Your terminal stays native.** The multiplexer core is derived from tmux (C), so PTYs, rendering, scrollback, and `~/.tmux.conf` keep working at native speed.
- **Original agent CLIs, not forks.** masil launches and observes the real `claude`, `codex`, `opencode`, `agy`, and other CLIs directly in panes.
- **Desktop TUI + optional mobile app.** Manage everything from a keyboard/mouse TUI in the terminal, and turn on the mobile gateway only when you want phone access.
- **Honest status reporting.** masil reports only what it can observe (process state, screen signals, hook events) and never fakes approvals or task completion.
- **Vibe-codable local plugins.** Extend both the TUI and mobile app with themes, panels, `/slash` commands, timeline cards, hooks, and presets using the bundled `masil-plugin` skill for Claude Code, Codex, and Antigravity.

## Quick start

### 1. Install on macOS or Linux

**Homebrew (macOS / Linux):**

```sh
brew tap Ochichan/masil https://github.com/Ochichan/masil
brew install --HEAD Ochichan/masil/masil
```

Update later with `brew upgrade --fetch-HEAD Ochichan/masil/masil`. Installing masil leaves your system `tmux` binary and sockets untouched.

**Prebuilt archives (macOS 15+ Apple Silicon, Ubuntu 24.04 x86_64):**

Download the archive and `SHA256SUMS` from [GitHub Releases](https://github.com/Ochichan/masil/releases). Runtime libraries required:

- macOS: `brew install libevent ncurses utf8proc jemalloc`
- Ubuntu 24.04: `sudo apt-get install libevent-core-2.1-7t64 libncursesw6 libtinfo6 libutf8proc3 libjemalloc2`

**Build from source:**

```sh
make            # builds bin/masil and bin/masil-agent
make install    # installs into ~/.local/bin (override with PREFIX=/path)
```

For full dependency and upgrade details, see [docs/public/install.md](docs/public/install.md). Windows is not supported as a masil host.

### 2. Start using masil with your AI coding tools

Start a masil terminal session just like tmux:

```sh
masil
```

Inside masil, launch agents in managed panes (or run your CLIs manually as usual; masil detects running agent panes automatically):

```sh
masil-agent agent start backend claude --cwd ~/Projects/api
masil-agent agent start tests codex --cwd ~/Projects/api --split %0
masil-agent agent list
```

Open the interactive **Agent Desk** TUI or compact **Side Panel**:

```sh
masil-agent agent ui        # full-screen agent desk (mouse + keyboard, EN/KO, dark/light/terminal themes)
masil-agent agent sidebar   # 34-column persistent side panel that expands with tmux zoom
```

Useful everyday shortcuts inside Agent Desk:
- `j`/`k` or arrows: move across agents
- `/`: live search (supports Korean initial consonants, so `ㄱㅂ` matches `개발서버`)
- `:` : command palette for agents, panes, and actions
- `g`: jump directly to the selected agent's pane
- `l` / `t`: toggle English/Korean and Dark/Light/Terminal theme

### 3. Optional: connect your phone

Nothing runs in the background until you opt in. To pair your phone with your Mac or Linux machine:

```sh
masil-agent remote setup
```

The terminal onboarding wizard walks through four tabs:
1. **Provider hooks**: installs opt-in marker hooks for Claude Code, Codex, OpenCode, and Antigravity CLI without touching your existing hooks.
2. **Relay & daemon**: configures direct LAN/Tailscale access (default port `7767`), optional self-hosted relay URL, and background daemon/service control.
3. **QR pairing**: generates a 5-minute one-time pairing QR code right in the terminal (press `o` to open a crisp SVG QR in your browser if your terminal is short).
4. **Paired devices**: view connected phones and revoke access immediately.

Once paired, all traffic between your phone and host is end-to-end encrypted with Noise NK.

## Supported AI coding tools

masil recognizes **24 coding agent CLIs**, including Claude Code, Codex, OpenCode, Antigravity CLI (`agy`), Gemini CLI, Cursor, GitHub Copilot CLI, Amp, Kilo, and OMP.

- **Full desktop + mobile support (hooks, live conversation sync, model/effort/mode controls):** Claude Code, Codex, OpenCode, Antigravity CLI (`agy`).
- **Desktop pane discovery, status tracking, prompt delivery, and session resume:** 18 providers with native resume support.

Run `masil-agent agent providers` to inspect detected providers on your machine, or read [docs/public/providers.md](docs/public/providers.md).

## What you can do

### Terminal workspace & Agent Desk
- **Drop-in tmux compatibility**: default `C-b` prefix, `~/.tmux.conf` support, isolated `masil-UID` sockets (never collides with stock `tmux`), and an optional `compat/tmux` shim for scripts.
- **Built-in mouse UI layer**: status bar menu, `+` button for new windows/panes, floating pane drag, scrollbars, and session snapshots (`prefix C-s` to save, `prefix C-r` to restore, optional 15-minute autosave). Pass `masil -f /dev/null` for plain tmux behavior.
- **Safe prompt delivery & waits**: `masil-agent agent prompt NAME 'run tests'` checks that the agent is idle and still the same process, records the operation in SQLite before sending, and avoids duplicate replays. Use `masil-agent agent wait NAME --state idle --timeout 600` in scripts.
- **Inbox & notifications**: enable `masil-agent agent inbox enable` and `masil-agent agent notify enable` to collect blocked prompts, approval requests, questions, and finished turns.
- **Git worktrees & file checkpoints**:
  - `masil-agent worktree create REPO BRANCH` creates an isolated git worktree for a task.
  - `masil-agent agent changes NAME` lists modified files in an agent's working tree.
  - `masil-agent agent checkpoint NAME make` and `checkpoint NAME restore` save and roll back single files with a preview.
- **Workspace templates & column layouts**:
  - `masil-agent layout apply NAME --yes` spins up repeatable multi-pane repos and agents from `~/.config/masil/layouts/NAME.toml`.
  - `masil-agent layout columns on` arranges a window as horizontally scrollable fixed-width columns.
- **Multi-machine control**: `masil-agent agent endpoints add` lets you inspect and control masil servers on other machines over SSH.

### Mobile companion app (`mobile/`)
Built with Expo / React Native for Android and iOS:

- **Conversation-first reading**: chat threads render clean prose and inline markdown code, collapsing noisy bursts of tool calls into lightweight 1-line summaries (tap to expand full tool input/output).
- **Live terminal fallback**: switch to the **Terminal** tab anytime for direct interactive terminal control of the live pane.
- **Visual agent launcher**: pick a project folder on your host machine with a GUI directory browser (plus quick recent-repo shortcuts), choose CLI provider, select model from a searchable dropdown, configure reasoning effort and execution mode, or start in a brand-new Git worktree branch.
- **Live session tuning**: change model, reasoning effort, or mode on an already running agent from the options sheet.
- **Workspace tools**: inspect real-time git diffs, browse project files, review checkpoints and schedules, use voice dictation, and switch between multiple paired hosts.
- **Live plugins & Creator Mode**: run plugin `/slash` commands in the composer, view interactive plugin tabs in Workspace Tools and cards in conversations, switch synced plugin themes, launch from plugin presets, or tap **Settings -> Plugins -> Create plugin with AI** to build a new plugin on the spot.

#### Building the mobile app locally

```sh
cd mobile
npm ci
npm run build:apk:arm64   # Android arm64 APK -> mobile/artifacts/masil-production-arm64.apk
npm run build:ios:local   # Local iOS build (requires Xcode)
```

### Self-hosted E2EE relay (`relay/`)

Over LAN or Tailscale, the mobile app connects directly to your host daemon with no external server. If you want remote access across networks without Tailscale, you can run `masil-relay` on any small VPS:

```sh
make relay                # builds bin/masil-relay
# or with Docker:
cd relay && docker compose -f docker-compose.example.yml up -d
```

Then pair using `masil-agent remote pair --relay wss://your-relay.example.com`. The relay only forwards opaque Noise NK encrypted envelopes and never sees prompts, code, or terminal output.

## Local plugins (vibe-codable with Claude Code, Codex, and Antigravity)

Extend both the Rust TUI and the Mobile app without recompiling binaries or rebuilding the APK. Every plugin lives in `~/.config/masil/plugins/<id>/` (or any local folder registered by path) and hot-reloads live across connected clients.

A single plugin (`masil-plugin.toml`) can contribute any combination of:
- **Themes (`theme.toml`)**: Omarchy-style hex palettes with zero code required, synced live between TUI and mobile.
- **Workspace panels**: declarative Masil UI Spec v1 blocks (`metric_row`, `table`, `list`, `markdown`, `code`, `diff`, `progress`, `sparkline`, `callout`, `button`, `toggle`, `select`, `form`) rendered natively in Mobile Workspace Tools and the TUI Command Palette (`:`).
- **Slash commands**: `/command [args]` with autocomplete in the mobile composer and entries in the TUI Command Palette, returning toasts, UI sheets, composer drafts, or inline timeline cards.
- **Timeline cards**: persistent structured cards pushed into an agent's conversation view.
- **Lifecycle hooks**: react to `agent.state_changed`, `agent.started`, `agent.closed`, `inbox.event`, `theme.changed`, and `prompt.before`.
- **Agent launch presets**: one-tap provider, model, effort, mode, worktree, and initial-prompt profiles in the mobile launcher.

### Install the official `masil-plugin` AI skill

Install the bundled skill once so Claude Code, Codex, and Antigravity know the live schema, UI spec, and verification commands:

```sh
masil-agent plugin skill install --target all
masil-agent plugin skill status
```

Target directories:
- Claude Code: `~/.claude/skills/masil-plugin/`
- Codex: `~/.codex/skills/masil-plugin/` and `~/.agents/skills/masil-plugin/`
- Antigravity (`agy`): `~/.gemini/config/skills/masil-plugin/`

Then ask Claude Code, Codex, or Antigravity directly:
- *"Create a Tokyo Night dark theme plugin for masil and apply it now."*
- *"Build a masil workspace panel showing my project's TODO checklist and a `/todo` slash command."*
- *"Add a masil hook plugin that posts a summary card whenever an agent enters blocked state."*

The agent runs `masil-agent plugin schema`, scaffolds with `init`, validates with `check` and `test`, and hot-reloads via `reload` so changes appear live in your terminal and phone.

### Try the bundled starter plugins

```sh
masil-agent plugin install examples/plugins/catppuccin-mocha
masil-agent plugin install examples/plugins/repo-pulse
masil-agent plugin theme set catppuccin-mocha
masil-agent plugin ls
```

You can also start **Creator Mode** directly from Mobile (**Settings -> Plugins -> Create plugin with AI** or the sparkle button in Workspace Tools) or from the TUI Command Palette (`:` -> **Create Masil plugin with AI**). All plugin Masil API calls over FD 3 obey `scope = read|act|admin`, and approval prompts remain strictly human-only. See [docs/public/plugins.md](docs/public/plugins.md) for the full contract.

## Architecture & privacy notes

- **No proxy in the terminal byte path**: `core/` (C, tmux `next-3.9` fork) owns PTYs, VT parsing, scrollback, and rendering. `masil-agent` (Rust) handles agent detection, TUI views, durable operations, and the opt-in remote gateway.
- **Zero background daemons by default**: ordinary `masil` usage starts no remote server and needs no Node.js runtime.
- **Diagnostics**: run `masil-agent doctor` (or `--versions` / `--bundle DIR`) for a local-only JSON health check with no network calls.

For deeper technical details, see:
- [Installation and updates](docs/public/install.md)
- [Local plugins and AI skill authoring](docs/public/plugins.md)
- [Provider support and setup](docs/public/providers.md)
- [Architecture and compatibility boundaries](docs/public/architecture.md)
- [Verification and benchmarks](docs/public/validation.md) and [recorded performance results](docs/public/performance-2026-11-18.md)

## Development & checks

```sh
make all                                                # build bin/masil and bin/masil-agent
cargo fmt --manifest-path agent/Cargo.toml --check
cargo clippy --locked --manifest-path agent/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path agent/Cargo.toml
make test-ci                                            # core regression suite
make test-relay                                         # wire & relay Rust tests
PYTHONPATH=tests python3 -m unittest -v tests/test_plugins_e2e.py  # local plugin CLI & skill E2E
cd mobile && npm run typecheck && npm test              # mobile app tests
```

## License

masil is licensed under the MIT License. `core/` is derived from tmux and retains its ISC License. Bundled mobile fonts (`Pretendard` and `Cascadia Mono`) are licensed under the SIL Open Font License 1.1. See [NOTICE](NOTICE) for third-party attributions.
