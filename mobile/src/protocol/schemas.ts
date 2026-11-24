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
]);
export type TimelineItem = z.infer<typeof TimelineItemSchema>;

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
