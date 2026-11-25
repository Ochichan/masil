# Masil plugins

Masil ships a local plugin system designed for AI-assisted authoring with Claude Code, Codex, and Antigravity (`agy`). Every plugin lives in `~/.config/masil/plugins/<id>/` (or any local folder registered by path) and hot-reloads live across both the Rust TUI and the Mobile app without recompiling binaries or rebuilding the APK.

## Install the official AI skill

Install the bundled `masil-plugin` skill so Claude Code, Codex, and Antigravity know the exact manifest, UI block spec, and validation loop:

```sh
masil-agent plugin skill install --target all
masil-agent plugin skill status
```

Target paths:
- Claude Code: `~/.claude/skills/masil-plugin/` (respects `CLAUDE_CONFIG_DIR`)
- Codex: `~/.codex/skills/masil-plugin/` (respects `CODEX_HOME`) and `~/.agents/skills/masil-plugin/`
- Antigravity (`agy`): `~/.gemini/config/skills/masil-plugin/`

You can also toggle these skills directly from the Mobile app under **Settings -> AI Plugin Skills**, or launch **Create plugin with AI** from Mobile Settings, Workspace Tools, or the TUI Command Palette (`:`).

## Contribution types

One plugin can combine any subset of six contribution types in `masil-plugin.toml`:

1. **Themes (`[[themes]]` + `theme.toml`)**: Omarchy-style palette (`canvas`, `panel`, `text`, `accent` plus optional overrides). Zero code required. Apply live to both TUI and Mobile with `masil-agent plugin theme set <id>`.
2. **Workspace panels (`[[panels]]`)**: Interactive tabs in Mobile Workspace Tools and overlays in the TUI Command Palette (`:`). Handlers return Masil UI Spec v1 JSON blocks (`header`, `metric_row`, `table`, `list`, `markdown`, `code`, `diff`, `progress`, `sparkline`, `callout`, `button`, `button_row`, `toggle`, `select`, `form`, `empty`).
3. **Slash commands (`[[commands]]`)**: Autocomplete on `/` in the Mobile composer and appear in the TUI Command Palette (`:`). Commands can return a `toast`, structured `ui` sheet, composer `draft`, or inline `timeline_card`.
4. **Timeline cards**: Structured UI cards persisted per agent and rendered inline inside the Mobile conversation view.
5. **Lifecycle hooks (`[[hooks]]`)**: React to `agent.state_changed`, `agent.started`, `agent.closed`, `inbox.event`, `theme.changed`, and `prompt.before`.
6. **Agent presets (`[[presets]]`)**: One-tap launch profiles (provider, model, effort, mode, worktree, initial prompt) in the Mobile New Agent sheet.

## CLI workflow

```sh
masil-agent plugin schema --json
masil-agent plugin init my-plugin --template panel --lang node
masil-agent plugin check my-plugin
masil-agent plugin test my-plugin panel main --cwd "$(pwd)"
masil-agent plugin enable my-plugin
masil-agent plugin reload my-plugin
masil-agent plugin ls
masil-agent plugin logs my-plugin
```

## Starter examples

- [examples/plugins/catppuccin-mocha](../../examples/plugins/catppuccin-mocha/masil-plugin.toml): zero-code dark theme plugin.
- [examples/plugins/repo-pulse](../../examples/plugins/repo-pulse/masil-plugin.toml): live git workspace panel, sparkline, and `/pulse` timeline card command.

## Safety boundary

Plugins run as local processes under your user account. Masil API calls over FD 3 (`MASIL_API_FD=3`) enforce the plugin's declared `scope` (`read`, `act`, or `admin`). Approvals and interactive questions remain human-only; `answer` and raw `send-keys` are never exposed to plugin scopes.
