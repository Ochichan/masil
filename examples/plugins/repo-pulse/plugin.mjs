import { execFileSync } from "node:child_process";
import fs from "node:fs";

const raw = fs.readFileSync(0, "utf8").trim();
const req = raw ? JSON.parse(raw) : {};
const cwd = req.agent?.cwd || process.cwd();

function git(args) {
  try {
    return execFileSync("git", args, {
      cwd,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 4000,
    }).trim();
  } catch {
    return "";
  }
}

const isRepo = git(["rev-parse", "--is-inside-work-tree"]) === "true";
const branch = isRepo ? git(["rev-parse", "--abbrev-ref", "HEAD"]) || "HEAD" : "not a git repo";
const statusLines = isRepo
  ? git(["status", "--porcelain"]).split("\n").filter(Boolean)
  : [];
const recentLog = isRepo
  ? git(["log", "-n", "6", "--pretty=format:%h|%s|%cr"])
      .split("\n")
      .filter(Boolean)
      .map((line) => {
        const [hash, subject, when] = line.split("|");
        return { hash: hash || "", subject: subject || "", when: when || "" };
      })
  : [];

const dayCounts = isRepo
  ? (() => {
      const rawDates = git(["log", "-n", "40", "--date=short", "--pretty=format:%ad"])
        .split("\n")
        .filter(Boolean);
      const map = new Map();
      for (const d of rawDates) {
        map.set(d, (map.get(d) || 0) + 1);
      }
      const vals = [...map.values()].slice(0, 7).reverse();
      return vals.length >= 2 ? vals : [0, vals[0] || 0];
    })()
  : [0, 0];

if (req.kind === "command") {
  const note = (req.args || "").trim();
  const summary = `${branch} · ${statusLines.length} dirty file(s)`;
  const blocks = [
    {
      type: "metric_row",
      items: [
        { label: "Branch", value: branch, tone: "accent" },
        {
          label: "Dirty",
          value: String(statusLines.length),
          tone: statusLines.length > 0 ? "warning" : "success",
        },
        { label: "Commits", value: String(recentLog.length), tone: "normal" },
      ],
    },
    ...(note
      ? [{ type: "callout", title: "Note", text: note, tone: "accent" }]
      : []),
    ...(recentLog.length
      ? [
          {
            type: "table",
            columns: ["Hash", "Commit", "When"],
            rows: recentLog.slice(0, 4).map((c) => [c.hash, c.subject, c.when]),
          },
        ]
      : []),
  ];

  console.log(
    JSON.stringify({
      toast: `Pulse: ${summary}`,
      timeline_card: {
        id: "repo-pulse-snapshot",
        title: "Repo Pulse",
        subtitle: summary,
        blocks,
      },
      ui: {
        v: 1,
        title: "Repo Pulse",
        subtitle: cwd,
        blocks,
      },
    })
  );
  process.exit(0);
}

const blocks = [
  {
    type: "header",
    title: "Repo Pulse",
    subtitle: cwd,
    badge: branch,
    tone: isRepo ? "accent" : "muted",
  },
  {
    type: "metric_row",
    items: [
      { label: "Branch", value: branch, tone: "accent" },
      {
        label: "Dirty",
        value: String(statusLines.length),
        tone: statusLines.length > 0 ? "warning" : "success",
      },
      { label: "Recent", value: String(recentLog.length), tone: "normal" },
    ],
  },
  {
    type: "sparkline",
    label: "Commit activity (recent days)",
    values: dayCounts,
    unit: " commits",
    tone: "accent",
  },
  statusLines.length > 0
    ? {
        type: "list",
        title: `Working tree (${statusLines.length})`,
        items: statusLines.slice(0, 12).map((line, i) => ({
          id: `dirty-${i}`,
          title: line.slice(3).trim() || line,
          badge: line.slice(0, 2).trim() || "M",
          tone: "warning",
        })),
      }
    : {
        type: "callout",
        title: "Clean working tree",
        text: "No uncommitted changes in this workspace.",
        tone: "success",
      },
  ...(recentLog.length > 0
    ? [
        {
          type: "table",
          columns: ["Hash", "Message", "Age"],
          rows: recentLog.map((c) => [c.hash, c.subject, c.when]),
        },
      ]
    : []),
  {
    type: "button_row",
    buttons: [
      { id: "refresh", label: "Refresh", style: "primary", action: "refresh" },
    ],
  },
];

console.log(
  JSON.stringify({
    v: 1,
    title: "Repo Pulse",
    subtitle: branch,
    blocks,
  })
);
