# Masil plugin examples

## 1. Theme-only plugin (zero code)

`~/.config/masil/plugins/tokyo-night/masil-plugin.toml`:

```toml
schema_version = 1
id = "tokyo-night"
name = "Tokyo Night"
version = "0.1.0"
description = "Tokyo Night dark palette for Masil TUI and Mobile."

[[themes]]
id = "tokyo-night"
name = "Tokyo Night"
appearance = "dark"
file = "theme.toml"
```

`~/.config/masil/plugins/tokyo-night/theme.toml`:

```toml
canvas = "#1a1b26"
panel = "#16161e"
raised = "#24283b"
text = "#c0caf5"
muted = "#7aa2f7"
accent = "#7aa2f7"
accent_text = "#15161e"
warning = "#e0af68"
danger = "#f7768e"
success = "#9ece6a"
track = "#3b4261"
```

## 2. Interactive panel + slash command (Node ESM)

`masil-plugin.toml`:

```toml
schema_version = 1
id = "todo-board"
name = "Todo Board"
version = "0.1.0"
description = "Track quick tasks per project inside Masil."
scope = "read"
entry = ["node", "plugin.mjs"]

[[panels]]
id = "board"
title = "Tasks"
icon = "CheckSquare"
context = "agent"
handler = "panel.board"

[[commands]]
id = "todo"
slash = "todo"
title = "Add task"
description = "Add a quick task item"
argument_hint = "<text>"
context = "agent"
handler = "command.todo"
```

`plugin.mjs`:

```js
import fs from "node:fs";

const req = JSON.parse(fs.readFileSync(0, "utf8") || "{}");
const tasks = Array.isArray(req.storage?.tasks) ? [...req.storage.tasks] : [];

if (req.kind === "command" && req.handler === "command.todo") {
  const title = (req.args || "").trim();
  if (!title) {
    console.log(JSON.stringify({ toast: "Usage: /todo <task>" }));
    process.exit(0);
  }
  tasks.push({ id: String(Date.now()), title, done: false });
  console.log(JSON.stringify({
    toast: `Added: ${title}`,
    storage_patch: { tasks },
    timeline_card: {
      id: "todo-summary",
      title: "Task added",
      subtitle: `${tasks.filter((t) => !t.done).length} open`,
      blocks: [{ type: "callout", text: title, tone: "accent" }],
    },
  }));
  process.exit(0);
}

if (req.action === "add" && req.form_values?.title?.trim()) {
  tasks.push({ id: String(Date.now()), title: req.form_values.title.trim(), done: false });
} else if (req.action === "toggle" && req.payload?.id) {
  for (const t of tasks) {
    if (t.id === req.payload.id) t.done = !t.done;
  }
}

const openCount = tasks.filter((t) => !t.done).length;
const blocks = [
  {
    type: "metric_row",
    items: [
      { label: "Open", value: String(openCount), tone: openCount ? "warning" : "success" },
      { label: "Done", value: String(tasks.length - openCount), tone: "success" },
    ],
  },
  tasks.length
    ? {
        type: "list",
        title: "Items",
        items: tasks.map((t) => ({
          id: t.id,
          title: t.done ? `[x] ${t.title}` : `[ ] ${t.title}`,
          badge: t.done ? "done" : "open",
          tone: t.done ? "muted" : "normal",
          action: "toggle",
          payload: { id: t.id },
        })),
      }
    : { type: "empty", title: "No tasks yet", body: "Add one below or run /todo <text>." },
  {
    type: "form",
    id: "new-task",
    submit_label: "Add task",
    action: "add",
    fields: [{ name: "title", label: "New task", placeholder: "Fix flaky test..." }],
  },
];

console.log(JSON.stringify({
  v: 1,
  title: "Tasks",
  blocks,
  storage_patch: { tasks },
}));
```

## 3. Lifecycle hook (Python 3)

```python
#!/usr/bin/env python3
import json, sys

req = json.load(sys.stdin)
event = req.get("event")
data = req.get("event_data") or {}

if event == "agent.state_changed" and data.get("state") == "blocked":
    agent = data.get("name") or data.get("pane") or "agent"
    print(json.dumps({
        "ok": True,
        "timeline_card": {
            "id": "blocked-alert",
            "title": "Needs attention",
            "subtitle": agent,
            "blocks": [{"type": "callout", "text": f"{agent} is waiting for input.", "tone": "warning"}],
        },
    }))
else:
    print(json.dumps({"ok": True}))
```
