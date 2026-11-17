# Architecture and compatibility

The C core is derived from tmux commit
`94796f6b1182507efac8a272fc309a79e22e58a5`, on its next-3.9 development line.
It owns PTYs, VT parsing, the terminal grid and history, input and rendering.
Rust does not parse or forward the terminal byte stream. `core/UPSTREAM.json`
records the imported file hashes; NOTICE records the included licenses.

The Rust `masil-agent` program owns agent inspection, the desk, layouts,
snapshots, diagnostics and management commands. It uses pane metadata, the
core bridge and a per-server SQLite operation store. The optional coordinator
handles resident features such as inbox and scheduling. These separate stores
have different purposes: a recorded command is not proof of provider acceptance.

## What carries over from tmux

The pinned core retains tmux command syntax, options, configuration lookup,
default key tables and control mode. Existing `.tmux.conf` files can be used.
The public regression gate checks selected contracts, not every plugin or
terminal combination. A fixed upstream source is not a promise to match every
past or future tmux release.

Socket directories and native protocol are separate from stock tmux. A masil
client cannot attach to a stock server. Scripts that invoke `tmux` by name can
use the optional `compat/tmux` shim in a selected shell's PATH. A script that
uses an absolute stock-tmux path needs its own configuration change.

Without `-f`, masil adds its mouse UI before the user's configuration. To test
plain core behavior use a new isolated server with `-f /dev/null`. This does
not change the configuration of a server that is already running.

## Management guarantees and limits

Actions check the agent's run identity. Supported durable operations record
intent before sending input or changing a pane. A retry with the same operation
key retrieves its recorded result. An unknown outcome stays unknown until
reconciliation or a person's resolution; it is not automatically resent.

`prompt` requires an observed idle foreground agent. That guard depends on the
quality of the provider's state signal. Missing hooks or changed TUI text can
misclassify state. Delivery, provider acceptance and successful task completion
are distinct results. `wait --state idle` is not a test for task success.

The extension API has read, act and admin scopes. It excludes direct approval
answers and arbitrary key injection. These restrictions prevent mistakes in
API use; they do not sandbox code running with the same user's shell access.

Agent management is bounded to 64 panes per server and eight registered
endpoints. SSH uses the user's authentication and host-key configuration.
Remote paths refer to the remote host. Some remote operations are deliberately
read-only; remote availability is not equivalent to local feature parity.

Snapshots can recreate shells, layouts and supported native agent sessions.
They do not preserve arbitrary processes across a server or machine restart.
Native session identity is evidence from launch arguments and callbacks, not
proof of the conversation currently visible in a TUI.
