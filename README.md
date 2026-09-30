# masil

masil is a terminal multiplexer built from the tmux source. It keeps the tmux commands, options and default key bindings. It adds tools that run coding agents such as Claude Code, Codex and OpenCode in their own TUIs inside panes and manage them from the shell or a side panel.

The repository has two programs. `core/` is tmux at commit `94796f6b1182507efac8a272fc309a79e22e58a5`, on the `next-3.9` development line, with masil's changes. `make core` builds it into `bin/masil`. `agent/` is a Rust program, `masil-agent`, that finds and controls agents and provides the agent UI, layout templates, saved sessions and diagnostics.

masil is still in development. The tests run on macOS arm64 and Debian 13.7 x86_64.

## Build

The build needs a C compiler, make, Python 3.12 or later, pkg-config, libevent, ncurses, utf8proc and jemalloc. Only `masil-agent` needs Cargo. On macOS the build uses the Homebrew copies of these libraries. If m4, autoconf or automake is missing, the build downloads the official release archive, checks its SHA-256 and builds it under `.build/`. It does not install or replace system packages.

```sh
make            # bin/masil and bin/masil-agent
make core       # bin/masil only
./bin/masil -L main new-session -s work
```

## Using masil as tmux

The prefix is `C-b` and the key tables are the tmux defaults. masil adds no default key bindings. It reads the same configuration files as tmux, such as `~/.tmux.conf` and `~/.config/tmux/tmux.conf`.

masil keeps its sockets in a `masil-UID` directory, apart from the `tmux-UID` directory that tmux uses. Its native protocol version is 136, so a masil client and a stock tmux server reject each other's connections. `masil -V` prints `tmux next-3.9` because existing scripts parse that string. For scripts that call `tmux` by name, `compat/tmux` is an optional shim. Add `compat/` to `PATH` only in the shells that need it. masil does not modify a system tmux or Herdr installation.

When masil starts without `-f`, it loads a mouse UI layer before the user's configuration. The status line gets a `masil` menu, a `+` button for new windows and panes, and a `Settings` button that opens `masil-agent settings`. Clicking a pane title renames the pane. Dragging a pane title moves a floating pane or a group of floating panes. The layer also adds a scrollbar. `prefix C-s` saves all sessions, and `prefix C-r` opens a menu of snapshots to restore. A background `masil-agent session autosave` process saves a snapshot every 15 minutes until you set `@masil-autosave` to `off`. The layer binds only keys that have no tmux default. The status line can sit at the top, bottom, left or right of the screen. The settings screen stores its choices as `@masil-*` options in `~/.config/masil/settings.conf`. To start with plain tmux defaults, pass `-f FILE`, for example `masil -f /dev/null`.

## Coding agents

Inside a masil pane, `masil-agent agent` talks to the current server. Outside one, pass `--socket PATH`. It needs no daemon and no configuration.

```sh
masil-agent agent providers
masil-agent agent start backend codex --cwd ~/work/service
masil-agent agent start review claude --cwd ~/work/service --split %0
masil-agent agent list
masil-agent agent prompt backend 'run the test suite'
masil-agent agent wait backend --state idle --timeout 600
masil-agent agent resume backend --name continued
```

masil recognizes 24 providers, including Claude Code, Codex, OpenCode, Gemini CLI, Cursor, Copilot and Amp. `start` launches the provider's own TUI in a new window or split. It passes an argument vector to the program and never types a command line into a shell. A TUI that you start by hand also appears in the list once masil identifies the pane's foreground process, and `attach %N NAME` gives it a name.

Other commands read the screen, send keys, put a draft in a tmux buffer, interrupt, close, and resume the native session for the 18 providers that support resume. `prompt` checks that the agent is idle and is still the same process, pastes the text and presses Enter. Its result means the text reached the TUI. It does not mean the provider accepted it. `wait` reports an observed state such as `idle` or `blocked`, not task success. masil never answers a provider's approval request.

`masil-agent agent ui` opens a full-screen agent list with search, filters, details and actions. `masil-agent agent sidebar` opens the same list as a 34-column side panel that expands with tmux zoom. Both work with mouse and keyboard, in English and Korean, with dark, light and terminal themes. In either view, press `:` for a command palette that lists agents, panes and actions together. `masil-agent agent find QUERY` runs the same search from the shell. Every word in the query must match, and Korean initial consonants match whole syllables, so `ㄱㅂ` finds `개발서버`.

### Session binding

Each launch gets a run ID, and masil records which native session belongs to that run and where the evidence came from. The binding is `requested` when masil started the agent with `--session`. It is `reported` when a provider callback that carries the run's identity names a session. If a later callback names a different session and no resume, clear, switch or fork event explains the change, the binding becomes `conflict`. masil then keeps the first session and refuses to resume from the run. There is no `verified` state, because nothing yet proves which session the TUI on screen shows.

Callbacks come from optional provider hooks. For 17 providers, `masil-agent agent integration export PROVIDER` prints the hook files as JSON, and you install them yourself. `masil-agent agent capabilities TARGET` lists what masil has observed for one agent. `--provider ID` shows what a provider declares.

### Durable operations

`start`, `resume`, `prompt`, `interrupt` and `close` write a record to a SQLite store before they change anything in a pane. The store has one file per server socket under `$XDG_STATE_HOME/masil/operations/`. A client can pass its own key. Actions on a running agent take `--run RUN --operation N`, where RUN comes from `agent get`. Launches take `--boot BOOT --operation ID`, where BOOT is the server's `#{masil_core_boot_id}`. A retry with the same key returns the recorded result and does not repeat the action. When masil cannot tell whether an action took effect, it records `outcome_unknown` and does not send it again. `masil-agent agent operations reconcile` settles expired records from evidence stored in the pane. `masil-agent agent operation resolve KEY --as delivered` or `--as not-delivered` records the user's answer.

## Layouts and saved sessions

`masil-agent layout` creates a new session from a TOML template at `~/.config/masil/layouts/NAME.toml`. Each pane in a template has a working directory and may have a command or an agent. `layout plan NAME` prints what `apply` would do, and `layout apply NAME --yes` does it. It never changes an existing session.

```toml
version = 1
session = "backend"
root = "~/work/service"

[[windows]]
name = "code"
layout = "even-horizontal"

[[windows.panes]]
cwd = "."
focus = true

[[windows.panes]]
cwd = "api"
command = ["nvim", "src/main.rs"]

[[windows]]
name = "agents"
layout = "main-vertical"

[[windows.panes]]
cwd = "."

[[windows.panes]]
agent = { provider = "codex", name = "builder" }
```

The first pane of a window is always a shell pane, so an agent pane cannot come first.

`masil-agent session save`, `list` and `restore` save and restore sessions. `session preview NAME` prints the restore plan without running it.

## Diagnostics

`masil-agent doctor` checks the installed binaries, the operating system, the masil server, the operation store, installed providers and local settings, and prints the result as JSON. It makes no network connections and starts nothing in the background. `--versions` also runs each provider's `--version`, all at once, with a 2 second limit. `--bundle DIR` saves the same report to `DIR/doctor.json` for bug reports.

## Other servers

`masil-agent agent endpoints add` registers up to eight other masil servers, on the same machine or over SSH. Registering an endpoint does not connect to it. `masil-agent agent list --all` shows agents from every enabled endpoint, and `masil-agent agent --endpoint ID COMMAND` runs a command on one of them. SSH connections use your OpenSSH configuration for authentication and host keys.

## OpenCode observer daemon

`masil-agent serve` is an optional daemon that follows configured OpenCode sessions through the OpenCode HTTP API. Nothing starts it automatically. It adds `attention`, which lists agents waiting on an approval or a question, and `watch-agents`, which streams the agent list. `ack` marks an item as read for every client of the daemon until the daemon exits. An ack never approves a provider request. The daemon reads the core through a bridge socket that the masil server opens only if you set `MASIL_BRIDGE_SOCKET` before starting it.

## Design

The tmux C core owns the PTY, the VT parser, the grid and history, key handling and the renderer. Terminal input and output never pass through Rust or a database, and masil has no second terminal parser. `masil-agent` keeps agent metadata in tmux pane options. Without the mouse UI layer, `masil-agent` runs only when you start a command or the UI. The layer adds one `masil-agent session autosave` process per server, and masil starts no other background process by itself.

## Performance

A run on 2026-09-28 compared the masil core with stock tmux, built from the same source with the same compiler flags, and with Herdr 0.8.2. The machine was a macOS arm64 Mac with 64 GiB of memory. The agent management layer was not part of this run.

| Measure | masil | tmux | Herdr 0.8.2 |
| --- | --- | --- | --- |
| Idle server RSS, 50 panes | 4.44 MiB | 4.39 MiB | 48.61 MiB |
| Input to screen, median | 2.67 ms | 2.65 ms | 16.68 ms |

Herdr used less CPU than masil for output in a single pane, 1.87% against 3.16%, and finished writing a 1 MiB burst sooner. Each program ran once in a fixed order, so this run cannot settle differences of a few percent.

## License

masil uses the MIT License. `core/` is derived from tmux and keeps its ISC License. `NOTICE` lists the included third-party code, which is tmux, yyjson and the agent detection manifests from Herdr.
