import { z } from "zod";

export const BindingConfidenceSchema = z.enum([
  "reported_by_hook",
  "manually_selected",
  "unverified_heuristic",
  "none",
]);
export type BindingConfidence = z.infer<typeof BindingConfidenceSchema>;

export const ApprovalOptionSchema = z.object({
  id: z.string(),
  label: z.string(),
  key_sequence: z.string().nullable().optional(),
});

export const TimelineItemSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("user_message"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    text: z.string(),
  }),
  z.object({
    kind: z.literal("assistant_message"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    markdown: z.string(),
    model: z.string().nullable().optional(),
  }),
  z.object({
    kind: z.literal("thinking"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    text: z.string(),
    collapsed_default: z.boolean(),
  }),
  z.object({
    kind: z.literal("tool_call"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    name: z.string(),
    summary: z.string(),
    args_json: z.unknown().optional(),
    output: z.string().nullable().optional(),
    status: z.enum(["running", "completed", "failed"]),
  }),
  z.object({
    kind: z.literal("approval_banner"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    request_id: z.string().nullable().optional(),
    prompt: z.string(),
    options: z.array(ApprovalOptionSchema),
    mode: z.enum(["structured_api", "terminal_confirm"]),
  }),
  z.object({
    kind: z.literal("system_notice"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    level: z.string(),
    text: z.string(),
  }),
  z.object({
    kind: z.literal("plugin_card"),
    id: z.string(),
    seq: z.number(),
    ts_ms: z.number(),
    plugin_id: z.string(),
    card_id: z.string(),
    title: z.string(),
    subtitle: z.string().nullable().optional(),
    blocks: z.array(z.unknown()),
  }),
]);
export type TimelineItem = z.infer<typeof TimelineItemSchema>;

export type UiTone =
  | "normal"
  | "muted"
  | "accent"
  | "success"
  | "warning"
  | "danger";

export interface UiButtonDef {
  id: string;
  label: string;
  style?: "primary" | "secondary" | "danger";
  action: string;
  payload?: unknown;
  confirm?: string | null;
}

export type UiBlock =
  | {
      type: "header";
      title: string;
      subtitle?: string | null;
      badge?: string | null;
      tone?: UiTone;
    }
  | { type: "text"; text: string; tone?: UiTone; mono?: boolean }
  | { type: "markdown"; markdown: string }
  | {
      type: "metric_row";
      items: Array<{
        label: string;
        value: string;
        delta?: string | null;
        tone?: UiTone;
      }>;
    }
  | { type: "badge"; label: string; tone?: UiTone }
  | {
      type: "key_value";
      rows: Array<{
        key: string;
        value: string;
        tone?: UiTone;
        mono?: boolean;
        copyable?: boolean;
      }>;
    }
  | {
      type: "list";
      title?: string | null;
      items: Array<{
        id?: string | null;
        title: string;
        subtitle?: string | null;
        badge?: string | null;
        tone?: UiTone;
        action?: string | null;
        payload?: unknown;
      }>;
    }
  | { type: "table"; columns: string[]; rows: string[][] }
  | { type: "code"; language?: string | null; code: string; copyable?: boolean }
  | { type: "diff"; text: string }
  | { type: "progress"; label: string; value: number; max: number; tone?: UiTone }
  | {
      type: "sparkline";
      label?: string | null;
      values: number[];
      unit?: string | null;
      tone?: UiTone;
    }
  | { type: "divider" }
  | { type: "callout"; title?: string | null; text: string; tone?: UiTone }
  | ({ type: "button" } & UiButtonDef)
  | { type: "button_row"; buttons: UiButtonDef[] }
  | {
      type: "toggle";
      id: string;
      label: string;
      value: boolean;
      action: string;
      payload?: unknown;
    }
  | {
      type: "select";
      id: string;
      label: string;
      value: string;
      options: Array<{ value: string; label: string }>;
      action: string;
    }
  | {
      type: "form";
      id: string;
      submit_label: string;
      action: string;
      fields: Array<{
        name: string;
        label: string;
        placeholder?: string | null;
        value?: string | null;
        multiline?: boolean;
      }>;
    }
  | { type: "empty"; title: string; body?: string | null };

export interface UiDocument {
  v?: number;
  title?: string | null;
  subtitle?: string | null;
  blocks: UiBlock[];
  custom_surface?: {
    kind: string;
    title?: string | null;
    data?: unknown;
  } | null;
}

export interface PluginThemeDef {
  id: string;
  plugin_id: string;
  name: string;
  appearance: "dark" | "light" | string;
  canvas: string;
  panel: string;
  raised: string;
  text: string;
  muted: string;
  accent: string;
  accent_text: string;
  warning: string;
  danger: string;
  success: string;
  track: string;
}

export interface PluginPanelDef {
  plugin_id: string;
  id: string;
  title: string;
  icon?: string | null;
  context: "agent" | "global" | string;
  refresh_secs?: number | null;
}

export interface PluginCommandDef {
  plugin_id: string;
  id: string;
  slash: string;
  title: string;
  description: string;
  argument_hint?: string | null;
  context: "agent" | "global" | string;
  icon?: string | null;
}

export interface PluginPresetDef {
  plugin_id: string;
  id: string;
  name: string;
  description: string;
  provider: string;
  model?: string | null;
  effort?: string | null;
  mode?: string | null;
  worktree?: boolean;
  initial_prompt?: string | null;
}

export interface PluginSkillStatus {
  id: string;
  label: string;
  cli_installed: boolean;
  skill_installed: boolean;
  managed: boolean;
  paths: string[];
}

export interface PluginInfo {
  id: string;
  dir: string;
  source: string;
  enabled: boolean;
  active: boolean;
  status: "running" | "disabled" | "error" | string;
  overview?: string | null;
  errors: string[];
  warnings: string[];
  manifest?: {
    name: string;
    version: string;
    description: string;
    author: string;
    scope: string;
    has_entry: boolean;
    theme_count: number;
    panel_count: number;
    command_count: number;
    hook_count: number;
    preset_count: number;
  } | null;
}

export interface PluginCatalog {
  enabled: boolean;
  active_theme?: string | null;
  reload_nonce: number;
  plugins: PluginInfo[];
  themes: PluginThemeDef[];
  panels: PluginPanelDef[];
  commands: PluginCommandDef[];
  presets: PluginPresetDef[];
  skills: PluginSkillStatus[];
}

export interface PluginCommandOutcome {
  toast?: string | null;
  ui?: UiDocument | null;
  draft?: string | null;
  timeline_card_saved?: boolean | null;
  refresh_agents?: boolean;
}

export interface AgentRow {
  remote_id: string;
  server_name: string;
  server_socket: string;
  pane: string;
  pane_id?: string;
  name: string;
  provider: string;
  state: string;
  cwd: string;
  run?: string;
  session_id?: string | null;
  summary?: string;
  pending_approvals?: number;
  pending_questions?: number;
}

export interface TimelinePage {
  binding_confidence: BindingConfidence;
  session_id: string | null;
  source_label: string;
  detected_model?: string | null;
  items: TimelineItem[];
  has_more: boolean;
}

export interface ProviderModelOption {
  id: string;
  label: string;
}

export interface ProviderModelsResponse {
  provider: string;
  models: ProviderModelOption[];
  efforts: string[];
  modes: string[];
  supports_live_model: boolean;
  supports_live_effort: boolean;
  supports_live_mode: boolean;
}

export interface ImportableSession {
  provider: string;
  session_id: string;
  title: string;
  preview: string;
  cwd: string;
  last_active_ms: number;
  source_path?: string | null;
}

export interface InboxItem {
  id?: number;
  seq?: number;
  kind: string;
  agent: string;
  pane: string;
  provider?: string;
  summary?: string | null;
  server_name?: string;
  server_socket?: string;
  observed_ms?: number;
  ack?: boolean;
}

export interface FileEntry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
}

export interface SavedHost {
  hostId: string;
  hostName: string;
  hostPubB64: string;
  deviceId: string;
  deviceSecret: string;
  relayUrl?: string;
  directAddr?: string;
}
