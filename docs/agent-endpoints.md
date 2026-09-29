# Agent endpoints

rmux can show and control native agents from up to eight other rmux servers. An endpoint is an explicit connection record. Adding one does not start a daemon, open an SSH tunnel, or contact the server.

## Configure endpoints

Endpoint configuration lives at `$XDG_CONFIG_HOME/rmux/endpoints.json`, or `~/.config/rmux/endpoints.json` when `XDG_CONFIG_HOME` is unavailable. rmux writes version 1 of this file with mode `0600`. It creates a missing `rmux` directory with mode `0700`, and accepts an existing owner directory when group and other users cannot write it. It refuses symlinks, non-regular files, files owned by another user, unsafe permissions, unknown JSON fields, duplicate IDs, and more than eight records. The ID `local` is reserved for the server named by `--socket`.

Use `rmux-agent agent endpoints` to change the file:

```sh
rmux-agent agent endpoints list
rmux-agent agent endpoints add build \
  --socket /run/user/1000/rmux-build.sock \
  --host dev@example.net \
  --label "Build host"
rmux-agent agent endpoints disable build
rmux-agent agent endpoints enable build
rmux-agent agent endpoints remove build
```

`--socket` must be an absolute path on the endpoint host. `--binary` accepts the command name `rmux-agent` or an absolute path. A missing `--binary` uses `rmux-agent`. Omit `--host` for another server on the same machine. A local endpoint still runs the configured binary as a child process and uses the same RPC protocol as SSH.

Configuration changes use a private lock and an atomic file replacement. A read or failed connection never changes the file.

## Use an endpoint

Pass `--endpoint ID` before a normal agent command to run it on that server:

```sh
rmux-agent agent --endpoint build list
rmux-agent agent --endpoint build get builder
rmux-agent agent --endpoint build read builder --history
rmux-agent agent --endpoint build start reviewer codex --cwd /srv/project
```

The endpoint supports list, get, read, start, rename, send-keys, draft, prompt, prompt receipts, acknowledge, close, resume, saved views, save, and restore. Paths such as `--cwd`, save files, and restore files refer to the endpoint host.

Use `list --all` with a local socket to aggregate enabled endpoints:

```sh
rmux-agent agent --socket /tmp/local-rmux.sock list --all
```

Aggregated agent IDs use `endpoint::name` when endpoints are configured. The local server uses the reserved endpoint ID `local`. Each endpoint reports its own connection error, so an unavailable host does not discard agents returned by other servers. Cached agents from a failed endpoint are marked stale and reject actions until a fresh identity check succeeds.

## Transport limits

Each request starts one bounded child command. Local endpoints run:

```text
BINARY agent --socket SOCKET rpc
```

SSH endpoints run `ssh` with `BatchMode=yes`, `StrictHostKeyChecking=yes`, and `ConnectTimeout=3`. rmux passes the destination as a separate argument and shell-quotes every remote command argument. It never changes host keys, prompts for credentials, starts a tunnel, or interpolates request data into the remote command. The JSON request is sent only on standard input.

One request may contain at most 256 KiB. Combined standard output and standard error may contain at most 1 MiB. The full call has an eight-second deadline. Each call runs in a new process group. Cancellation or timeout kills the group, including grandchildren, and the timeout path reaps the direct child. rmux does not retry a request. It treats process failures, authentication failures, disconnects, malformed JSON, invalid protocol envelopes, and unsupported protocol versions as errors for that endpoint.

## Identity checks

RPC uses a version 1 envelope and typed operations. List reads the native server boot identity before and after collecting agents. A restart during the read fails that endpoint inventory.

Actions carry the pane ID, server boot ID, PTY generation, agent run ID, native session reference, and observation revision returned by the preceding read. The server resolves the current pane and compares those fields before acting. A changed identity or revision rejects the action. Prompt-receipt lookup may ignore a changed observation revision, but it still requires the same pane, boot, generation, run, and session reference. These checks prevent an old aggregated row from acting on a replacement process.

The stdin RPC command is an internal transport interface. It accepts one JSON request, emits one version 1 JSON envelope, and exits. It does not listen on a network port.

## Native terminal and shared selection

The Agent management desk automatically includes configured endpoints. Rows use endpoint-qualified IDs, and the status line reports connection failures per server. A disabled endpoint stops polling; a removed endpoint disappears. Pending calls are cancelled with their process groups. No reconnect repeats a prompt, start, or other action.

`agent --endpoint build focus builder` attaches the current terminal to the original remote TUI. Selecting a remote row in the desk opens a local native connection window. Terminal bytes travel through native rmux/OpenSSH, not JSON. Detach with the remote rmux prefix and `d`; the provider continues running.

A fresh attachment is refused when another client is already attached to the target session or viewing a linked target window. This prevents changing another client's current window or active pane. Existing connections are reused only when their local PTY generation and remote client lease still match. If the connection has navigated to another remote pane, return to the intended Agent in that connection window before selecting it again. A new connection is reported successful only after its remote client lease is verified.

The desk captures endpoint configuration when opening a Start dialog. Replacing that endpoint while the dialog is open rejects the submission. Remote starts require an explicit directory on that host. Endpoint changes are applied while the desk is open.

## Validation boundary

Tests use two isolated real rmux servers, a provider fixture that never calls a model, strict SSH argument checks, and actual native PTY attachment. They cover same-name/pane-ID routing, restart and disconnection, stale input rejection, failed or cancelled transports, and shared selection. No real SSH destination or provider account was configured as part of this change. Both hosts need this rmux/rmux-agent build; older helpers without RPC version 1 are reported unavailable.
