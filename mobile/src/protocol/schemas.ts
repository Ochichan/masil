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

export const InteractivePromptKindSchema = z.enum([
  "permission",
  "question",
  "input",
  "confirmation",
]);
export type InteractivePromptKind = z.infer<typeof InteractivePromptKindSchema>;

export const InteractivePromptModeSchema = z.enum([
  "structured_api",
  "terminal_tui",
]);
export type InteractivePromptMode = z.infer<typeof InteractivePromptModeSchema>;

export const InteractiveOptionSchema = z.object({
  id: z.string(),
  index: z.number(),
  label: z.string(),
  description: z.string().nullable().optional(),
  selected: z.boolean().optional().default(false),
  checked: z.boolean().nullable().optional(),
  hotkey: z.string().nullable().optional(),
  is_custom_input: z.boolean().optional().default(false),
  is_destructive: z.boolean().optional().default(false),
  disabled_reason: z.string().nullable().optional(),
});
export type InteractiveOption = z.infer<typeof InteractiveOptionSchema>;

export const StructuredQuestionOptionSchema = z.object({
  label: z.string(),
  description: z.string().nullable().optional(),
});
export type StructuredQuestionOption = z.infer<
  typeof StructuredQuestionOptionSchema
>;

export const StructuredQuestionSchema = z.object({
  header: z.string().optional().default(""),
  question: z.string(),
  options: z.array(StructuredQuestionOptionSchema).optional().default([]),
  custom: z.boolean().optional().default(true),
  multiple: z.boolean().optional().default(false),
});
export type StructuredQuestion = z.infer<typeof StructuredQuestionSchema>;

export const InteractivePromptSchema = z.object({
  id: z.string(),
  kind: InteractivePromptKindSchema,
  mode: InteractivePromptModeSchema,
  request_id: z.string().nullable().optional(),
  title: z.string(),
  body: z.string().optional().default(""),
  step_label: z.string().nullable().optional(),
  options: z.array(InteractiveOptionSchema).optional().default([]),
  selected_index: z.number().optional().default(0),
  multi_select: z.boolean().optional().default(false),
  allows_free_text: z.boolean().optional().default(false),
  free_text_only: z.boolean().optional().default(false),
  allows_cancel: z.boolean().optional().default(true),
  can_navigate_tabs: z.boolean().optional().default(false),
  always_patterns: z.array(z.string()).optional().default([]),
  session_peers_count: z.number().optional().default(0),
  is_subagent: z.boolean().optional().default(false),
  questions: z.array(StructuredQuestionSchema).optional().default([]),
  screen_excerpt: z.string().nullable().optional(),
  hint_line: z.string().nullable().optional(),
  risk: z
    .object({
      tier: z.enum(["low", "medium", "high", "critical"]),
      label: z.string(),
      score: z.number(),
      reasons: z.array(z.string()).optional().default([]),
      command: z.string().nullable().optional(),
      truncated: z.boolean().optional().default(false),
      matched_rule_id: z.string().nullable().optional(),
      matched_action: z.string().nullable().optional(),
      batch_eligible: z.boolean().optional().default(false),
      prompt_digest: z.string(),
    })
    .nullable()
    .optional(),
});
export type InteractivePrompt = z.infer<typeof InteractivePromptSchema>;

export const PromptRiskTierSchema = z.enum(["low", "medium", "high", "critical"]);
export type PromptRiskTier = z.infer<typeof PromptRiskTierSchema>;

export const PromptRiskAssessmentSchema = z.object({
  tier: PromptRiskTierSchema,
  label: z.string(),
  score: z.number(),
  reasons: z.array(z.string()).optional().default([]),
  command: z.string().nullable().optional(),
  truncated: z.boolean().optional().default(false),
  matched_rule_id: z.string().nullable().optional(),
  matched_action: z.string().nullable().optional(),
  batch_eligible: z.boolean().optional().default(false),
  prompt_digest: z.string(),
});
export type PromptRiskAssessment = z.infer<typeof PromptRiskAssessmentSchema>;

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

export interface AuditSummary {
  id: number;
  verdict: string;
  risk_score: number;
  claim_verified?: boolean | null;
  critical_count: number;
  high_count: number;
  warning_count: number;
  info_count: number;
  hunk_count: number;
  checkpoint_id?: string | null;
  prev_checkpoint_id?: string | null;
  created_ms: number;
}

export interface AuditFinding {
  id?: string;
  rule_id: string;
  category?: string;
  severity: "critical" | "high" | "warning" | "info" | string;
  title: string;
  detail: string;
  file?: string | null;
  line?: number | null;
  evidence?: string | null;
}

export interface RiskHunk {
  id?: string;
  file: string;
  old_start: number;
  new_start: number;
  added: number;
  removed: number;
  risk_score: number;
  reason: string;
  diff: string;
}

export interface TurnAuditReport {
  id: number;
  pane_id: string;
  run_id: string;
  checkpoint_id?: string | null;
  prev_checkpoint_id?: string | null;
  verdict: string;
  risk_score: number;
  claim_verified?: boolean | null;
  findings: AuditFinding[];
  hunks: RiskHunk[];
  created_ms: number;
}

export type CollisionSeverity =
  | "clean"
  | "overlap"
  | "semantic_risk"
  | "hard_conflict"
  | "same_directory_race";

export interface CollisionPair {
  repo_key: string;
  agent_a: string;
  agent_b: string;
  pane_a?: string;
  pane_b?: string;
  worktree_a?: string;
  worktree_b?: string;
  severity: CollisionSeverity;
  files: string[];
  details: string[];
  last_warned_ms?: number | null;
  computed_ms: number;
}

export interface CircuitBreakerState {
  run_id: string;
  pane_id: string;
  tripped: boolean;
  tripped_reason?: string | null;
  tripped_at_ms?: number | null;
  same_error_streak: number;
  no_progress_streak: number;
  bypass_until_turn: number;
  pre_oscillation_checkpoint_id?: number | null;
  pane_actively_focused?: boolean;
  error_excerpt?: string | null;
}

export interface CacheWarmthState {
  run_id: string;
  pane_id: string;
  provider: string;
  ttl_secs: number;
  last_turn_at_ms: number;
  est_context_tokens: number;
  warm_remaining_ms: number;
  tier: "hot" | "expiring" | "cold" | string;
  badge: string;
  invalidate_warning: boolean;
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
  workspace?: string;
  run?: string;
  process?: string;
  revision?: string;
  seen?: boolean;
  stale?: boolean;
  returned_idle?: boolean;
  session_id?: string | null;
  summary?: string;
  permission_count?: number;
  question_count?: number;
  subagent_count?: number;
  pending_approvals?: number;
  pending_questions?: number;
  capabilities?: Record<string, unknown>;
  audit_summary?: AuditSummary | null;
  breaker?: CircuitBreakerState | null;
  cache_warmth?: CacheWarmthState | null;
  collision_severity?: CollisionSeverity | null;
  collision_peers?: string[];
  sandbox?: string | null;
  dvr_turn_count?: number;
  last_policy_audit?: PermissionAuditEntry | null;
}

export interface TimelinePage {
  binding_confidence: BindingConfidence;
  session_id: string | null;
  source_label: string;
  detected_model?: string | null;
  items: TimelineItem[];
  active_prompts?: InteractivePrompt[];
  has_more: boolean;
}

export interface QueuedPromptItem {
  id: number | string;
  body?: string;
  text?: string;
  position?: number;
  revision?: number;
  attachments?: string[];
  created_ms?: number;
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
  directAddrs?: string[];
}

export interface DetectedPort {
  port: number;
  url: string;
  process: string;
  pid?: number | null;
  source: "process_tree" | "pty_url" | "host_loopback" | string;
  recommended: boolean;
  label?: string | null;
}

export interface TunnelPortDiscovery {
  recommended: DetectedPort[];
  host_ports: DetectedPort[];
  denied_ports: number[];
}

export interface FocusShieldStatus {
  active: boolean;
  started_ms?: number | null;
  until_ms?: number | null;
  remaining_ms: number;
  reason?: string | null;
  held_count: number;
  updated_ms: number;
}

export interface BriefingItem {
  id: string;
  agent_name: string;
  remote_id: string;
  pane: string;
  server_socket: string;
  provider: string;
  title: string;
  subtitle: string;
  badge: string;
  action: string;
  bucket: "needs_decision" | "ready_to_merge" | "review_recommended" | string;
}

export interface BriefingMetrics {
  completed_turns: number;
  files_touched: number;
  lines_changed: number;
  breaker_interventions: number;
  active_collisions: number;
  held_notifications: number;
  active_agents: number;
}

export interface BriefingReport {
  generated_ms: number;
  since_ms: number;
  window_label: string;
  focus?: FocusShieldStatus | null;
  metrics: BriefingMetrics;
  needs_decision: BriefingItem[];
  ready_to_merge: BriefingItem[];
  review_recommended: BriefingItem[];
  ui: UiDocument;
}

export interface LivePulsePayload {
  host_id: string;
  host_name: string;
  working_count: number;
  blocked_count: number;
  done_count: number;
  alert_count: number;
  top_agent_alias?: string | null;
  top_event_kind?: string | null;
  top_summary_line?: string | null;
  deep_link: string;
  updated_at: number;
}

export interface LivePulsePreferences {
  enabled: boolean;
  summaryEnabled: boolean;
}

export interface AssetUploadResultEvent {
  upload_id: number;
  ok: boolean;
  agent_id?: string;
  delivery_mode?: "send" | "queue" | "prefill" | string;
  saved_path?: string;
  prompt_path?: string;
  in_git_worktree?: boolean;
  size_bytes?: number;
  structured_prompt?: string;
  error?: string;
}

export interface PermissionPolicyRule {
  id: string;
  scope: string;
  match_command?: string[];
  match_path?: string[];
  max_risk: string;
  action: "auto_approve" | "suggest_batch" | "always_confirm" | "deny" | string;
  enabled: boolean;
  source?: string;
}

export interface PermissionAuditEntry {
  id: number;
  run_id: string;
  pane_id: string;
  rule_id?: string | null;
  provider: string;
  command: string;
  risk_tier: PromptRiskTier | string;
  decision: string;
  prompt_digest: string;
  screen_excerpt?: string | null;
  revoked_ms?: number | null;
  created_ms: number;
}

export interface SandboxStatus {
  available: boolean;
  default_enabled: boolean;
  default_profile: string;
  fail_closed: boolean;
  backend: string;
  detail: string;
}

export interface ArenaRecord {
  id: string;
  repo_key: string;
  base_commit: string;
  base_tree_oid?: string | null;
  prompt: string;
  verify_cmd?: string | null;
  status: string;
  winner_racer?: string | null;
  created_ms: number;
  updated_ms: number;
}

export interface ArenaRacerRecord {
  arena_id: string;
  racer_name: string;
  provider: string;
  model?: string | null;
  worktree_id: string;
  pane_id: string;
  run_id: string;
  status: string;
  elapsed_ms?: number | null;
  verify_exit_code?: number | null;
  audit_verdict?: string | null;
  files_changed?: number | null;
  insertions?: number | null;
  deletions?: number | null;
}

export interface ArenaListItem {
  arena: ArenaRecord;
  racers: ArenaRacerRecord[];
}

export interface DvrTurnRecord {
  id: number;
  run_id: string;
  pane_id: string;
  turn_seq: number;
  provider: string;
  prompt_preview?: string | null;
  checkpoint_id: string;
  tree_oid: string;
  audit_verdict?: string | null;
  files_changed: number;
  terminal_excerpt?: string | null;
  pinned: boolean;
  pin_note?: string | null;
  created_ms: number;
}

export interface DvrInspectResult {
  turn: DvrTurnRecord;
  prev_turn_seq?: number | null;
  diff: string;
  diff_files: number;
  total_turns: number;
}
