//! Phase 1C: Prompt Cache Warmth Gauge (`agent/src/guard/cache_warmth.rs`, D-06).
//! Strictly passive, honest estimated TTL tracking — NEVER sends keepalive
//! pings to any provider API.

use super::config::GuardConfig;
use crate::managed::{CacheWarmthRecord, Store};
use crate::observation::now_ms;
use masil_wire::rpc::TimelineItem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

/// Minimum estimated context tokens to highlight Warm-first sorting and
/// cache-invalidation warnings before switching models/effort (D-06).
pub const HIGH_CONTEXT_TOKEN_THRESHOLD: u64 = 30_000;

/// Threshold under which a warm cache is shown as expiring soon (`⏳`).
pub const EXPIRING_SOON_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheWarmthSnapshot {
    pub run_id: String,
    pub pane_id: String,
    pub provider: String,
    pub ttl_secs: u64,
    pub last_turn_at_ms: u64,
    pub warm_remaining_ms: u64,
    /// `"hot" | "expiring" | "cold"`
    pub tier: String,
    pub est_context_tokens: u64,
    pub estimated: bool,
    pub invalidate_warning: bool,
    pub badge_text: String,
}

impl CacheWarmthSnapshot {
    pub fn from_record(rec: &CacheWarmthRecord, at_ms: u64) -> Self {
        let ttl_ms = rec.ttl_secs.saturating_mul(1000);
        let elapsed = at_ms.saturating_sub(rec.last_turn_at_ms);
        let remaining = ttl_ms.saturating_sub(elapsed);
        let tier = if remaining == 0 {
            "cold"
        } else if remaining < EXPIRING_SOON_MS {
            "expiring"
        } else {
            "hot"
        };
        let badge_text = format_badge(tier, remaining);
        let invalidate_warning =
            remaining > 0 && rec.est_context_tokens >= HIGH_CONTEXT_TOKEN_THRESHOLD;
        Self {
            run_id: rec.run_id.clone(),
            pane_id: rec.pane_id.clone(),
            provider: rec.provider.clone(),
            ttl_secs: rec.ttl_secs,
            last_turn_at_ms: rec.last_turn_at_ms,
            warm_remaining_ms: remaining,
            tier: tier.to_owned(),
            est_context_tokens: rec.est_context_tokens,
            estimated: true,
            invalidate_warning,
            badge_text,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "pane_id": self.pane_id,
            "provider": self.provider,
            "ttl_secs": self.ttl_secs,
            "last_turn_at_ms": self.last_turn_at_ms,
            "warm_remaining_ms": self.warm_remaining_ms,
            "tier": self.tier,
            "est_context_tokens": self.est_context_tokens,
            "estimated": self.estimated,
            "invalidate_warning": self.invalidate_warning,
            "badge_text": self.badge_text,
        })
    }
}

/// Resolves the estimated prompt cache TTL in seconds for `provider` (D-06).
pub fn resolve_ttl_secs(provider: &str, overrides: &HashMap<String, u64>) -> u64 {
    let lower = provider.trim().to_ascii_lowercase();
    if let Some(&secs) = overrides.get(&lower) {
        return secs.clamp(30, 86_400);
    }
    match lower.as_str() {
        "claude" | "claude-code" => {
            let is_api_key = std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty());
            let tier_key = if is_api_key {
                "claude_api"
            } else {
                "claude_sub"
            };
            if let Some(&secs) = overrides.get(tier_key) {
                return secs.clamp(30, 86_400);
            }
            if is_api_key { 300 } else { 3600 }
        }
        "codex" => 600,
        "gemini" | "agy" | "antigravity" => 300,
        _ => 300,
    }
}

/// Passive, deterministic context token estimator from transcript items
/// (~4 chars/token heuristic; never calls any external endpoint).
pub fn estimate_context_tokens(items: &[TimelineItem]) -> u64 {
    let mut chars: usize = 0;
    for item in items {
        match item {
            TimelineItem::UserMessage { text, .. } => chars += text.len(),
            TimelineItem::AssistantMessage { markdown, .. } => chars += markdown.len(),
            TimelineItem::Thinking { text, .. } => chars += text.len(),
            TimelineItem::ToolCall {
                summary, output, ..
            } => {
                chars += summary.len();
                if let Some(out) = output {
                    chars += out.len().min(16_000);
                }
            }
            TimelineItem::SystemNotice { text, .. } => chars += text.len(),
            TimelineItem::ApprovalBanner { prompt, .. } => chars += prompt.len(),
            TimelineItem::PluginCard { title, .. } => chars += title.len(),
        }
    }
    // Add baseline system prompt overhead (~4k tokens) when conversation is active.
    let raw_tokens = (chars as u64) / 4;
    if raw_tokens > 0 {
        raw_tokens.saturating_add(4_000)
    } else {
        0
    }
}

pub fn format_badge(tier: &str, remaining_ms: u64) -> String {
    if tier == "cold" || remaining_ms == 0 {
        return "❄️ cold".to_owned();
    }
    let total_secs = remaining_ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    let icon = if tier == "expiring" { "⏳" } else { "🔥" };
    if mins >= 60 {
        let hrs = mins / 60;
        let rem_m = mins % 60;
        format!("{icon} ~{hrs}h{rem_m:02}m (est.)")
    } else {
        format!("{icon} ~{mins}m{secs:02}s (est.)")
    }
}

/// Records a completed turn's cache warmth watermark into SQLite `cache_warmth`.
pub fn record_turn_warmth(
    socket: &Path,
    run_id: &str,
    pane_id: &str,
    provider: &str,
    est_context_tokens: u64,
    cfg: &GuardConfig,
) -> Option<CacheWarmthSnapshot> {
    if !cfg.cache_warmth.enabled {
        return None;
    }
    let at_ms = now_ms();
    let ttl_secs = resolve_ttl_secs(provider, &cfg.cache_ttl);
    let mut store = Store::open(socket).ok()?;
    let prev_tokens = store
        .get_cache_warmth(run_id)
        .ok()
        .flatten()
        .map_or(0, |r| r.est_context_tokens);
    let rec = CacheWarmthRecord {
        run_id: run_id.to_owned(),
        pane_id: pane_id.to_owned(),
        provider: provider.to_owned(),
        ttl_secs,
        last_turn_at_ms: at_ms,
        est_context_tokens: est_context_tokens.max(prev_tokens),
        updated_ms: at_ms,
    };
    let _ = store.upsert_cache_warmth(&rec);
    Some(CacheWarmthSnapshot::from_record(&rec, at_ms))
}
