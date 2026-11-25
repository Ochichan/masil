# Masil UI Spec v1 reference

Every panel handler, command `ui` payload, and `timeline_card` uses the same JSON block tree rendered natively in both the Rust Ratatui TUI and the React Native Mobile app.

## Document envelope

```json
{
  "v": 1,
  "title": "Optional title",
  "subtitle": "Optional subtitle",
  "blocks": [],
  "storage_patch": {}
}
```

Limits: 256 KiB serialized JSON, at most 200 blocks.

## Tones

`normal` | `muted` | `accent` | `success` | `warning` | `danger`

## Block types

| `type` | Fields | Notes |
| --- | --- | --- |
| `header` | `title`, `subtitle?`, `badge?`, `tone?` | Top summary banner |
| `text` | `text`, `tone?`, `mono?` | Plain or monospace paragraph |
| `markdown` | `markdown` | Headings, lists, bold, inline code, code blocks |
| `metric_row` | `items: [{ label, value, delta?, tone? }]` | 1 to 6 KPI tiles in one row |
| `badge` | `label`, `tone?` | Single status pill |
| `key_value` | `rows: [{ key, value, tone?, mono?, copyable? }]` | Structured metadata rows |
| `list` | `title?`, `items: [{ id?, title, subtitle?, badge?, tone?, action?, payload? }]` | Optional tappable rows that trigger `action` |
| `table` | `columns: [string]`, `rows: [[string]]` | Every row length must equal `columns.length` |
| `code` | `language?`, `code`, `copyable?` | Syntax/mono block with tap-to-copy on mobile |
| `diff` | `text` | Unified diff with +/- coloring |
| `progress` | `label`, `value`, `max`, `tone?` | `max > 0`, both finite |
| `sparkline` | `label?`, `values: [number]`, `unit?`, `tone?` | SVG sparkline on mobile, Unicode blocks in TUI |
| `divider` | *(none)* | Hairline separator |
| `callout` | `title?`, `text`, `tone` | Highlighted alert card |
| `button` | `id`, `label`, `style: primary|secondary|danger`, `action`, `payload?`, `confirm?` | Re-invokes the panel handler with `request.action` |
| `button_row` | `buttons: [button]` | 1 to 4 buttons side by side |
| `toggle` | `id`, `label`, `value`, `action`, `payload?` | Boolean switch triggering `action` with `{ value: !value }` |
| `select` | `id`, `label`, `value`, `options: [{ value, label }]`, `action` | Chip selector triggering `action` with `{ value }` |
| `form` | `id`, `submit_label`, `action`, `fields: [{ name, label, placeholder?, value?, multiline? }]` | Sends `request.form_values` on submit |
| `empty` | `title`, `body?` | Clean empty state |

## Theme `theme.toml` contract

```toml
canvas = "#1e1e2e"       # required
panel = "#181825"        # required
text = "#cdd6f4"         # required (keep >= 4.5:1 contrast against panel)
accent = "#cba6f7"       # required
# Optional overrides (auto-derived if omitted):
raised = "#313244"
muted = "#a6adc8"
accent_text = "#11111b"
warning = "#f9e2af"
danger = "#f38ba8"
success = "#a6e3a1"
track = "#45475a"
```
