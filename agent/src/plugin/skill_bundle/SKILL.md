---
name: masil-plugin
description: Build, edit, theme, validate, install, reload, or troubleshoot local Masil plugins for both TUI and Mobile. Use when the user asks to create or customize a Masil theme, workspace panel, slash command, timeline card, lifecycle hook, or agent launch preset, or asks to debug or reload a Masil plugin.
---

<!-- masil-managed-skill:v1 -->

# Masil plugins

Build and verify Masil plugins directly on the host machine. Changes take effect live in both the Rust TUI and the Mobile app with no binary or APK rebuild.

## Workflow

Follow these steps in order and do not finish until step 6 succeeds:

1. **Inspect the live binary contract.** Run `masil-agent plugin schema --json` first so your manifest, UI blocks, and hook names match the installed binary.
2. **Choose a plugin folder.**
   - New personal plugin: scaffold directly in `~/.config/masil/plugins/<id>` using `masil-agent plugin init <id> --template <theme|command|panel|hook|preset|full> --lang <node|python|bash>`.
   - Existing local repo or folder: edit the folder in place and register it with `masil-agent plugin install /abs/path/to/plugin --id <id>`.
3. **Choose the smallest contribution set that solves the request.**
   - **Theme (`[[themes]]` + `theme.toml`)**: Omarchy-style palette (`canvas`, `panel`, `text`, `accent` plus optional overrides). Zero code required.
   - **Workspace panel (`[[panels]]`)**: Interactive tab in Mobile Workspace Tools and overlay in TUI Command Palette (`:`). Returns a Masil UI Spec v1 JSON document.
   - **Slash command (`[[commands]]`)**: `/slash [args]` in Mobile composer and Command Palette action in TUI. Can return `toast`, `ui`, `draft`, or `timeline_card`.
   - **Timeline card**: Returned as `timeline_card` from a command or hook, rendered inline inside the agent's conversation timeline.
   - **Lifecycle hook (`[[hooks]]`)**: Reacts to `agent.state_changed`, `agent.started`, `agent.closed`, `inbox.event`, `theme.changed`, or `prompt.before`.
   - **Agent preset (`[[presets]]`)**: One-tap launch profile (provider, model, effort, mode, worktree, initial prompt) in Mobile New Agent sheet.
4. **Implement zero-dependency handlers by default.** Prefer Node ESM (`plugin.mjs`) or Python 3 (`plugin.py`) with no third-party install step unless the user asks for an external SDK. Read `references/ui-spec.md` for all UI blocks and `references/examples.md` for complete patterns.
5. **Validate and test before reloading.**
   ```bash
   masil-agent plugin check <id>
   masil-agent plugin test <id> panel <panel-id> --cwd "$(pwd)"
   masil-agent plugin test <id> command <command-id> --args ""
   ```
   Fix every schema, contrast, or runtime error before moving on.
6. **Enable, reload, and verify live.**
   ```bash
   masil-agent plugin enable <id>
   masil-agent plugin reload <id>
   masil-agent plugin ls
   ```
   For theme plugins, apply immediately if the user requested it:
   ```bash
   masil-agent plugin theme set <theme-id>
   ```
   Verify `masil-agent plugin ls` reports zero errors for `<id>`. If anything fails, inspect `masil-agent plugin logs <id>`, fix the root cause, and repeat steps 5 and 6.

## Handler contract summary

- Masil executes `entry` with cwd set to the plugin folder, passes one JSON request object on `stdin`, and reads one JSON response object from `stdout`.
- Environment variables: `MASIL_PLUGIN_ID`, `MASIL_PLUGIN_DIR`, `MASIL_PLUGIN_STATE_DIR`, `MASIL_API_VERSION`, `MASIL_API_FD=3`.
- Keep logs on `stderr` (captured into `$XDG_STATE_HOME/masil/plugins/<id>/plugin.log`). Keep `stdout` strictly valid JSON.
- Per-plugin JSON key-value storage is passed in `request.storage` and persisted when the handler returns `storage_patch`.
- Masil API v1 is available on FD 3 according to `scope = "read" | "act" | "admin"`. Never attempt to automate approval prompts; approvals stay human-only.

## References

- Read [references/ui-spec.md](references/ui-spec.md) when building panels, command UI sheets, or timeline cards.
- Read [references/examples.md](references/examples.md) when you need working manifest and handler templates.
